use crate::{
    Fees, NearStakingPool, NearStakingPoolAccount, OfflineReason, StakeAccountBalances,
    StakeAccountData, StakeActionCallbacks, StakedBalance, StakingPool, StakingPoolBalances,
    StakingPoolOperator, StakingPoolOperatorCommand, Status, Treasury,
    ERR_STAKED_BALANCE_TOO_LOW_TO_UNSTAKE, ERR_STAKE_ACTION_FAILED, LOG_EVENT_EARNINGS,
    LOG_EVENT_LIQUIDITY, LOG_EVENT_NOT_ENOUGH_TO_STAKE, LOG_EVENT_STAKE, LOG_EVENT_STATUS_OFFLINE,
    LOG_EVENT_STATUS_ONLINE, LOG_EVENT_TREASURY_DEPOSIT, LOG_EVENT_TREASURY_DIVIDEND,
    LOG_EVENT_UNSTAKE, MAX_FEE, PERMISSION_TREASURER,
};
use oysterpack_smart_account_management::{
    components::account_management::AccountManagementComponent, AccountDataObject, AccountMetrics,
    AccountRepository, Permission, PermissionsManagement, StorageManagement,
    ERR_ACCOUNT_NOT_REGISTERED, ERR_NOT_AUTHORIZED,
};
use oysterpack_smart_contract::{
    components::contract_ownership::ContractOwnershipComponent, BalanceId, ContractNearBalances,
    ContractOwnership,
};
use oysterpack_smart_fungible_token::{
    components::fungible_token::FungibleTokenComponent, FungibleToken, Memo, TokenAmount,
    TokenService, TransferCallMessage, TransferReceiver,
};
use oysterpack_smart_near::domain::TGas;
use oysterpack_smart_near::{
    asserts::{ERR_ILLEGAL_STATE, ERR_INSUFFICIENT_FUNDS, ERR_INVALID, ERR_NEAR_DEPOSIT_REQUIRED},
    component::{Component, ComponentState, Deploy},
    data::numbers::U256,
    domain::{
        ActionType, BasisPoints, ByteLen, Gas, PublicKey, SenderIsReceiver, TransactionResource,
        YoctoNear,
    },
    json_function_callback,
    near_sdk::{
        borsh::{self, BorshDeserialize, BorshSerialize},
        env, is_promise_success,
        json_types::ValidAccountId,
        serde::{Deserialize, Serialize},
        AccountId, Promise, PromiseOrValue,
    },
    to_valid_account_id, TERA, YOCTO,
};
use std::cmp::min;

pub type AccountManager = AccountManagementComponent<StakeAccountData>;
pub type StakeFungibleToken = FungibleTokenComponent<StakeAccountData>;

/// Staking Pool Component
///
/// ## Deployment
/// - permissions: [`crate::PERMISSION_TREASURER`];
pub struct StakingPoolComponent {
    account_manager: AccountManager,
    stake_token: StakeFungibleToken,
}

impl StakingPoolComponent {
    pub fn new(
        account_manager: AccountManagementComponent<StakeAccountData>,
        stake: StakeFungibleToken,
    ) -> Self {
        Self {
            account_manager,
            stake_token: stake,
        }
    }
}

impl Component for StakingPoolComponent {
    type State = State;
    const STATE_KEY: u128 = 1954854625400732566949949714395710108;
}

#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Clone, Copy, PartialEq, Debug,
)]
#[serde(crate = "oysterpack_smart_near::near_sdk::serde")]
pub struct State {
    /// validator public key used for staking
    pub stake_public_key: PublicKey,
    /// fee charged when staking funds
    pub staking_fee: BasisPoints,
    /// fee charged based on earnings
    pub earnings_fee: BasisPoints,

    pub status: Status,

    /// used to check if staking rewards were earned
    pub last_contract_managed_total_balance: YoctoNear,

    /// NEAR deposited into the treasury is staked and stake earnings are used to boost the
    /// staking pool yield
    /// - this is used to track the treasury STAKE NEAR value
    /// - if it changes before staking fees are deposited, then it means the treasury received
    ///   staking rewards. The staking rewards will be burned before depositing the staking fee
    pub treasury_balance: YoctoNear,
}

impl State {
    pub const TOTAL_STAKED_BALANCE: BalanceId = BalanceId(1956973021105502521442959170292258855);

    /// unstaked funds are locked for 4 epochs
    /// - we need to track unstaked funds that is owned by accounts separate from the account NEAR balances
    /// - accounts will need to withdraw unstaked balances against this balance in combination with
    /// [`Self::UNSTAKED_LIQUIDITY_POOL`]
    pub const TOTAL_UNSTAKED_BALANCE: BalanceId = BalanceId(1955705469859818043123742456310621056);
    /// provides liquidity for withdrawing unstaked funds that are still locked
    /// - liquidity is added automatically when funds are staked and there are unstaked funds pending
    ///   withdrawal
    /// - when liquidity is added, funds are debited from [`Self::TOTAL_UNSTAKED_BALANCE`] and credited
    ///   to this balance
    pub const UNSTAKED_LIQUIDITY_POOL: BalanceId = BalanceId(1955784487678443851622222785149485288);

    /// returns the total balance that is currently managed by the contract for staking
    /// - this is used to compute staking rewards that are earned - since this balance is completely
    ///  managed by the contract, then if the balance increases, then we know rewards have been earned
    /// - account storage balances are excluded from the equation
    fn contract_managed_total_balance() -> YoctoNear {
        let total_contract_balance: YoctoNear =
            (env::account_balance() + env::account_locked_balance() - env::attached_deposit())
                .into();
        total_contract_balance - AccountMetrics::load().total_near_balance
    }

    /// excludes
    /// attached deposit because this should only be called in view mode on the contract
    /// - `env::attached_deposit` is illegal to call in view mode
    pub(crate) fn contract_managed_total_balance_in_view_mode() -> YoctoNear {
        let total_contract_balance: YoctoNear =
            (env::account_balance() + env::account_locked_balance()).into();
        total_contract_balance - AccountMetrics::load().total_near_balance
    }

    /// returns any earnings that have been received since the last time we checked - but excludes
    /// attached deposit because this should only be called in view mode on the contract
    /// - `env::attached_deposit` is illegal to call in view mode
    fn check_for_earnings_in_view_mode(&self) -> YoctoNear {
        State::contract_managed_total_balance_in_view_mode()
            .saturating_sub(*self.last_contract_managed_total_balance)
            .into()
    }

    pub(crate) fn total_staked_balance() -> YoctoNear {
        ContractNearBalances::near_balance(Self::TOTAL_STAKED_BALANCE)
    }

    fn incr_total_staked_balance(amount: YoctoNear) {
        ContractNearBalances::incr_balance(Self::TOTAL_STAKED_BALANCE, amount);
    }

    fn decr_total_staked_balance(amount: YoctoNear) {
        ContractNearBalances::decr_balance(Self::TOTAL_STAKED_BALANCE, amount);
    }

    pub(crate) fn total_unstaked_balance() -> YoctoNear {
        ContractNearBalances::near_balance(Self::TOTAL_UNSTAKED_BALANCE)
    }

    fn incr_total_unstaked_balance(amount: YoctoNear) {
        ContractNearBalances::incr_balance(Self::TOTAL_UNSTAKED_BALANCE, amount);
    }

    /// first tries to apply the debit against [`Self::UNSTAKED_LIQUIDITY_POOL`] and then against
    /// [`Self::TOTAL_UNSTAKED_BALANCE`]
    fn decr_total_unstaked_balance(mut amount: YoctoNear) {
        let liquidity = Self::liquidity();
        if liquidity > YoctoNear::ZERO {
            if amount <= liquidity {
                let total_liquidity =
                    ContractNearBalances::decr_balance(Self::UNSTAKED_LIQUIDITY_POOL, amount);
                LOG_EVENT_LIQUIDITY
                    .log(format!("removed={}, total={}", liquidity, total_liquidity));
                return;
            }
            amount -= liquidity;
            ContractNearBalances::clear_balance(Self::UNSTAKED_LIQUIDITY_POOL);
            LOG_EVENT_LIQUIDITY.log(format!("removed={}, total=0", liquidity));
            if amount > YoctoNear::ZERO {
                ContractNearBalances::decr_balance(Self::TOTAL_UNSTAKED_BALANCE, amount);
            }
        } else {
            ContractNearBalances::decr_balance(Self::TOTAL_UNSTAKED_BALANCE, amount);
        }
    }

    /// If there are unstaked funds awaiting withdrawal, then transfer the specified amount to the
    /// liquidity pool
    fn add_liquidity(amount: YoctoNear) {
        if amount == YoctoNear::ZERO {
            return;
        }

        let total_unstaked_balance =
            ContractNearBalances::near_balance(Self::TOTAL_UNSTAKED_BALANCE);
        if total_unstaked_balance == YoctoNear::ZERO {
            return;
        }

        let liquidity = min(amount, total_unstaked_balance);
        ContractNearBalances::decr_balance(Self::TOTAL_UNSTAKED_BALANCE, liquidity);
        let total_liquidity =
            ContractNearBalances::incr_balance(Self::UNSTAKED_LIQUIDITY_POOL, liquidity);
        LOG_EVENT_LIQUIDITY.log(format!("added={}, total={}", liquidity, total_liquidity));
    }

    pub(crate) fn liquidity() -> YoctoNear {
        ContractNearBalances::near_balance(Self::UNSTAKED_LIQUIDITY_POOL)
    }
}

impl Deploy for StakingPoolComponent {
    type Config = StakingPoolComponentConfig;

    /// default settings:
    /// - staking fee = 80 BPS (0.8%)
    /// - the staking pool is deployed as stopped, i.e., in order to start staking, the pool will
    ///   need to be explicitly started after deployment
    fn deploy(config: Self::Config) {
        // the contract account serves as the treasury account
        // we need to register an account with storage management in order to deposit STAKE into the
        // treasury
        let treasury = env::current_account_id();
        AccountManager::register_account_if_not_exists(&treasury);

        let state = State {
            stake_public_key: config.stake_public_key,
            status: Status::Offline(OfflineReason::Stopped),
            staking_fee: config.staking_fee.unwrap_or(80.into()),
            earnings_fee: config.earnings_fee.unwrap_or(0.into()),
            treasury_balance: YoctoNear::ZERO,
            last_contract_managed_total_balance: State::contract_managed_total_balance(),
        };
        let state = Self::new_state(state);
        state.save();
    }
}

#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq,
)]
#[serde(crate = "oysterpack_smart_near::near_sdk::serde")]
pub struct StakingPoolComponentConfig {
    pub stake_public_key: PublicKey,
    pub staking_fee: Option<BasisPoints>,
    pub earnings_fee: Option<BasisPoints>,
}

impl StakingPool for StakingPoolComponent {
    fn ops_stake_balance(&self, account_id: ValidAccountId) -> Option<StakeAccountBalances> {
        self.account_manager
            .storage_balance_of(account_id.clone())
            .map(|storage_balance| {
                let staked = {
                    let token_balance = self.stake_token.ft_balance_of(account_id.clone());
                    if token_balance == TokenAmount::ZERO {
                        None
                    } else {
                        Some(StakedBalance {
                            stake: token_balance,
                            near_value: self.ops_stake_token_value(Some(token_balance)),
                        })
                    }
                };

                let unstaked = self
                    .account_manager
                    .load_account_data(account_id.as_ref())
                    .map(|data| {
                        if data.unstaked_balances.total() == YoctoNear::ZERO {
                            None
                        } else {
                            Some(data.unstaked_balances.into())
                        }
                    })
                    .flatten();

                StakeAccountBalances {
                    storage_balance,
                    staked,
                    unstaked,
                }
            })
    }

    fn ops_stake(&mut self) -> PromiseOrValue<StakeAccountBalances> {
        let account_id = env::predecessor_account_id();
        let mut account = self
            .account_manager
            .registered_account_near_data(&account_id);

        self.state_with_updated_earnings();

        // stake the account's total available storage balance + attached deposit
        let (near_amount, stake_token_amount) = {
            let account_storage_available_balance = account
                .storage_balance(self.account_manager.storage_balance_bounds().min)
                .available;
            account.decr_near_balance(account_storage_available_balance);

            let near = account_storage_available_balance + env::attached_deposit();
            ERR_NEAR_DEPOSIT_REQUIRED.assert_with_message(
                || near > YoctoNear::ZERO,
                || "deposit NEAR into storage balance or attach NEAR deposit",
            );
            let (stake, remainder) = self.near_to_stake(near);
            account.incr_near_balance(remainder);
            account.save();

            (near - remainder, stake)
        };

        if near_amount == YoctoNear::ZERO {
            // INVARIANT CHECK: if `near_amount` is zero, then `stake_token_amount` should be zero
            assert_eq!(stake_token_amount, TokenAmount::ZERO);
            // NOTE: any attached deposit be deposited into the account's storage balance - this, there
            // is no need to panic
            LOG_EVENT_NOT_ENOUGH_TO_STAKE.log("");
            return self.registered_stake_account_balance(&account_id);
        }

        State::add_liquidity(near_amount);
        self.stake(&account_id, near_amount, stake_token_amount)
    }

    fn ops_unstake(&mut self, amount: Option<YoctoNear>) -> PromiseOrValue<StakeAccountBalances> {
        let account_id = env::predecessor_account_id();
        ERR_ACCOUNT_NOT_REGISTERED.assert(|| self.account_manager.account_exists(&account_id));

        let state = self.state_with_updated_earnings();

        let stake_balance = self
            .stake_token
            .ft_balance_of(to_valid_account_id(&account_id));
        if stake_balance == TokenAmount::ZERO {
            if amount.is_none() {
                return self.registered_stake_account_balance(&account_id);
            }
            ERR_INSUFFICIENT_FUNDS.panic_with_message("STAKE balance is zero");
            unreachable!()
        }
        let stake_near_value = self.stake_near_value_rounded_down(stake_balance);
        let (near_amount, stake_token_amount) = match amount {
            None => (stake_near_value, stake_balance), // unstake all
            Some(near_amount) => {
                ERR_INSUFFICIENT_FUNDS.assert(|| stake_near_value >= near_amount);
                // we round up the number of STAKE tokens to ensure that we never overdraw from the
                // staked balance - this is more than compensated for by transaction fee earnings
                let stake_token_amount = self.near_stake_value_rounded_up(near_amount);
                ERR_STAKED_BALANCE_TOO_LOW_TO_UNSTAKE
                    .assert(|| stake_balance >= stake_token_amount);
                (near_amount, stake_token_amount)
            }
        };

        LOG_EVENT_UNSTAKE.log(format!(
            "near_amount={}, stake_token_amount={}",
            near_amount, stake_token_amount
        ));

        State::decr_total_staked_balance(near_amount);
        State::incr_total_unstaked_balance(near_amount);
        self.stake_token.ft_burn(&account_id, stake_token_amount);
        let burned_near_value = self.ops_stake_token_value(Some(stake_token_amount));
        let rounding_diff = burned_near_value.saturating_sub(*near_amount);
        self.credit_account_unstaked_balance(&account_id, near_amount + rounding_diff);

        match state.status {
            Status::Online => {
                let promise = Self::create_stake_workflow(state.stake_public_key, &account_id);
                PromiseOrValue::Promise(promise)
            }
            Status::Offline(_) => {
                LOG_EVENT_STATUS_OFFLINE.log("");
                self.registered_stake_account_balance(&account_id)
            }
        }
    }

    fn ops_restake(&mut self, amount: Option<YoctoNear>) -> PromiseOrValue<StakeAccountBalances> {
        let account_id = env::predecessor_account_id();
        ERR_ACCOUNT_NOT_REGISTERED.assert(|| self.account_manager.account_exists(&account_id));

        self.state_with_updated_earnings();

        match self.account_manager.load_account_data(&account_id) {
            // account has no unstaked funds to restake
            None => match amount {
                None => self.registered_stake_account_balance(&account_id),
                Some(_) => {
                    ERR_INSUFFICIENT_FUNDS.panic();
                    unreachable!()
                }
            },
            Some(mut account) => {
                let (near_amount, stake_token_amount) = {
                    let near = amount.unwrap_or_else(|| account.unstaked_balances.total());
                    let (stake, remainder) = self.near_to_stake(near);
                    let stake_near_value = near - remainder;
                    account
                        .unstaked_balances
                        .debit_for_restaking(stake_near_value);
                    account.save();
                    State::decr_total_unstaked_balance(stake_near_value);
                    (stake_near_value, stake)
                };
                // NOTE: restaking does not add liquidity because no new funds are being deposited
                self.stake(&account_id, near_amount, stake_token_amount)
            }
        }
    }

    fn ops_stake_withdraw(&mut self, amount: Option<YoctoNear>) -> StakeAccountBalances {
        let account_id = env::predecessor_account_id();
        ERR_ACCOUNT_NOT_REGISTERED.assert(|| self.account_manager.account_exists(&account_id));

        fn debit_available_balance(
            mut unstaked_balances: AccountDataObject<StakeAccountData>,
            amount: YoctoNear,
        ) {
            unstaked_balances
                .unstaked_balances
                .debit_available_balance(amount);
            if unstaked_balances.unstaked_balances.total() == YoctoNear::ZERO {
                unstaked_balances.delete();
            } else {
                unstaked_balances.save();
            }
            State::decr_total_unstaked_balance(amount);
            // debit contract managed total balance
            {
                let mut state = StakingPoolComponent::state();
                state.last_contract_managed_total_balance -= amount;
                state.save();
            }

            Promise::new(env::predecessor_account_id()).transfer(*amount);
        }

        // earnings are updated to ensure updated balances are returned
        self.state_with_updated_earnings();

        match amount {
            // withdraw all available
            None => {
                if let Some(mut account_staked_data) =
                    self.account_manager.load_account_data(&account_id)
                {
                    account_staked_data.unstaked_balances.apply_liquidity();
                    let amount = account_staked_data.unstaked_balances.available();
                    if amount > YoctoNear::ZERO {
                        debit_available_balance(account_staked_data, amount);
                    }
                }
            }
            // withdraw specified amount
            Some(amount) => {
                ERR_INVALID.assert(|| amount > YoctoNear::ZERO, || "amount must be > 0");
                match self.account_manager.load_account_data(&account_id) {
                    Some(mut unstaked_balances) => {
                        unstaked_balances.unstaked_balances.apply_liquidity();
                        debit_available_balance(unstaked_balances, amount);
                    }
                    None => ERR_INSUFFICIENT_FUNDS.panic(),
                }
            }
        }

        self.ops_stake_balance(to_valid_account_id(&account_id))
            .unwrap()
    }

    fn ops_stake_transfer(
        &mut self,
        receiver_id: ValidAccountId,
        amount: YoctoNear,
        memo: Option<Memo>,
    ) -> TokenAmount {
        self.state_with_updated_earnings();
        let stake_value = self.near_stake_value_rounded_up(amount);
        self.stake_token.ft_transfer(receiver_id, stake_value, memo);
        stake_value
    }

    fn ops_stake_transfer_call(
        &mut self,
        receiver_id: ValidAccountId,
        amount: YoctoNear,
        memo: Option<Memo>,
        msg: TransferCallMessage,
    ) -> Promise {
        self.state_with_updated_earnings();
        let stake_value = self.near_stake_value_rounded_up(amount);
        self.stake_token
            .ft_transfer_call(receiver_id, stake_value, memo, msg)
    }

    fn ops_stake_token_value(&self, amount: Option<TokenAmount>) -> YoctoNear {
        self.compute_stake_near_value_rounded_down(
            amount.unwrap_or(YOCTO.into()),
            State::total_staked_balance() + Self::state().check_for_earnings_in_view_mode(),
        )
    }

    fn ops_stake_token_value_with_earnings(&mut self, amount: Option<TokenAmount>) -> YoctoNear {
        self.state_with_updated_earnings();
        self.stake_near_value_rounded_down(amount.unwrap_or(YOCTO.into()))
    }

    fn ops_stake_status(&self) -> Status {
        Self::state().status
    }

    fn ops_stake_pool_balances(&self) -> StakingPoolBalances {
        StakingPoolBalances::new(
            *StakingPoolComponent::state(),
            self.stake_token.ft_total_supply(),
        )
    }

    fn ops_stake_fees(&self) -> Fees {
        let state = Self::state();
        Fees {
            staking_fee: state.staking_fee,
            earnings_fee: state.earnings_fee,
        }
    }

    fn ops_stake_public_key(&self) -> PublicKey {
        Self::state().stake_public_key
    }
}

impl NearStakingPool for StakingPoolComponent {
    fn get_account_staked_balance(&self, account_id: ValidAccountId) -> YoctoNear {
        self.ops_stake_balance(account_id)
            .map_or(YoctoNear::ZERO, |balance| {
                balance
                    .staked
                    .map_or(YoctoNear::ZERO, |balance| balance.near_value)
            })
    }

    fn get_account_unstaked_balance(&self, account_id: ValidAccountId) -> YoctoNear {
        self.ops_stake_balance(account_id)
            .map_or(YoctoNear::ZERO, |balance| {
                let unstaked = balance
                    .unstaked
                    .map_or(YoctoNear::ZERO, |balance| balance.total);
                unstaked + balance.storage_balance.available
            })
    }

    fn is_account_unstaked_balance_available(&self, account_id: ValidAccountId) -> bool {
        self.ops_stake_balance(account_id).map_or(true, |balance| {
            balance
                .unstaked
                .map_or(true, |balance| balance.total == balance.available)
        })
    }

    fn get_account_total_balance(&self, account_id: ValidAccountId) -> YoctoNear {
        self.ops_stake_balance(account_id)
            .map_or(YoctoNear::ZERO, |balance| {
                let staked = balance
                    .staked
                    .map_or(YoctoNear::ZERO, |balance| balance.near_value);
                let unstaked = balance
                    .unstaked
                    .map_or(YoctoNear::ZERO, |balance| balance.total);
                staked + unstaked + balance.storage_balance.available
            })
    }

    fn get_account(&self, account_id: ValidAccountId) -> NearStakingPoolAccount {
        self.ops_stake_balance(account_id.clone()).map_or(
            NearStakingPoolAccount {
                account_id: account_id.as_ref().to_string(),
                unstaked_balance: YoctoNear::ZERO,
                staked_balance: YoctoNear::ZERO,
                can_withdraw: true,
            },
            |balance| NearStakingPoolAccount {
                account_id: account_id.as_ref().to_string(),
                unstaked_balance: balance
                    .unstaked
                    .as_ref()
                    .map_or(YoctoNear::ZERO, |balance| balance.total)
                    + balance.storage_balance.available,
                staked_balance: balance
                    .staked
                    .map_or(YoctoNear::ZERO, |balance| balance.near_value),
                can_withdraw: balance
                    .unstaked
                    .map_or(true, |balance| balance.total == balance.available),
            },
        )
    }

    fn deposit(&mut self) {
        self.account_manager.storage_deposit(None, None);
    }

    fn deposit_and_stake(&mut self) {
        self.ops_stake();
    }

    fn withdraw(&mut self, amount: YoctoNear) {
        self.ops_stake_withdraw(Some(amount));
    }

    fn withdraw_all(&mut self) {
        self.ops_stake_withdraw(None);
    }

    fn stake(&mut self, amount: YoctoNear) {
        ERR_INVALID.assert(|| amount > YoctoNear::ZERO, || "amount must be > 0");
        let (mut near_account, account_data) = self
            .account_manager
            .registered_account(&env::predecessor_account_id());
        match account_data {
            Some(mut data) => {
                if data.unstaked_balances.total() < amount {
                    ERR_INSUFFICIENT_FUNDS.assert(|| {
                        data.unstaked_balances.total()
                            + near_account
                                .storage_balance(self.account_manager.storage_balance_bounds().min)
                                .available
                            >= amount
                    });

                    let storage_balance = amount - data.unstaked_balances.total();
                    near_account.decr_near_balance(storage_balance);
                    near_account.save();

                    data.unstaked_balances.credit_unstaked(storage_balance);
                    data.save();
                    State::incr_total_unstaked_balance(storage_balance);
                }
            }
            None => {
                ERR_INSUFFICIENT_FUNDS.assert(|| {
                    near_account
                        .storage_balance(self.account_manager.storage_balance_bounds().min)
                        .available
                        >= amount
                });

                near_account.decr_near_balance(amount);
                near_account.save();

                self.credit_account_unstaked_balance(&env::predecessor_account_id(), amount);
                State::incr_total_unstaked_balance(amount);
            }
        }
        self.ops_restake(Some(amount));
    }

    fn unstake(&mut self, amount: YoctoNear) {
        self.ops_unstake(Some(amount));
    }

    fn unstake_all(&mut self) {
        self.ops_unstake(None);
    }
}

impl StakingPoolOperator for StakingPoolComponent {
    fn ops_stake_operator_command(&mut self, command: StakingPoolOperatorCommand) {
        self.account_manager.assert_operator();

        match command {
            StakingPoolOperatorCommand::StopStaking => Self::stop_staking(OfflineReason::Stopped),
            StakingPoolOperatorCommand::StartStaking => self.start_staking(),
            StakingPoolOperatorCommand::UpdatePublicKey(public_key) => {
                Self::update_public_key(public_key)
            }
            StakingPoolOperatorCommand::UpdateFees(fees) => Self::update_staking_fees(fees),
        }
    }
}

impl StakingPoolComponent {
    /// we always try to stop, even if the pool is already offline
    /// - for example, if the staking public key is invalid, then the stake action would fail
    fn stop_staking(reason: OfflineReason) {
        if let OfflineReason::StakeActionFailed = reason {
            ERR_STAKE_ACTION_FAILED.log("");
        }

        // update status to offline
        let mut state = Self::state();
        if state.status.is_online() {
            state.status = Status::Offline(reason);
            state.save();
            LOG_EVENT_STATUS_OFFLINE.log(reason);
        } else {
            LOG_EVENT_STATUS_OFFLINE.log(format!("{} - but was already offline", reason));
        }

        // unstake all
        if env::account_locked_balance() > 0 {
            Promise::new(env::current_account_id())
                .stake(0, state.stake_public_key.into())
                .then(json_function_callback(
                    "ops_stake_stop_finalize",
                    Option::<()>::None,
                    YoctoNear::ZERO,
                    Self::compute_callback_gas(
                        Gas(5 * TERA),
                        Self::staking_workflow_receipts_gas(),
                    ),
                ));
        }
    }

    fn start_staking(&mut self) {
        let mut state = self.state_with_updated_earnings();
        if let Status::Offline(_) = state.status {
            // update status
            state.status = Status::Online;
            state.save();

            LOG_EVENT_STATUS_ONLINE.log("");

            // stake
            let total_staked_balance = State::total_staked_balance();
            if total_staked_balance > YoctoNear::ZERO {
                Promise::new(env::current_account_id())
                    .stake(*total_staked_balance, state.stake_public_key.into())
                    .then(json_function_callback(
                        "ops_stake_start_finalize",
                        Option::<()>::None,
                        YoctoNear::ZERO,
                        Self::callback_gas_with_check_for_enough_gas(),
                    ));
            }
        } else {
            LOG_EVENT_STATUS_ONLINE.log("already online");
        }
    }

    fn update_public_key(public_key: PublicKey) {
        let mut state = Self::state();
        ERR_ILLEGAL_STATE.assert(
            || !state.status.is_online(),
            || "staking pool must be offline to update the staking public key",
        );
        state.stake_public_key = public_key;
        state.save();
    }

    fn update_staking_fees(fees: Fees) {
        ERR_INVALID.assert(
            || fees.staking_fee <= MAX_FEE,
            || "max staking fee is 1000 BPS (10%)",
        );
        ERR_INVALID.assert(
            || fees.earnings_fee <= MAX_FEE,
            || "max earnings fee is 1000 BPS (10%)",
        );
        ERR_INVALID.assert(
            || *fees.staking_fee > 0 || *fees.earnings_fee > 0,
            || "min fee is 1 BPS (0.01%) for at least 1 fee type",
        );
        let mut state = Self::state();
        state.staking_fee = fees.staking_fee;
        state.earnings_fee = fees.earnings_fee;
        state.save();
    }
}

impl StakeActionCallbacks for StakingPoolComponent {
    fn ops_stake_finalize(&mut self, account_id: AccountId) -> StakeAccountBalances {
        // we get the balance here first because if the stake action fails, then we want to minimize
        // the amount of work done after the promise workflow is created to stop staking because
        // the gas supplied to the callback takes the rest of the gas minus 5 TGas to compete this call
        let balance = self
            .ops_stake_balance(to_valid_account_id(&account_id))
            .unwrap();

        if Self::state().status.is_online() && !is_promise_success() {
            Self::stop_staking(OfflineReason::StakeActionFailed);
        }

        balance
    }

    fn ops_stake_start_finalize(&mut self) {
        if is_promise_success() {
            LOG_EVENT_STATUS_ONLINE.log("staked");
        } else {
            Self::stop_staking(OfflineReason::StakeActionFailed);
        }
    }

    fn ops_stake_stop_finalize(&mut self) {
        if is_promise_success() {
            LOG_EVENT_STATUS_OFFLINE.log("all NEAR has been unstaked");
        } else {
            ERR_STAKE_ACTION_FAILED.log("failed to unstake when trying to stop staking pool");
        }
    }
}

impl Treasury for StakingPoolComponent {
    fn ops_stake_treasury_deposit(&mut self) -> PromiseOrValue<StakeAccountBalances> {
        let deposit = YoctoNear::from(env::attached_deposit());
        ERR_NEAR_DEPOSIT_REQUIRED.assert(|| deposit > YoctoNear::ZERO);

        let mut state = self.state_with_updated_earnings();
        let stake = self.near_stake_value_rounded_down(deposit);
        state.treasury_balance += deposit;
        state.save();

        State::add_liquidity(deposit);
        self.stake(&env::current_account_id(), deposit, stake)
    }

    fn ops_stake_treasury_distribution(&mut self) {
        let deposit = YoctoNear::from(env::attached_deposit());
        let total_staked_balance_before_earnings = State::total_staked_balance();
        let state = self.state_with_updated_earnings();
        if deposit > YoctoNear::ZERO {
            State::add_liquidity(deposit);
            self.stake(&env::current_account_id(), deposit, TokenAmount::ZERO);
        } else if state.status.is_online() {
            if State::total_staked_balance() > total_staked_balance_before_earnings {
                Self::create_stake_workflow(state.stake_public_key, &env::current_account_id());
            }
        }
    }

    fn ops_stake_treasury_transfer_to_owner(&mut self, amount: Option<YoctoNear>) {
        let owner_account_id = ContractOwnershipComponent.ops_owner();
        ERR_NOT_AUTHORIZED.assert(|| {
            let account_id = env::predecessor_account_id();
            if owner_account_id == account_id {
                return true;
            }
            let account = self
                .account_manager
                .registered_account_near_data(&account_id);
            account.contains_permissions(self.treasurer_permission().into())
        });

        AccountManager::register_account_if_not_exists(&owner_account_id);

        let mut state = self.state_with_updated_earnings();

        let treasury_account = env::current_account_id();
        let (amount, stake) = {
            let treasury_balance = self
                .stake_token
                .ft_balance_of(to_valid_account_id(&treasury_account));
            if treasury_balance == TokenAmount::ZERO {
                return;
            }

            let treasury_near_balance = self.stake_near_value_rounded_down(treasury_balance);
            let amount = match amount {
                None => treasury_near_balance,
                Some(amount) => {
                    ERR_INSUFFICIENT_FUNDS.assert(|| treasury_near_balance >= amount);
                    amount
                }
            };
            let stake = self.near_stake_value_rounded_up(amount);
            (amount, min(treasury_balance, stake))
        };

        // transfer STAKE from treasury to owner account
        {
            self.stake_token.ft_burn(&treasury_account, stake);
            self.stake_token.ft_mint(&owner_account_id, stake);
        }

        // debit from the treasury balance
        {
            state.treasury_balance -= amount;
            state.save();
        }
    }

    fn ops_stake_grant_treasurer(&mut self, account_id: ValidAccountId) {
        self.account_manager
            .ops_permissions_grant(account_id, self.treasurer_permission().into());
    }

    fn ops_stake_revoke_treasurer(&mut self, account_id: ValidAccountId) {
        self.account_manager
            .ops_permissions_revoke(account_id, self.treasurer_permission().into());
    }

    fn ops_stake_is_treasurer(&mut self, account_id: ValidAccountId) -> bool {
        self.account_manager
            .load_account_near_data(account_id.as_ref())
            .map_or(false, |account| {
                account.contains_permissions(self.treasurer_permission().into())
            })
    }
}

impl TransferReceiver for StakingPoolComponent {
    /// updates the treasury balance
    ///
    /// ## Panics
    /// If not called as self, i.e., by the STAKE pool contract
    fn ft_on_transfer(
        &mut self,
        _sender_id: ValidAccountId,
        _amount: TokenAmount,
        _msg: TransferCallMessage,
    ) -> PromiseOrValue<TokenAmount> {
        ERR_NOT_AUTHORIZED.assert_with_message(
            || env::predecessor_account_id() == env::current_account_id(),
            || "this method can only be invoked by the STAKE pool contract internally",
        );
        let mut state = self.state_with_updated_earnings();
        let treasury_stake_balance = self
            .stake_token
            .ft_balance_of(to_valid_account_id(&env::current_account_id()));
        state.treasury_balance = self.stake_near_value_rounded_down(treasury_stake_balance);
        state.save();
        LOG_EVENT_TREASURY_DEPOSIT.log(format!("treasury balance = {}", state.treasury_balance));
        PromiseOrValue::Value(TokenAmount::ZERO)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(crate = "oysterpack_smart_near::near_sdk::serde")]
struct StakeActionCallbackArgs {
    account_id: AccountId,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(crate = "oysterpack_smart_near::near_sdk::serde")]
struct ResumeFinalizeCallbackArgs {
    pub total_staked_balance: YoctoNear,
}

// staking related methods
impl StakingPoolComponent {
    /// Stakes the NEAR and mints the corresponding STAKE for the account
    ///
    /// ## Args
    /// - `near_amount` - new funds that are being staked
    /// - `stake_token_amount` - the amount of tokens that will be minted
    fn stake(
        &mut self,
        account_id: &str,
        near_amount: YoctoNear,
        stake_token_amount: TokenAmount,
    ) -> PromiseOrValue<StakeAccountBalances> {
        if near_amount == YoctoNear::ZERO {
            // INVARIANT CHECK: if `near_amount` is zero, then `stake_token_amount` should be zero
            assert_eq!(stake_token_amount, TokenAmount::ZERO);
            // NOTE: any attached deposit be deposited into the account's storage balance - this, there
            // is no need to panic
            LOG_EVENT_NOT_ENOUGH_TO_STAKE.log("");
            return self.registered_stake_account_balance(account_id);
        }

        LOG_EVENT_STAKE.log(format!(
            "near_amount={}, stake_token_amount={}",
            near_amount, stake_token_amount
        ));

        let state =
            self.process_stake_transaction_finances(account_id, near_amount, stake_token_amount);

        match state.status {
            Status::Online => PromiseOrValue::Promise(Self::create_stake_workflow(
                state.stake_public_key,
                account_id,
            )),
            Status::Offline(_) => {
                LOG_EVENT_STATUS_OFFLINE.log("");
                self.registered_stake_account_balance(account_id)
            }
        }
    }
}

impl StakingPoolComponent {
    /// invokes `Self::compute_callback_gas`, but also checks that enough gas is attached for the
    /// callback
    fn callback_gas_with_check_for_enough_gas() -> Gas {
        let staking_workflow_receipts_gas = Self::staking_workflow_receipts_gas();
        let gas: Gas = Self::compute_callback_gas(Gas(5 * TERA), staking_workflow_receipts_gas);

        // make sure there is enough gas for the callback
        // - because of NEAR's async nature, if there is not enough gas, then this transaction will
        //   commit its state, but the callback will fail - thus its better to fail fast and let
        //   the user know to retry with more gas
        const CALLBACK_COMPUTE_GAS: TGas = TGas(10);
        let min_callback_gas = staking_workflow_receipts_gas + CALLBACK_COMPUTE_GAS;
        ERR_INVALID.assert(
            || gas >= min_callback_gas,
            || {
                let min_required_gas = env::used_gas()
                    + *staking_workflow_receipts_gas
                    + *TGas(5) // gas required to complete this call
                    + *min_callback_gas;
                format!(
                    "not enough gas was attached - min required gas is {} TGas",
                    min_required_gas / TERA + 1 // round up 1 TGas
                )
            },
        );

        gas
    }

    /// computes how much gas will be required to complete this call given the remaining compute
    /// and receipts this call will create
    ///
    /// ## Args
    /// `gas_for_remaining_compute` - how much compute gas is needed for the remaining work in this call
    /// `gas_for_receipts` - how much gas is required to create any remaining receipts
    fn compute_callback_gas(gas_for_remaining_compute: Gas, gas_for_receipts: Gas) -> Gas {
        (env::prepaid_gas() - env::used_gas() - *gas_for_receipts - *gas_for_remaining_compute)
            .into()
    }

    fn staking_workflow_receipts_gas() -> Gas {
        const RECEIPT: TransactionResource =
            TransactionResource::ActionReceipt(SenderIsReceiver(true));
        const STAKE_ACTION: TransactionResource =
            TransactionResource::Action(ActionType::Stake(SenderIsReceiver(true)));
        const FUNC_CALL: TransactionResource = TransactionResource::Action(
            ActionType::FunctionCall(SenderIsReceiver(true), ByteLen(512)),
        );
        const DATA_RECEIPT: TransactionResource =
            TransactionResource::DataReceipt(SenderIsReceiver(true), ByteLen(200));
        Gas::compute(vec![
            (RECEIPT, 2), // stake action + callback
            (STAKE_ACTION, 1),
            (FUNC_CALL, 1),
            (DATA_RECEIPT, 1),
        ])
    }

    fn treasurer_permission(&self) -> Permission {
        self.account_manager
            .permission_by_name(PERMISSION_TREASURER)
            .unwrap()
    }

    fn treasury_stake_balance(&self) -> (TokenAmount, YoctoNear) {
        let treasury_stake_balance = self
            .stake_token
            .ft_balance_of(to_valid_account_id(&env::current_account_id()));
        let treasury_near_value = self.stake_near_value_rounded_down(treasury_stake_balance);
        (treasury_stake_balance, treasury_near_value)
    }

    /// - credit total staked balance and contract managed total balance with the staked amount
    /// - mints STAKE on the account for amount staked
    /// - collects staking fee
    ///
    /// Returns the updated state after saving it to storage.
    fn process_stake_transaction_finances(
        &mut self,
        account_id: &str,
        amount: YoctoNear,
        stake_token_amount: TokenAmount,
    ) -> ComponentState<State> {
        let mut state = Self::state();
        State::incr_total_staked_balance(amount);
        state.last_contract_managed_total_balance += amount;

        // stake_token_amount will be ZERO if this is a funds distribution
        // - see [`Treasury::ops_stake_treasury_distribution`]
        if stake_token_amount > TokenAmount::ZERO {
            self.stake_token.ft_mint(&account_id, stake_token_amount);
        }

        // collect staking fee - treasury and owner accounts do not get charged staking fees
        let owner_id = ContractOwnershipComponent.ops_owner();
        if stake_token_amount > TokenAmount::ZERO
            && state.staking_fee > BasisPoints::ZERO
            && account_id != &env::current_account_id()
            && account_id != &owner_id
        {
            let staking_fee = self.near_stake_value_rounded_down(amount * state.staking_fee);
            if staking_fee > TokenAmount::ZERO {
                self.stake_token.ft_burn(&account_id, staking_fee);
                self.stake_token.ft_mint(&owner_id, staking_fee);
            }
        }

        state.save();
        state
    }

    fn registered_stake_account_balance(
        &self,
        account_id: &str,
    ) -> PromiseOrValue<StakeAccountBalances> {
        PromiseOrValue::Value(
            self.ops_stake_balance(to_valid_account_id(account_id))
                .unwrap(),
        )
    }

    pub(crate) fn state() -> ComponentState<State> {
        Self::load_state().expect("component has not been deployed")
    }

    pub(crate) fn state_with_updated_earnings(&mut self) -> ComponentState<State> {
        /// returns the current treasury balance after paying the dividend - which means the treasury
        /// NEAR value still increases overtime because after paying the dividend, STAKE value goes up
        /// and the new treasury balance is based on the new STAKE value
        fn pay_treasury_dividend(
            this: &mut StakingPoolComponent,
            treasury_balance: YoctoNear,
        ) -> YoctoNear {
            let (treasury_stake_balance, current_treasury_near_value) =
                this.treasury_stake_balance();
            if treasury_balance == YoctoNear::ZERO {
                // then this seeds the treasury balance
                return current_treasury_near_value;
            }

            let treasury_staking_earnings = current_treasury_near_value
                .saturating_sub(*treasury_balance)
                .into();
            let treasury_staking_earnings_stake_value =
                this.near_stake_value_rounded_down(treasury_staking_earnings);
            if treasury_staking_earnings_stake_value == TokenAmount::ZERO {
                return current_treasury_near_value;
            }

            this.stake_token.ft_burn(
                &env::current_account_id(),
                treasury_staking_earnings_stake_value,
            );
            LOG_EVENT_TREASURY_DIVIDEND.log(format!(
                "{} yoctoNEAR / {} yoctoSTAKE",
                treasury_staking_earnings, treasury_staking_earnings_stake_value
            ));
            this.stake_near_value_rounded_down(
                treasury_stake_balance - treasury_staking_earnings_stake_value,
            )
        }

        let mut state = Self::state();

        // If there are no stakers,i.e., STAKE total supply is zero, then earnings will not be
        // staked in this staking transaction - earnings will be staked in the next transaction.
        // - the reason we do this is because when computing token values, a zero token supply
        //   effectively resets the token value 1:1 for STAKE:NEAR
        if self.stake_token.ft_total_supply() == TokenAmount::ZERO {
            return state;
        }

        let contract_managed_total_balance = State::contract_managed_total_balance();
        let earnings: YoctoNear = contract_managed_total_balance
            .saturating_sub(*state.last_contract_managed_total_balance)
            .into();
        let owner_earnings = if earnings > YoctoNear::ZERO {
            LOG_EVENT_EARNINGS.log(earnings);

            if state.earnings_fee > BasisPoints::ZERO {
                let owner_earnings = state.earnings_fee * earnings;
                // distributes earnings minus owner earnings
                State::incr_total_staked_balance(earnings - owner_earnings);
                owner_earnings
            } else {
                State::incr_total_staked_balance(earnings);
                YoctoNear::ZERO
            }
        } else {
            YoctoNear::ZERO
        };

        state.last_contract_managed_total_balance = contract_managed_total_balance;
        state.treasury_balance = pay_treasury_dividend(self, state.treasury_balance);

        // mint owner earnings only after rest of earnings are first distributed
        if owner_earnings > YoctoNear::ZERO {
            // NOTE: because of rounding down, there might be some earnings that can't be converted
            // into STAKE, which will end up being distributed into the pool
            let owner_stake_earnings = self.near_stake_value_rounded_down(owner_earnings);
            if owner_stake_earnings > TokenAmount::ZERO {
                let owner_id = ContractOwnershipComponent.ops_owner();
                self.stake_token.ft_mint(&owner_id, owner_stake_earnings);
            }
            State::incr_total_staked_balance(owner_earnings);
        }

        state.save();
        state
    }

    fn credit_account_unstaked_balance(&self, account_id: &str, amount: YoctoNear) {
        let mut account = self.account_manager.registered_account_data(&account_id);
        account.unstaked_balances.credit_unstaked(amount);
        account.save();
    }

    fn create_stake_workflow(stake_public_key: PublicKey, account_id: &str) -> Promise {
        let stake = Promise::new(env::current_account_id())
            .stake(*State::total_staked_balance(), stake_public_key.into());
        let finalize = json_function_callback(
            "ops_stake_finalize",
            Some(StakeActionCallbackArgs {
                account_id: account_id.to_string(),
            }),
            YoctoNear::ZERO,
            Self::callback_gas_with_check_for_enough_gas(),
        );
        stake.then(finalize)
    }

    fn stake_near_value_rounded_down(&self, stake: TokenAmount) -> YoctoNear {
        self.compute_stake_near_value_rounded_down(stake, State::total_staked_balance())
    }

    fn compute_stake_near_value_rounded_down(
        &self,
        stake: TokenAmount,
        total_staked_near_balance: YoctoNear,
    ) -> YoctoNear {
        if *stake == 0 {
            return YoctoNear::ZERO;
        }

        let ft_total_supply = *self.stake_token.ft_total_supply();
        if ft_total_supply == 0 {
            return (*stake).into();
        }

        (U256::from(*total_staked_near_balance) * U256::from(*stake) / U256::from(ft_total_supply))
            .as_u128()
            .into()
    }

    /// converts the specified NEAR amount to STAKE and returns the STAKE equivalent and any NEAR
    /// remainder that could not be converted into STAKE because of rounding
    ///
    /// ## Notes
    /// because of rounding down we need to convert the STAKE value back to NEAR, which ensures that
    /// the account will not be short changed when they unstake
    fn near_to_stake(&self, amount: YoctoNear) -> (TokenAmount, YoctoNear) {
        let stake = self.near_stake_value_rounded_down(amount);
        let stake_near_value = self.stake_near_value_rounded_down(stake);
        (stake, amount - stake_near_value)
    }

    fn near_stake_value_rounded_down(&self, amount: YoctoNear) -> TokenAmount {
        if amount == YoctoNear::ZERO {
            return TokenAmount::ZERO;
        }

        let ft_total_supply = *self.stake_token.ft_total_supply();
        if ft_total_supply == 0 {
            return (*amount).into();
        }

        let total_staked_balance = *State::total_staked_balance();

        (U256::from(ft_total_supply) * U256::from(*amount) / U256::from(total_staked_balance))
            .as_u128()
            .into()
    }

    fn near_stake_value_rounded_up(&self, amount: YoctoNear) -> TokenAmount {
        if amount == YoctoNear::ZERO {
            return TokenAmount::ZERO;
        }

        let ft_total_supply = *self.stake_token.ft_total_supply();
        if ft_total_supply == 0 {
            return amount.value().into();
        }

        let total_staked_balance = *State::total_staked_balance();

        ((U256::from(ft_total_supply) * U256::from(*amount) + U256::from(total_staked_balance - 1))
            / U256::from(total_staked_balance))
        .as_u128()
        .into()
    }
}

#[cfg(test)]
mod tests_staking_pool {
    use super::*;
    use crate::*;
    use oysterpack_smart_account_management::{
        components::account_management::{
            AccountManagementComponent, AccountManagementComponentConfig,
        },
        ContractPermissions,
    };
    use oysterpack_smart_contract::{
        components::contract_operator::ContractOperatorComponent, ContractOperator,
    };
    use oysterpack_smart_fungible_token::components::fungible_token::FungibleTokenConfig;
    use oysterpack_smart_fungible_token::{
        components::fungible_token::FungibleTokenComponent, FungibleToken, Metadata, Name, Spec,
        Symbol, FT_METADATA_SPEC,
    };
    use oysterpack_smart_near::{
        component::*,
        near_sdk::{env, serde_json, test_utils},
        *,
    };
    use oysterpack_smart_near_test::*;
    use std::collections::HashMap;
    use std::convert::*;

    pub type AccountManager = AccountManagementComponent<StakeAccountData>;

    pub type StakeFungibleToken = FungibleTokenComponent<StakeAccountData>;

    const OWNER: &str = "owner";
    const ACCOUNT: &str = "bob";

    pub fn deploy_stake_contract(stake_public_key: PublicKey) {
        let owner = to_valid_account_id(&env::predecessor_account_id());
        ContractOwnershipComponent::deploy(owner.clone());

        AccountManager::deploy(AccountManagementComponentConfig {
            storage_usage_bounds: None,
            admin_account: owner.clone(),
            component_account_storage_mins: Some(vec![StakeFungibleToken::account_storage_min]),
        });

        // transfer any contract balance to the owner - minus the contract operational balance
        {
            contract_operator().ops_operator_lock_storage_balance(10000.into());
            let account_manager = account_manager();
            let mut owner_account = account_manager.registered_account_near_data(owner.as_ref());
            owner_account
                .incr_near_balance(ContractOwnershipComponent.ops_owner_balance().available);
            owner_account.save();
        }

        StakeFungibleToken::deploy(FungibleTokenConfig {
            metadata: Metadata {
                spec: Spec(FT_METADATA_SPEC.to_string()),
                name: Name("STAKE".to_string()),
                symbol: Symbol("STAKE".to_string()),
                decimals: 24,
                icon: None,
                reference: None,
                reference_hash: None,
            },
            token_supply: 0,
        });

        StakingPoolComponent::deploy(StakingPoolComponentConfig {
            stake_public_key,
            staking_fee: None,
            earnings_fee: None,
        });

        println!("STAKE contract has been deployed");
    }

    fn account_manager() -> AccountManager {
        StakeFungibleToken::register_storage_management_event_handler();

        let contract_permissions = {
            let mut permissions = HashMap::with_capacity(1);
            permissions.insert(0, PERMISSION_TREASURER);
            ContractPermissions(permissions)
        };

        AccountManager::new(contract_permissions)
    }

    fn ft_stake() -> StakeFungibleToken {
        StakeFungibleToken::new(account_manager())
    }

    fn contract_operator() -> ContractOperatorComponent<StakeAccountData> {
        ContractOperatorComponent::new(account_manager())
    }

    fn staking_pool() -> StakingPoolComponent {
        StakingPoolComponent::new(account_manager(), ft_stake())
    }

    fn staking_public_key() -> PublicKey {
        serde_json::from_str("\"ed25519:GTi3gtSio5ZYYKTT8WVovqJEob6KqdmkTi8KqGSfwqdm\"").unwrap()
    }

    fn log_contract_managed_total_balance(msg: &str) {
        let total_contract_balance: YoctoNear =
            (env::account_balance() + env::account_locked_balance() - env::attached_deposit())
                .into();
        println!(
            r#"### contract_managed_total_balance - {}
env::account_balance()                          {}
env::account_locked_balance()                   {}
env::attached_deposit()                         {}
AccountMetrics::load().total_near_balance       {}      
--------------------------------------------------
contract_managed_total_balance                  {}
last_contract_managed_total_balance             {}
**************************************************"#,
            msg,
            env::account_balance(),
            env::account_locked_balance(),
            env::attached_deposit(),
            AccountMetrics::load().total_near_balance,
            total_contract_balance - AccountMetrics::load().total_near_balance,
            StakingPoolComponent::state().last_contract_managed_total_balance,
        );
    }

    #[cfg(test)]
    mod tests_offline {
        use super::*;

        #[cfg(test)]
        mod tests_stake {
            use super::*;

            #[test]
            fn earnings_received_through_account_balance_increase_with_zero_total_stake_supply() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                let ft_stake = ft_stake();

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // simulate earnings received by increasing account balance
                const EARNINGS: YoctoNear = YoctoNear(YOCTO);
                ctx.account_balance = env::account_balance() + *EARNINGS;
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());

                // even there are earnings that have accumulated, because there are no stakers, then
                // we expect the STAKE token value to be 1:1
                assert_eq!(staking_pool.ops_stake_token_value(None), YOCTO.into());
                // Act
                let balances = if let PromiseOrValue::Value(balances) = staking_pool.ops_stake() {
                    balances
                } else {
                    panic!("expected value")
                };
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [STAKE] near_amount=1000000000000000000000000, stake_token_amount=1000000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: bob, amount: 1000000000000000000000000",
                    "[INFO] [FT_BURN] account: bob, amount: 8000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: owner, amount: 8000000000000000000000",
                    "[WARN] [STATUS_OFFLINE] ",
                ]);
                println!("{}", serde_json::to_string_pretty(&balances).unwrap());
                assert_eq!(
                    *balances.staked.as_ref().unwrap().stake,
                    992000000000000000000000
                );
                // when FT total supply is zero, then earnings are held and not staked
                // earnings will be staked on the next staking action
                assert_eq!(
                    *balances.staked.as_ref().unwrap().near_value,
                    1984000000000000000000000
                );
                // STAKE token value is 2 NEAR because of the earnings that have not yet been staked
                assert_eq!(staking_pool.ops_stake_token_value(None), (2 * YOCTO).into());
                assert_eq!(ft_stake.ft_total_supply(), YOCTO.into());
                println!(
                    "owner stake balances = {}",
                    serde_json::to_string_pretty(
                        &staking_pool.ops_stake_balance(to_valid_account_id(OWNER))
                    )
                    .unwrap()
                );
                println!(
                    "{}",
                    serde_json::to_string_pretty(&staking_pool.ops_stake_pool_balances()).unwrap()
                );

                ctx.account_balance = env::account_balance() + *EARNINGS;
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());
                assert_eq!(staking_pool.ops_stake_token_value(None), (3 * YOCTO).into());

                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                // Act - simulate more earnings on next stake
                let balances = if let PromiseOrValue::Value(balances) = staking_pool.ops_stake() {
                    balances
                } else {
                    panic!("expected Value")
                };

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [EARNINGS] 2000000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Deposit(YoctoNear(1))",
                    "[INFO] [STAKE] near_amount=999999999999999999999999, stake_token_amount=333333333333333333333333",
                    "[INFO] [FT_MINT] account: bob, amount: 333333333333333333333333",
                    "[INFO] [FT_BURN] account: bob, amount: 2666666666666666666666",
                    "[INFO] [FT_MINT] account: owner, amount: 2666666666666666666666",
                    "[WARN] [STATUS_OFFLINE] ",
                ]);

                {
                    println!(
                        "account {}",
                        serde_json::to_string_pretty(&balances).unwrap()
                    );

                    assert_eq!(
                        *balances.staked.as_ref().unwrap().stake,
                        1322666666666666666666667
                    );
                    assert_eq!(
                        *balances.staked.as_ref().unwrap().near_value,
                        3968000000000000000000001
                    );
                }

                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());

                // Assert - account STAKE NEAR balances sum up to the total staked balance
                {
                    let owner_stake_balances = staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .unwrap();
                    println!(
                        "owner_stake_balances {}",
                        serde_json::to_string_pretty(&owner_stake_balances).unwrap()
                    );
                    let stake_pool_balances = staking_pool.ops_stake_pool_balances();
                    println!(
                        "stake_pool_balances {}",
                        serde_json::to_string_pretty(&stake_pool_balances).unwrap()
                    );
                    assert_eq!(
                        stake_pool_balances.total_staked,
                        balances.staked.as_ref().unwrap().near_value
                            + owner_stake_balances.staked.as_ref().unwrap().near_value
                    );
                }

                {
                    let stake_token_value = staking_pool.ops_stake_token_value(None);
                    println!("stake_token_value = {}", stake_token_value);
                    let stake_total_supply = ft_stake.ft_total_supply();
                    println!("ft_total_supply = {}", stake_total_supply);
                    let owner_stake_balance = staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .unwrap();
                    println!(
                        "owner stake balances = {}",
                        serde_json::to_string_pretty(&owner_stake_balance).unwrap()
                    );
                    let stake_pool_balances = staking_pool.ops_stake_pool_balances();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&stake_pool_balances).unwrap()
                    );
                    // since all earnings are staked, then the total STAKE supply value should match
                    // the total staked balance
                    assert_eq!(
                        staking_pool.ops_stake_token_value(Some(stake_total_supply)),
                        stake_pool_balances.total_staked,
                    );
                }

                {
                    ctx.account_balance = env::account_balance();
                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = 0;
                    ctx.is_view = true;
                    testing_env!(ctx.clone());
                    assert_eq!(staking_pool.ops_stake_token_value(None), (3 * YOCTO).into());
                }
            }

            #[test]
            fn with_attached_deposit() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let contract_managed_total_balance = State::contract_managed_total_balance();

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                let ft_stake = ft_stake();

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // assert that there was no net change to contract_managed_total_balance
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                assert_eq!(
                    contract_managed_total_balance,
                    State::contract_managed_total_balance()
                );

                assert_eq!(
                    account_manager
                        .storage_balance_of(to_valid_account_id(ACCOUNT))
                        .unwrap()
                        .available,
                    YoctoNear::ZERO
                );

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(balances) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);

                    println!("{}", serde_json::to_string_pretty(&balances).unwrap());
                    let staking_fee = staking_pool.ops_stake_fees().staking_fee
                        * staking_pool.ops_stake_token_value(None);
                    assert!(balances.unstaked.is_none());
                    match balances.staked.as_ref() {
                        Some(stake) => {
                            assert_eq!(stake.stake, (YOCTO - *staking_fee).into());
                            assert_eq!(stake.near_value, (YOCTO - *staking_fee).into());
                        }
                        None => panic!("expected staked balance"),
                    }

                    assert_eq!(
                        balances,
                        staking_pool
                            .ops_stake_balance(to_valid_account_id(ACCOUNT))
                            .unwrap()
                    );

                    // Assert
                    assert_eq!(logs, vec![
                        "[INFO] [STAKE] near_amount=1000000000000000000000000, stake_token_amount=1000000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                        "[INFO] [FT_MINT] account: bob, amount: 1000000000000000000000000",
                        "[INFO] [FT_BURN] account: bob, amount: 8000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                        "[INFO] [FT_MINT] account: owner, amount: 8000000000000000000000",
                        "[WARN] [STATUS_OFFLINE] ",
                    ]);
                    let staking_fee = staking_pool.ops_stake_fees().staking_fee * YOCTO;
                    assert_eq!(staking_pool.ops_stake_token_value(None), YOCTO.into());
                    assert_eq!(
                        ft_stake.ft_balance_of(to_valid_account_id(ACCOUNT)),
                        (YOCTO - *staking_fee).into()
                    );
                    assert_eq!(
                        ft_stake.ft_balance_of(to_valid_account_id(OWNER)),
                        (*staking_fee).into()
                    );
                    let state = StakingPoolComponent::state();
                    println!("{:#?}", *state);
                    assert_eq!(State::total_staked_balance(), YOCTO.into());
                    assert_eq!(state.treasury_balance, YoctoNear::ZERO);

                    log_contract_managed_total_balance("after staking");

                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = 0;
                    testing_env!(ctx.clone());
                    assert_eq!(
                        contract_managed_total_balance + YOCTO,
                        State::contract_managed_total_balance()
                    );
                    let stake_pool_balances = staking_pool.ops_stake_pool_balances();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&stake_pool_balances).unwrap()
                    );
                    assert_eq!(stake_pool_balances.treasury_balance, YoctoNear::ZERO);
                    assert_eq!(stake_pool_balances.total_staked, YOCTO.into());
                    assert_eq!(stake_pool_balances.total_unstaked, YoctoNear::ZERO);
                    assert_eq!(stake_pool_balances.unstaked_liquidity, YoctoNear::ZERO);
                } else {
                    panic!("expected value")
                }
            }

            #[test]
            fn with_zero_attached_deposit_and_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let contract_managed_total_balance = State::contract_managed_total_balance();

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                let ft_stake = ft_stake();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // assert that there was no net change to contract_managed_total_balance
                ctx.account_balance = env::account_balance() + YOCTO;
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                assert_eq!(
                    contract_managed_total_balance,
                    State::contract_managed_total_balance()
                );

                assert_eq!(
                    account_manager
                        .storage_balance_of(to_valid_account_id(ACCOUNT))
                        .unwrap()
                        .available,
                    YOCTO.into()
                );

                log_contract_managed_total_balance("before staking");
                assert_eq!(staking_pool.ops_stake_token_value(None), YOCTO.into());

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(balances) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);

                    println!("{}", serde_json::to_string_pretty(&balances).unwrap());
                    let staking_fee = staking_pool.ops_stake_fees().staking_fee * YOCTO;
                    assert!(balances.unstaked.is_none());
                    match balances.staked.as_ref() {
                        Some(stake) => {
                            assert_eq!(stake.stake, (YOCTO - *staking_fee).into());
                            assert_eq!(stake.near_value, (YOCTO - *staking_fee).into());
                        }
                        None => panic!("expected staked balance"),
                    }

                    assert_eq!(
                        balances,
                        staking_pool
                            .ops_stake_balance(to_valid_account_id(ACCOUNT))
                            .unwrap()
                    );

                    // Assert
                    assert_eq!(logs, vec![
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] Withdrawal(YoctoNear(1000000000000000000000000))",
                        "[INFO] [STAKE] near_amount=1000000000000000000000000, stake_token_amount=1000000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                        "[INFO] [FT_MINT] account: bob, amount: 1000000000000000000000000",
                        "[INFO] [FT_BURN] account: bob, amount: 8000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                        "[INFO] [FT_MINT] account: owner, amount: 8000000000000000000000",
                        "[WARN] [STATUS_OFFLINE] ",
                    ]);
                    let staking_fee = staking_pool.ops_stake_fees().staking_fee * YOCTO;
                    assert_eq!(staking_pool.ops_stake_token_value(None), YOCTO.into());
                    assert_eq!(
                        ft_stake.ft_balance_of(to_valid_account_id(ACCOUNT)),
                        (YOCTO - *staking_fee).into()
                    );
                    assert_eq!(
                        ft_stake.ft_balance_of(to_valid_account_id(OWNER)),
                        (*staking_fee).into()
                    );
                    let state = StakingPoolComponent::state();
                    println!("{:#?}", *state);
                    assert_eq!(State::total_staked_balance(), YOCTO.into());
                    assert_eq!(state.treasury_balance, YoctoNear::ZERO);

                    log_contract_managed_total_balance("after staking");

                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = 0;
                    testing_env!(ctx.clone());
                    assert_eq!(
                        contract_managed_total_balance + YOCTO,
                        State::contract_managed_total_balance()
                    );
                    let stake_pool_balances = staking_pool.ops_stake_pool_balances();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&stake_pool_balances).unwrap()
                    );
                    assert_eq!(stake_pool_balances.treasury_balance, YoctoNear::ZERO);
                    assert_eq!(stake_pool_balances.total_staked, YOCTO.into());
                    assert_eq!(stake_pool_balances.total_unstaked, YoctoNear::ZERO);
                    assert_eq!(stake_pool_balances.unstaked_liquidity, YoctoNear::ZERO);
                } else {
                    panic!("expected value")
                }
            }

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn with_unregistered_account() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                ctx.account_balance = env::account_balance();
                testing_env!(ctx);
                staking_pool.ops_stake();
            }

            #[test]
            #[should_panic(
                expected = "[ERR] [NEAR_DEPOSIT_REQUIRED] deposit NEAR into storage balance or attach NEAR deposit"
            )]
            fn registered_account_with_zero_storage_available_balance_and_zero_attached_deposit() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // Act
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx);
                staking_pool.ops_stake();
            }

            #[test]
            fn stake_amount_too_low_too_stake() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() + (2 * YOCTO);
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());

                assert_eq!(staking_pool.ops_stake_token_value(None), (3 * YOCTO).into());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());
                assert_eq!(staking_pool.ops_stake_token_value(None), (3 * YOCTO).into());
                assert_eq!(
                    *staking_pool.ops_stake_pool_balances().total_staked,
                    (5 * YOCTO) - 2
                );
                println!(
                    "ops_stake_token_value = {}",
                    staking_pool.ops_stake_token_value(None)
                );
                assert_eq!(
                    staking_pool
                        .ops_stake_balance(to_valid_account_id(ACCOUNT))
                        .unwrap()
                        .storage_balance
                        .available,
                    2.into()
                );

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(balances) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                    assert_eq!(
                        logs,
                        vec![
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] Withdrawal(YoctoNear(2))",
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] Deposit(YoctoNear(2))",
                            "[INFO] [NOT_ENOUGH_TO_STAKE] ",
                        ]
                    );

                    assert_eq!(balances.storage_balance.available, 2.into());
                } else {
                    panic!("expected value")
                }
            }

            #[test]
            fn stake_when_liquidity_needed() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 10 * YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());

                let total_staked_balance = staking_pool.ops_stake_pool_balances().total_staked;
                assert_eq!(total_staked_balance, (10 * YOCTO).into());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() - *total_staked_balance;
                ctx.account_locked_balance = *total_staked_balance;
                ctx.attached_deposit = 0;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                staking_pool.ops_unstake(None);
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                println!(
                    "{}",
                    serde_json::to_string_pretty(&staking_pool.ops_stake_pool_balances()).unwrap()
                );

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = 5 * YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [LIQUIDITY] added=5000000000000000000000000, total=5000000000000000000000000",
                    "[INFO] [STAKE] near_amount=5000000000000000000000000, stake_token_amount=5000000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: bob, amount: 5000000000000000000000000",
                    "[INFO] [FT_BURN] account: bob, amount: 40000000000000000000000",
                    "[INFO] [FT_MINT] account: owner, amount: 40000000000000000000000",
                    "[WARN] [STATUS_OFFLINE] ",
                ]);

                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                assert_eq!(pool_balances.unstaked_liquidity, (5 * YOCTO).into());
            }

            #[test]
            fn stake_with_treasury_balance() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // stake 10 NEAR
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 10 * YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());
                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                assert_eq!(
                    pool_balances,
                    serde_json::from_str(
                        r#"{
  "total_staked": "10000000000000000000000000",
  "total_stake_supply": "10000000000000000000000000",
  "total_unstaked": "0",
  "unstaked_liquidity": "0",
  "treasury_balance": "0",
  "current_contract_managed_total_balance": "13172980000000000000000000",
  "last_contract_managed_total_balance": "13172980000000000000000000",
  "earnings": "0"
}"#
                    )
                    .unwrap()
                );

                // transfer STAKE from the owner to the treasurys
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 1;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                let owner_balance = staking_pool
                    .ops_stake_balance(to_valid_account_id(OWNER))
                    .unwrap();
                staking_pool.ops_stake_transfer_call(
                    to_valid_account_id(&env::current_account_id()),
                    owner_balance.staked.as_ref().unwrap().near_value,
                    None,
                    "".into(),
                );

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());
                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                // the staking pool will not become aware of the new STAKE that the owner transferred
                // over until a stake transaction is processed - thus the current treasury balance
                // should still be zero
                assert_eq!(pool_balances.treasury_balance, YoctoNear::ZERO);
                // confirms that the STAKE was transferred from the owner to the treasury STAKE account
                assert_eq!(
                    staking_pool
                        .ops_stake_balance(to_valid_account_id(&env::current_account_id()))
                        .unwrap()
                        .staked
                        .unwrap()
                        .stake,
                    owner_balance.staked.as_ref().unwrap().stake
                );

                // stake
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = YOCTO;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    // 1 yoctoNEAR was earned from the 1 yoctoNEAR attached deposit from the FT transfer
                    "[INFO] [EARNINGS] 1",
                    // the STAKE NEAR value has increased but because of rounding, 1 yoctoNEAR could
                    // be staked and is deposited into the storage balance
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Deposit(YoctoNear(1))",                    
                    "[INFO] [STAKE] near_amount=999999999999999999999999, stake_token_amount=999999999999999999999999",
                    "[INFO] [FT_MINT] account: bob, amount: 999999999999999999999999",
                    "[INFO] [FT_BURN] account: bob, amount: 7999999999999999999998",
                    "[INFO] [FT_MINT] account: owner, amount: 7999999999999999999998",
                    "[WARN] [STATUS_OFFLINE] ",
                ]);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());
                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                // funds that were transferred over from the owner FT account now are picked up
                // by the treasury
                assert_eq!(
                    pool_balances.treasury_balance,
                    owner_balance.staked.as_ref().unwrap().near_value
                );

                println!(
                    "stake_token_value = {}",
                    staking_pool.ops_stake_token_value(None)
                );
                // the 1 yoctoNEAR that was earned was too low to affect the STAKE NEAR value because
                // the returned value is rounded down
                assert_eq!(staking_pool.ops_stake_token_value(None), YOCTO.into());

                // Act - with no new earnings
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    // the 1 NEAR + 1 yoctoNEAR from the account's storage available balance was staked
                    // but because the STAKE NEAR value is slightly higher than 1, 1 yoctoNEAR could
                    // not be staked because of rounding and is returned back to the account storage balance
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Withdrawal(YoctoNear(1))",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Deposit(YoctoNear(1))",
                    "[INFO] [STAKE] near_amount=1000000000000000000000000, stake_token_amount=1000000000000000000000000",
                    "[INFO] [FT_MINT] account: bob, amount: 1000000000000000000000000",
                    "[INFO] [FT_BURN] account: bob, amount: 7999999999999999999999",
                    "[INFO] [FT_MINT] account: owner, amount: 7999999999999999999999",
                    "[WARN] [STATUS_OFFLINE] ",
                ]);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());
                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());

                // Act - stake again with no new earning
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Withdrawal(YoctoNear(1))",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Deposit(YoctoNear(1))",
                    "[INFO] [STAKE] near_amount=1000000000000000000000000, stake_token_amount=1000000000000000000000000",
                    "[INFO] [FT_MINT] account: bob, amount: 1000000000000000000000000",
                    "[INFO] [FT_BURN] account: bob, amount: 7999999999999999999999",
                    "[INFO] [FT_MINT] account: owner, amount: 7999999999999999999999",
                    "[WARN] [STATUS_OFFLINE] ",
                ]);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());
                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                assert_eq!(
                    pool_balances,
                    serde_json::from_str(
                        r#"{
  "total_staked": "13000000000000000000000000",
  "total_stake_supply": "12999999999999999999999999",
  "total_unstaked": "0",
  "unstaked_liquidity": "0",
  "treasury_balance": "80000000000000000000000",
  "current_contract_managed_total_balance": "16172980000000000000000000",
  "last_contract_managed_total_balance": "16172980000000000000000000",
  "earnings": "0"
}"#
                    )
                    .unwrap()
                );

                let treasury_balance = staking_pool
                    .ops_stake_balance(to_valid_account_id(&env::current_account_id()))
                    .unwrap();
                println!(
                    "treasury balance: {}",
                    serde_json::to_string_pretty(&treasury_balance).unwrap()
                );
                assert_eq!(
                    *treasury_balance.staked.as_ref().unwrap().stake,
                    *(staking_pool.ops_stake_fees().staking_fee * (10 * YOCTO))
                );
                assert_eq!(
                    treasury_balance.staked.as_ref().unwrap().near_value,
                    staking_pool.ops_stake_fees().staking_fee * (10 * YOCTO)
                );

                // Act - stake again - with new earnings - 0.1 NEAR
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() + (YOCTO / 10);
                ctx.attached_deposit = YOCTO;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [EARNINGS] 100000000000000000000000",
                    "[INFO] [FT_BURN] account: contract.near, amount: 610687022900763358778",
                    "[INFO] [TREASURY_DIVIDEND] 615384615384615384615 yoctoNEAR / 610687022900763358778 yoctoSTAKE",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Withdrawal(YoctoNear(1))",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Deposit(YoctoNear(1))",
                    "[INFO] [STAKE] near_amount=1000000000000000000000000, stake_token_amount=992319794883748033331391",
                    "[INFO] [FT_MINT] account: bob, amount: 992319794883748033331391",
                    "[INFO] [FT_BURN] account: bob, amount: 7938558359069984266651",
                    "[INFO] [FT_MINT] account: owner, amount: 7938558359069984266651",
                    "[WARN] [STATUS_OFFLINE] ",
                ]);

                let pool_balances_after_dividend_payout = staking_pool.ops_stake_pool_balances();
                println!(
                    "pool_balances_after_dividend_payout {}",
                    serde_json::to_string_pretty(&pool_balances_after_dividend_payout).unwrap()
                );
                // treasury balance is slight increased because after the treasury dividend is burned
                // the STAKE value increases
                assert_eq!(
                    pool_balances_after_dividend_payout,
                    serde_json::from_str(
                        r#"{
  "total_staked": "14100000000000000000000000",
  "total_stake_supply": "13991709107860847269972612",
  "total_unstaked": "0",
  "unstaked_liquidity": "0",
  "treasury_balance": "80003758250534376247857",
  "current_contract_managed_total_balance": "17272980000000000000000000",
  "last_contract_managed_total_balance": "17272980000000000000000000",
  "earnings": "0"
}"#
                    )
                    .unwrap()
                );

                let treasury_balance_after_dividend_paid = staking_pool
                    .ops_stake_balance(to_valid_account_id(&env::current_account_id()))
                    .unwrap();
                println!(
                    "treasury balance: {}",
                    serde_json::to_string_pretty(&treasury_balance_after_dividend_paid).unwrap()
                );
                assert_eq!(
                    *treasury_balance_after_dividend_paid
                        .staked
                        .as_ref()
                        .unwrap()
                        .stake,
                    *treasury_balance.staked.as_ref().unwrap().stake - 610687022900763358778
                );
                assert_eq!(
                    treasury_balance_after_dividend_paid,
                    serde_json::from_str(
                        r#"{
  "storage_balance": {
    "total": "3930000000000000000000",
    "available": "0"
  },
  "staked": {
    "stake": "79389312977099236641222",
    "near_value": "80003758250534376247857"
  },
  "unstaked": null
}"#
                    )
                    .unwrap()
                );
            }

            #[test]
            fn as_owner() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let contract_managed_total_balance = State::contract_managed_total_balance();

                let mut staking_pool = staking_pool();

                let ft_stake = ft_stake();

                ctx.account_balance = env::account_balance();
                ctx.is_view = true;
                testing_env!(ctx.clone());
                let owner_balance = staking_pool
                    .ops_stake_balance(to_valid_account_id(OWNER))
                    .unwrap();

                // Act
                ctx.predecessor_account_id = OWNER.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Promise(_) = staking_pool.ops_stake() {
                    panic!("expected promise");
                }
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                // no staking fee should be charged to the owner
                assert_eq!(logs, vec![
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Withdrawal(YoctoNear(9996819160000000000000000000))",
                    "[INFO] [STAKE] near_amount=9997819160000000000000000000, stake_token_amount=9997819160000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: owner, amount: 9997819160000000000000000000",
                    "[WARN] [STATUS_OFFLINE] ",
                ]);

                let pool_balances = staking_pool.ops_stake_pool_balances();
                assert_eq!(
                    pool_balances.total_staked,
                    owner_balance.storage_balance.available + YOCTO
                );

                // Assert
                assert_eq!(staking_pool.ops_stake_token_value(None), YOCTO.into());
                assert_eq!(
                    *ft_stake.ft_balance_of(to_valid_account_id(OWNER)),
                    *(owner_balance.storage_balance.available + YOCTO)
                );
                let state = StakingPoolComponent::state();
                println!("{:#?}", *state);
                assert_eq!(state.treasury_balance, YoctoNear::ZERO);

                log_contract_managed_total_balance("after staking");

                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                assert_eq!(
                    contract_managed_total_balance
                        + owner_balance.storage_balance.available
                        + YOCTO,
                    State::contract_managed_total_balance()
                );
                let stake_pool_balances = staking_pool.ops_stake_pool_balances();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&stake_pool_balances).unwrap()
                );
                assert_eq!(stake_pool_balances.treasury_balance, YoctoNear::ZERO);
                assert_eq!(
                    stake_pool_balances.total_staked,
                    staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .as_ref()
                        .unwrap()
                        .staked
                        .as_ref()
                        .unwrap()
                        .near_value
                );
                assert_eq!(stake_pool_balances.total_unstaked, YoctoNear::ZERO);
                assert_eq!(stake_pool_balances.unstaked_liquidity, YoctoNear::ZERO);
            }
        }

        #[cfg(test)]
        mod tests_unstake {
            use super::*;

            #[test]
            fn unstake_partial() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(balances) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);

                    let pool_balances = staking_pool.ops_stake_pool_balances();
                    println!(
                        "pool_balances before unstaking: {}",
                        serde_json::to_string_pretty(&pool_balances).unwrap()
                    );

                    let staked_balance = balances.staked.as_ref().unwrap().near_value;

                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = 0;
                    testing_env!(ctx.clone());
                    if let PromiseOrValue::Value(balances_after_unstaking) =
                        staking_pool.ops_unstake(Some((*staked_balance / 4).into()))
                    {
                        let logs = test_utils::get_logs();
                        println!("{:#?}", logs);
                        assert_eq!(logs, vec![
                            "[INFO] [UNSTAKE] near_amount=248000000000000000000000, stake_token_amount=248000000000000000000000",
                            "[INFO] [FT_BURN] account: bob, amount: 248000000000000000000000",
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(184)",
                            "[WARN] [STATUS_OFFLINE] ",
                        ]);

                        println!(
                            "account balances_after_unstaking: {}",
                            serde_json::to_string_pretty(&balances_after_unstaking).unwrap()
                        );
                        assert_eq!(
                            balances_after_unstaking.unstaked.as_ref().unwrap().total,
                            staked_balance / 4
                        );
                        assert_eq!(
                            balances_after_unstaking
                                .unstaked
                                .as_ref()
                                .unwrap()
                                .available,
                            YoctoNear::ZERO
                        );

                        let pool_balances = staking_pool.ops_stake_pool_balances();
                        println!(
                            "pool_balances: {}",
                            serde_json::to_string_pretty(&pool_balances).unwrap()
                        );
                        assert_eq!(
                            pool_balances.total_staked,
                            balances_after_unstaking.staked.as_ref().unwrap().near_value
                                + staking_pool
                                    .ops_stake_balance(to_valid_account_id(OWNER))
                                    .unwrap()
                                    .staked
                                    .unwrap()
                                    .near_value
                        );
                        assert_eq!(
                            pool_balances.total_unstaked,
                            balances_after_unstaking.unstaked.as_ref().unwrap().total
                        );
                    } else {
                        panic!("expected value")
                    }
                } else {
                    panic!("expected value")
                }
            }

            #[test]
            fn unstake_all() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(balances) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);

                    let pool_balances = staking_pool.ops_stake_pool_balances();
                    println!(
                        "pool_balances before unstaking: {}",
                        serde_json::to_string_pretty(&pool_balances).unwrap()
                    );

                    let staked_balance = balances.staked.as_ref().unwrap().near_value;

                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = 0;
                    testing_env!(ctx.clone());
                    if let PromiseOrValue::Value(balances_after_unstaking) =
                        staking_pool.ops_unstake(None)
                    {
                        let logs = test_utils::get_logs();
                        println!("{:#?}", logs);
                        assert_eq!(logs, vec![
                            "[INFO] [UNSTAKE] near_amount=992000000000000000000000, stake_token_amount=992000000000000000000000",
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-104)",
                            "[INFO] [FT_BURN] account: bob, amount: 992000000000000000000000",
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(184)",
                            "[WARN] [STATUS_OFFLINE] ",
                        ]);

                        println!(
                            "account balances_after_unstaking: {}",
                            serde_json::to_string_pretty(&balances_after_unstaking).unwrap()
                        );
                        assert_eq!(
                            balances_after_unstaking.unstaked.as_ref().unwrap().total,
                            staked_balance
                        );
                        assert_eq!(
                            balances_after_unstaking
                                .unstaked
                                .as_ref()
                                .unwrap()
                                .available,
                            YoctoNear::ZERO
                        );

                        let pool_balances = staking_pool.ops_stake_pool_balances();
                        println!(
                            "pool_balances: {}",
                            serde_json::to_string_pretty(&pool_balances).unwrap()
                        );
                        assert_eq!(
                            pool_balances.total_staked,
                            staking_pool
                                .ops_stake_balance(to_valid_account_id(OWNER))
                                .unwrap()
                                .staked
                                .unwrap()
                                .near_value
                        );
                        assert_eq!(
                            pool_balances.total_unstaked,
                            balances_after_unstaking.unstaked.as_ref().unwrap().total
                        );
                    } else {
                        panic!("expected value")
                    }
                } else {
                    panic!("expected value")
                }
            }

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn account_not_registered() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx);
                staking_pool.ops_unstake(None);
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_FUNDS] STAKE balance is zero")]
            fn zero_stake_balance_and_unstake_specified_amount() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx);
                staking_pool.ops_unstake(Some(YOCTO.into()));
            }

            #[test]
            fn zero_stake_balance_and_unstake_all() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // Act
                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx);
                if let PromiseOrValue::Value(balances) = staking_pool.ops_unstake(None) {
                    assert!(balances.staked.is_none());
                } else {
                    panic!("expected value")
                }
            }

            #[test]
            fn unstake_with_earnings_received() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // stake
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                let staked_balance =
                    if let PromiseOrValue::Value(balance) = staking_pool.ops_stake() {
                        let staking_fee = staking_pool.ops_stake_fees().staking_fee * YOCTO;
                        assert_eq!(
                            balance.staked.as_ref().unwrap().near_value,
                            (YOCTO - *staking_fee).into()
                        );
                        balance
                    } else {
                        panic!("expected value")
                    };
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                // Act
                const EARNINGS: YoctoNear = YoctoNear(YOCTO);
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() + *EARNINGS;
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(balance) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                    assert_eq!(logs, vec![
                        "[INFO] [EARNINGS] 1000000000000000000000000",
                        "[INFO] [UNSTAKE] near_amount=1984000000000000000000000, stake_token_amount=992000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-104)",
                        "[INFO] [FT_BURN] account: bob, amount: 992000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(184)",
                        "[WARN] [STATUS_OFFLINE] ",
                    ]);

                    let unstaked_near_value = staking_pool
                        .ops_stake_token_value(Some(staked_balance.staked.as_ref().unwrap().stake));
                    assert_eq!(
                        balance.unstaked.as_ref().unwrap().total,
                        unstaked_near_value
                    );
                } else {
                    panic!("expected value")
                }
            }

            #[test]
            fn unstake_with_dividend_received() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // stake
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                let staked_balance =
                    if let PromiseOrValue::Value(balance) = staking_pool.ops_stake() {
                        let staking_fee = staking_pool.ops_stake_fees().staking_fee * YOCTO;
                        assert_eq!(
                            balance.staked.as_ref().unwrap().near_value,
                            (YOCTO - *staking_fee).into()
                        );
                        balance
                    } else {
                        panic!("expected value")
                    };
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                // deposit into treasury
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_treasury_deposit();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                // Act
                const EARNINGS: YoctoNear = YoctoNear(YOCTO);
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() + *EARNINGS;
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(balance) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                    assert_eq!(logs, vec![
                        "[INFO] [EARNINGS] 1000000000000000000000000",
                        "[INFO] [FT_BURN] account: contract.near, amount: 333333333333333333333333",
                        "[INFO] [TREASURY_DIVIDEND] 500000000000000000000000 yoctoNEAR / 333333333333333333333333 yoctoSTAKE",
                        "[INFO] [UNSTAKE] near_amount=1785599999999999999999999, stake_token_amount=992000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-104)",
                        "[INFO] [FT_BURN] account: bob, amount: 992000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(184)",
                        "[WARN] [STATUS_OFFLINE] ",
                    ]);

                    println!("{}", serde_json::to_string_pretty(&balance).unwrap());
                    let unstaked_near_value = staking_pool
                        .ops_stake_token_value(Some(staked_balance.staked.as_ref().unwrap().stake));
                    assert_eq!(
                        balance.unstaked.as_ref().unwrap().total
                            + balance.storage_balance.available,
                        unstaked_near_value
                    );
                } else {
                    panic!("expected value")
                }
            }
        }

        #[cfg(test)]
        mod tests_withdraw {
            use super::*;

            #[test]
            fn withdraw_some() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();
                let state_before_withdrawal = StakingPoolComponent::state();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(Some((1000).into()));

                // Assert
                assert!(test_utils::get_logs().is_empty());

                assert_eq!(
                    balances.unstaked.as_ref().unwrap().total,
                    balances_before_withdrawal.unstaked.as_ref().unwrap().total - 1000
                );

                let state = StakingPoolComponent::state();
                assert_eq!(
                    state.last_contract_managed_total_balance,
                    state_before_withdrawal.last_contract_managed_total_balance - 1000,
                );

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 1);
                match &receipts[0].actions[0] {
                    Action::Transfer(action) => {
                        assert_eq!(action.deposit, 1000);
                    }
                    _ => panic!("expected transfer action"),
                }
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_FUNDS]")]
            fn withdraw_some_with_zero_unstaked() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_withdraw(Some((1000).into()));
            }

            #[test]
            fn withdraw_available_using_liquidity() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                State::add_liquidity((YOCTO / 2).into());
                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(None);

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec!["[INFO] [LIQUIDITY] removed=500000000000000000000000, total=0",]
                );

                assert_eq!(
                    balances.unstaked.as_ref().unwrap().total,
                    balances_before_withdrawal.unstaked.as_ref().unwrap().total - (YOCTO / 2)
                );
                assert_eq!(
                    balances.unstaked.as_ref().unwrap().available,
                    YoctoNear::ZERO
                );

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 1);
                match &receipts[0].actions[0] {
                    Action::Transfer(action) => {
                        assert_eq!(action.deposit, (YOCTO / 2));
                    }
                    _ => panic!("expected transfer action"),
                }
            }

            #[test]
            fn withdraw_available_using_full_liquidity() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // staking adds liquidity
                ctx.predecessor_account_id = OWNER.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = *balances_before_withdrawal.unstaked.as_ref().unwrap().total;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                // Act
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(None);

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec![
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-184)",
                        "[INFO] [LIQUIDITY] removed=992000000000000000000000, total=0",
                    ]
                );

                assert!(balances.unstaked.is_none(),);

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 1);
                match &receipts[0].actions[0] {
                    Action::Transfer(action) => {
                        assert_eq!(action.deposit, 992000000000000000000000);
                    }
                    _ => panic!("expected transfer action"),
                }
            }

            #[test]
            fn withdraw_available_using_up_all_liquidity() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register accounts
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = "alice".to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // stake storage deposit
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // Add liquidity
                ctx.predecessor_account_id = "alice".to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO / 2;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                // Act
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(None);

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec![
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-184)",
                        "[INFO] [LIQUIDITY] removed=500000000000000000000000, total=0",
                    ]
                );

                assert!(balances.unstaked.is_none());

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 1);
                match &receipts[0].actions[0] {
                    Action::Transfer(action) => {
                        assert_eq!(action.deposit, 992000000000000000000000);
                    }
                    _ => panic!("expected transfer action"),
                }
            }

            #[test]
            fn withdraw_all() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(None);

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec!["[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-184)",]
                );

                assert!(balances.unstaked.is_none());

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 1);
                match &receipts[0].actions[0] {
                    Action::Transfer(action) => {
                        assert_eq!(
                            action.deposit,
                            *balances_before_withdrawal.unstaked.as_ref().unwrap().total
                        );
                    }
                    _ => panic!("expected transfer action"),
                }
            }

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED] ")]
            fn withdraw_all_not_registered() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                testing_env!(ctx);
                staking_pool.ops_stake_withdraw(None);
            }

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED] ")]
            fn withdraw_some_not_registered() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                testing_env!(ctx);
                staking_pool.ops_stake_withdraw(Some(YOCTO.into()));
            }

            #[test]
            fn withdraw_all_with_zero_unstaked_funds() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(None);

                // Assert
                assert!(test_utils::get_logs().is_empty());

                assert_eq!(balances_before_withdrawal, balances);
                assert!(deserialize_receipts().is_empty());
            }

            #[test]
            fn withdraw_all_with_zero_stake_funds() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(None);

                // Assert
                assert!(test_utils::get_logs().is_empty());

                assert_eq!(balances_before_withdrawal, balances);
                assert!(deserialize_receipts().is_empty());
            }

            #[test]
            #[should_panic(expected = "[ERR] [INVALID] amount must be > 0")]
            fn withdraw_some_specifying_zero() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_withdraw(Some(YoctoNear::ZERO));
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_FUNDS] ")]
            fn withdraw_some_insufficient_funds() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_withdraw(Some(
                    balances_before_withdrawal.unstaked.as_ref().unwrap().total + 1,
                ));
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_FUNDS] ")]
            fn withdraw_some_but_funds_not_available() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 3;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_withdraw(Some(
                    balances_before_withdrawal.unstaked.as_ref().unwrap().total,
                ));
            }

            #[test]
            fn withdraw_some_with_earnings_received() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // unstake 1/2
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(Some((YOCTO / 2).into()))
                {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();
                let state_before_withdrawal = StakingPoolComponent::state();

                // Act
                ctx.account_balance = env::account_balance() + (YOCTO / 2);
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(Some((1000).into()));

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec!["[INFO] [EARNINGS] 500000000000000000000000",]);

                assert_eq!(
                    balances.unstaked.as_ref().unwrap().total,
                    balances_before_withdrawal.unstaked.as_ref().unwrap().total - 1000
                );

                let state = StakingPoolComponent::state();
                assert_eq!(
                    state.last_contract_managed_total_balance,
                    state_before_withdrawal.last_contract_managed_total_balance - 1000
                        + (YOCTO / 2),
                );

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 1);
                match &receipts[0].actions[0] {
                    Action::Transfer(action) => {
                        assert_eq!(action.deposit, 1000);
                    }
                    _ => panic!("expected transfer action"),
                }
            }
        }

        #[cfg(test)]
        mod tests_restake {
            use super::*;

            #[test]
            fn restake_partial() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                let balances_before_restaking = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(balances) =
                    staking_pool.ops_restake(Some((1000).into()))
                {
                    // Assert
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                    assert_eq!(
                        logs,
                        vec![
                            "[INFO] [STAKE] near_amount=1000, stake_token_amount=1000",
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                            "[INFO] [FT_MINT] account: bob, amount: 1000",
                            "[INFO] [FT_BURN] account: bob, amount: 8",
                            "[INFO] [FT_MINT] account: owner, amount: 8",
                            "[WARN] [STATUS_OFFLINE] ",
                        ]
                    );

                    println!("{}", serde_json::to_string_pretty(&balances).unwrap());
                    assert_eq!(
                        balances.unstaked.as_ref().unwrap().total,
                        balances_before_restaking.unstaked.as_ref().unwrap().total - 1000
                    );
                    let staking_fee = staking_pool.ops_stake_fees().staking_fee * 1000;
                    assert_eq!(
                        balances.staked.as_ref().unwrap().near_value,
                        (1000 - *staking_fee).into()
                    );
                } else {
                    panic!("expected Value")
                }
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_FUNDS]")]
            fn restake_partial_with_zero_unstaked_balance() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = 0;
                ctx.account_balance = env::account_balance();
                testing_env!(ctx);
                staking_pool.ops_restake(Some(YOCTO.into()));
            }

            #[test]
            fn restake_all() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                let balance_before_restaking = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(balances) = staking_pool.ops_restake(None) {
                    // Assert
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                    assert_eq!(
                        logs,
                        vec![
                            "[INFO] [STAKE] near_amount=992000000000000000000000, stake_token_amount=992000000000000000000000",
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                            "[INFO] [FT_MINT] account: bob, amount: 992000000000000000000000",
                            "[INFO] [FT_BURN] account: bob, amount: 7936000000000000000000",
                            "[INFO] [FT_MINT] account: owner, amount: 7936000000000000000000",
                            "[WARN] [STATUS_OFFLINE] ",
                        ]
                    );

                    println!("{}", serde_json::to_string_pretty(&balances).unwrap());
                    assert!(balances.unstaked.is_none());
                    let staking_fee = staking_pool.ops_stake_fees().staking_fee
                        * balance_before_restaking.unstaked.as_ref().unwrap().total;
                    assert_eq!(
                        balances.staked.as_ref().unwrap().near_value,
                        balance_before_restaking.unstaked.as_ref().unwrap().total - staking_fee
                    );
                } else {
                    panic!("expected Value")
                }
            }

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn restake_all_with_unregistered_account() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = 0;
                ctx.account_balance = env::account_balance();
                testing_env!(ctx);
                staking_pool.ops_restake(None);
            }

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn restake_some_with_unregistered_account() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = 0;
                ctx.account_balance = env::account_balance();
                testing_env!(ctx);
                staking_pool.ops_restake(Some(YOCTO.into()));
            }

            #[test]
            fn restake_all_with_earnings_received() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // stake
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());

                if let PromiseOrValue::Promise(_) = staking_pool.ops_stake() {
                    panic!("expected value")
                }
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                // Unstake
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value")
                }

                println!(
                    "{}",
                    serde_json::to_string_pretty(&staking_pool.ops_stake_pool_balances()).unwrap()
                );

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &staking_pool.ops_stake_balance(to_valid_account_id(ACCOUNT))
                    )
                    .unwrap()
                );

                let earnings: YoctoNear = staking_pool.ops_stake_pool_balances().total_staked;
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() + *earnings;
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                println!(
                    "{}",
                    serde_json::to_string_pretty(&staking_pool.ops_stake_pool_balances()).unwrap()
                );

                // Act
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(balance) = staking_pool.ops_restake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                    assert_eq!(logs, vec![
                        "[INFO] [EARNINGS] 8000000000000000000000",
                        "[INFO] [STAKE] near_amount=992000000000000000000000, stake_token_amount=496000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                        "[INFO] [FT_MINT] account: bob, amount: 496000000000000000000000",
                        "[INFO] [FT_BURN] account: bob, amount: 3968000000000000000000",
                        "[INFO] [FT_MINT] account: owner, amount: 3968000000000000000000",
                        "[WARN] [STATUS_OFFLINE] ",
                    ]);

                    println!("{}", serde_json::to_string_pretty(&balance).unwrap());
                } else {
                    panic!("expected value")
                }
            }

            #[test]
            fn restake_all_after_withdrawing_half_with_earnings_received() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // stake
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());

                if let PromiseOrValue::Promise(_) = staking_pool.ops_stake() {
                    panic!("expected value")
                }
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                // Unstake
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value")
                }

                println!(
                    "{}",
                    serde_json::to_string_pretty(&staking_pool.ops_stake_pool_balances()).unwrap()
                );

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let balance = staking_pool.ops_stake_balance(to_valid_account_id(ACCOUNT));
                println!("{}", serde_json::to_string_pretty(&balance).unwrap());

                // Withdraw 1/2
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_withdraw(Some(
                    balance.as_ref().unwrap().unstaked.as_ref().unwrap().total / 2,
                ));

                let earnings: YoctoNear = staking_pool.ops_stake_pool_balances().total_staked;
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() + *earnings;
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                println!(
                    "{}",
                    serde_json::to_string_pretty(&staking_pool.ops_stake_pool_balances()).unwrap()
                );

                // Act

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(balance) = staking_pool.ops_restake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                    assert_eq!(logs, vec![
                        "[INFO] [EARNINGS] 8000000000000000000000",
                        "[INFO] [STAKE] near_amount=496000000000000000000000, stake_token_amount=248000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                        "[INFO] [FT_MINT] account: bob, amount: 248000000000000000000000",
                        "[INFO] [FT_BURN] account: bob, amount: 1984000000000000000000",
                        "[INFO] [FT_MINT] account: owner, amount: 1984000000000000000000",
                        "[WARN] [STATUS_OFFLINE] ",
                    ]);

                    println!("{}", serde_json::to_string_pretty(&balance).unwrap());
                } else {
                    panic!("expected value")
                }
            }
        }

        #[test]
        fn ops_stake_token_value_with_updated_earnings() {
            // Arrange
            let mut ctx = new_context(ACCOUNT);
            ctx.predecessor_account_id = OWNER.to_string();
            testing_env!(ctx.clone());

            deploy_stake_contract(staking_public_key());

            let mut account_manager = account_manager();
            let mut staking_pool = staking_pool();
            assert!(!staking_pool.ops_stake_status().is_online());

            // register account
            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = YOCTO;
            testing_env!(ctx.clone());
            account_manager.storage_deposit(None, Some(true));

            assert_eq!(
                staking_pool.ops_stake_token_value_with_earnings(None),
                YOCTO.into()
            );
            assert_eq!(
                staking_pool.ops_stake_token_value_with_earnings(None),
                staking_pool.ops_stake_token_value(None)
            );

            // stake
            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = YOCTO;
            testing_env!(ctx.clone());

            staking_pool.ops_stake();

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = 0;
            testing_env!(ctx.clone());

            assert_eq!(
                staking_pool.ops_stake_token_value_with_earnings(None),
                YOCTO.into()
            );
            assert_eq!(
                staking_pool.ops_stake_token_value_with_earnings(None),
                staking_pool.ops_stake_token_value(None)
            );

            // simulate earnings
            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance() + YOCTO;
            ctx.attached_deposit = 0;
            testing_env!(ctx.clone());

            assert_eq!(
                staking_pool.ops_stake_token_value_with_earnings(None),
                (2 * YOCTO).into()
            );

            let logs = test_utils::get_logs();
            println!("{:#?}", logs);
            assert_eq!(logs, vec!["[INFO] [EARNINGS] 1000000000000000000000000",]);
        }
    }

    #[cfg(test)]
    mod tests_online {
        use super::*;

        #[cfg(test)]
        mod tests_stake {
            use super::*;

            #[test]
            fn with_attached_deposit() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let contract_managed_total_balance = State::contract_managed_total_balance();

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                let ft_stake = ft_stake();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // assert that there was no net change to contract_managed_total_balance
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                assert_eq!(
                    contract_managed_total_balance,
                    State::contract_managed_total_balance()
                );

                assert_eq!(
                    account_manager
                        .storage_balance_of(to_valid_account_id(ACCOUNT))
                        .unwrap()
                        .available,
                    YoctoNear::ZERO
                );

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected promise");
                }
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 2);
                {
                    let receipt = &receipts[0];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::Stake(action) => {
                            assert_eq!(
                                action.stake,
                                *staking_pool.ops_stake_pool_balances().total_staked
                            );

                            assert_eq!(
                                action.public_key,
                                "1".to_string()
                                    + staking_pool
                                        .ops_stake_public_key()
                                        .to_string()
                                        .split(":")
                                        .last()
                                        .unwrap()
                            );
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
                {
                    let receipt = &receipts[1];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::FunctionCall(action) => {
                            assert_eq!(action.method_name, "ops_stake_finalize");
                            let args: StakeActionCallbackArgs =
                                serde_json::from_str(&action.args).unwrap();
                            assert_eq!(args.account_id, ACCOUNT);
                            assert_eq!(action.deposit, 0);
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }

                // Assert
                assert_eq!(logs, vec![
                    "[INFO] [STAKE] near_amount=1000000000000000000000000, stake_token_amount=1000000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: bob, amount: 1000000000000000000000000",
                    "[INFO] [FT_BURN] account: bob, amount: 8000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: owner, amount: 8000000000000000000000",
                ]);
                let staking_fee = staking_pool.ops_stake_fees().staking_fee * YOCTO;
                assert_eq!(staking_pool.ops_stake_token_value(None), YOCTO.into());
                assert_eq!(
                    ft_stake.ft_balance_of(to_valid_account_id(ACCOUNT)),
                    (YOCTO - *staking_fee).into()
                );
                assert_eq!(
                    ft_stake.ft_balance_of(to_valid_account_id(OWNER)),
                    (*staking_fee).into()
                );
                let state = StakingPoolComponent::state();
                println!("{:#?}", *state);
                assert_eq!(State::total_staked_balance(), YOCTO.into());
                assert_eq!(state.treasury_balance, YoctoNear::ZERO);

                log_contract_managed_total_balance("after staking");

                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                assert_eq!(
                    contract_managed_total_balance + YOCTO,
                    State::contract_managed_total_balance()
                );
                let stake_pool_balances = staking_pool.ops_stake_pool_balances();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&stake_pool_balances).unwrap()
                );
                assert_eq!(stake_pool_balances.treasury_balance, YoctoNear::ZERO);
                assert_eq!(stake_pool_balances.total_staked, YOCTO.into());
                assert_eq!(stake_pool_balances.total_unstaked, YoctoNear::ZERO);
                assert_eq!(stake_pool_balances.unstaked_liquidity, YoctoNear::ZERO);
            }

            #[test]
            fn with_zero_attached_deposit_and_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let contract_managed_total_balance = State::contract_managed_total_balance();

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                let ft_stake = ft_stake();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // assert that there was no net change to contract_managed_total_balance
                ctx.account_balance = env::account_balance() + YOCTO;
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                assert_eq!(
                    contract_managed_total_balance,
                    State::contract_managed_total_balance()
                );

                assert_eq!(
                    account_manager
                        .storage_balance_of(to_valid_account_id(ACCOUNT))
                        .unwrap()
                        .available,
                    YOCTO.into()
                );

                log_contract_managed_total_balance("before staking");
                assert_eq!(staking_pool.ops_stake_token_value(None), YOCTO.into());

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Promise(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);

                    let staking_fee = staking_pool.ops_stake_fees().staking_fee * YOCTO;

                    let balances = staking_pool
                        .ops_stake_balance(to_valid_account_id(ACCOUNT))
                        .unwrap();
                    assert!(balances.unstaked.is_none());
                    match balances.staked.as_ref() {
                        Some(stake) => {
                            assert_eq!(stake.stake, (YOCTO - *staking_fee).into());
                            assert_eq!(stake.near_value, (YOCTO - *staking_fee).into());
                        }
                        None => panic!("expected staked balance"),
                    }

                    // Assert
                    assert_eq!(logs, vec![
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] Withdrawal(YoctoNear(1000000000000000000000000))",
                        "[INFO] [STAKE] near_amount=1000000000000000000000000, stake_token_amount=1000000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                        "[INFO] [FT_MINT] account: bob, amount: 1000000000000000000000000",
                        "[INFO] [FT_BURN] account: bob, amount: 8000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                        "[INFO] [FT_MINT] account: owner, amount: 8000000000000000000000",
                    ]);
                    let staking_fee = staking_pool.ops_stake_fees().staking_fee * YOCTO;
                    assert_eq!(staking_pool.ops_stake_token_value(None), YOCTO.into());
                    assert_eq!(
                        ft_stake.ft_balance_of(to_valid_account_id(ACCOUNT)),
                        (YOCTO - *staking_fee).into()
                    );
                    assert_eq!(
                        ft_stake.ft_balance_of(to_valid_account_id(OWNER)),
                        (*staking_fee).into()
                    );
                    let state = StakingPoolComponent::state();
                    println!("{:#?}", *state);
                    assert_eq!(State::total_staked_balance(), YOCTO.into());
                    assert_eq!(state.treasury_balance, YoctoNear::ZERO);

                    log_contract_managed_total_balance("after staking");

                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = 0;
                    testing_env!(ctx.clone());
                    assert_eq!(
                        contract_managed_total_balance + YOCTO,
                        State::contract_managed_total_balance()
                    );
                    let stake_pool_balances = staking_pool.ops_stake_pool_balances();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&stake_pool_balances).unwrap()
                    );
                    assert_eq!(stake_pool_balances.treasury_balance, YoctoNear::ZERO);
                    assert_eq!(stake_pool_balances.total_staked, YOCTO.into());
                    assert_eq!(stake_pool_balances.total_unstaked, YoctoNear::ZERO);
                    assert_eq!(stake_pool_balances.unstaked_liquidity, YoctoNear::ZERO);
                } else {
                    panic!("expected Promise")
                }
            }

            #[test]
            fn earnings_received_through_account_balance_increase_with_zero_total_stake_supply() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                let ft_stake = ft_stake();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                ctx.account_balance = env::account_balance();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // simulate earnings received by increasing account balance
                const EARNINGS: YoctoNear = YoctoNear(YOCTO);
                ctx.account_balance = env::account_balance() + *EARNINGS;
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());

                // even there are earnings that have accumulated, because there are no stakers, then
                // we expect the STAKE token value to be 1:1
                assert_eq!(staking_pool.ops_stake_token_value(None), YOCTO.into());
                // Act
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected promise")
                }
                let balances = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [STAKE] near_amount=1000000000000000000000000, stake_token_amount=1000000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: bob, amount: 1000000000000000000000000",
                    "[INFO] [FT_BURN] account: bob, amount: 8000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: owner, amount: 8000000000000000000000",
                ]);
                println!("{}", serde_json::to_string_pretty(&balances).unwrap());
                assert_eq!(
                    *balances.staked.as_ref().unwrap().stake,
                    992000000000000000000000
                );
                // when FT total supply is zero, then earnings are held and not staked
                // earnings will be staked on the next staking action
                assert_eq!(
                    *balances.staked.as_ref().unwrap().near_value,
                    1984000000000000000000000
                );
                // STAKE token value is 2 NEAR because of the earnings that have not yet been staked
                assert_eq!(staking_pool.ops_stake_token_value(None), (2 * YOCTO).into());
                assert_eq!(ft_stake.ft_total_supply(), YOCTO.into());
                println!(
                    "owner stake balances = {}",
                    serde_json::to_string_pretty(
                        &staking_pool.ops_stake_balance(to_valid_account_id(OWNER))
                    )
                    .unwrap()
                );
                println!(
                    "{}",
                    serde_json::to_string_pretty(&staking_pool.ops_stake_pool_balances()).unwrap()
                );

                ctx.account_balance = env::account_balance() + *EARNINGS;
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());
                assert_eq!(staking_pool.ops_stake_token_value(None), (3 * YOCTO).into());

                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                // Act - simulate more earnings on next stake
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected promise")
                }
                let balances = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [EARNINGS] 2000000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Deposit(YoctoNear(1))",
                    "[INFO] [STAKE] near_amount=999999999999999999999999, stake_token_amount=333333333333333333333333",
                    "[INFO] [FT_MINT] account: bob, amount: 333333333333333333333333",
                    "[INFO] [FT_BURN] account: bob, amount: 2666666666666666666666",
                    "[INFO] [FT_MINT] account: owner, amount: 2666666666666666666666",
                ]);

                {
                    println!(
                        "account {}",
                        serde_json::to_string_pretty(&balances).unwrap()
                    );

                    assert_eq!(
                        *balances.staked.as_ref().unwrap().stake,
                        1322666666666666666666667
                    );
                    assert_eq!(
                        *balances.staked.as_ref().unwrap().near_value,
                        3968000000000000000000001
                    );
                    // because the STAKE:NEAR value is 1:3, then 1 yoctoNEAR could not be staked
                    assert_eq!(balances.storage_balance.available, 1.into());
                }

                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());

                // Assert - account STAKE NEAR balances sum up to the total staked balance
                {
                    let owner_stake_balances = staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .unwrap();
                    println!(
                        "owner_stake_balances {}",
                        serde_json::to_string_pretty(&owner_stake_balances).unwrap()
                    );
                    let stake_pool_balances = staking_pool.ops_stake_pool_balances();
                    println!(
                        "stake_pool_balances {}",
                        serde_json::to_string_pretty(&stake_pool_balances).unwrap()
                    );
                    assert_eq!(
                        stake_pool_balances.total_staked,
                        balances.staked.as_ref().unwrap().near_value
                            + owner_stake_balances.staked.as_ref().unwrap().near_value
                    );
                }

                {
                    let stake_token_value = staking_pool.ops_stake_token_value(None);
                    println!("stake_token_value = {}", stake_token_value);
                    let stake_total_supply = ft_stake.ft_total_supply();
                    println!("ft_total_supply = {}", stake_total_supply);
                    let owner_stake_balance = staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .unwrap();
                    println!(
                        "owner stake balances = {}",
                        serde_json::to_string_pretty(&owner_stake_balance).unwrap()
                    );
                    let stake_pool_balances = staking_pool.ops_stake_pool_balances();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&stake_pool_balances).unwrap()
                    );
                    // since all earnings are staked, then the total STAKE supply value should match
                    // the total staked balance
                    assert_eq!(
                        staking_pool.ops_stake_token_value(Some(stake_total_supply)),
                        stake_pool_balances.total_staked,
                    );
                }

                {
                    ctx.account_balance = env::account_balance();
                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = 0;
                    ctx.is_view = true;
                    testing_env!(ctx.clone());
                    assert_eq!(staking_pool.ops_stake_token_value(None), (3 * YOCTO).into());
                }
            }

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn with_unregistered_account() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                ctx.account_balance = env::account_balance();
                testing_env!(ctx);
                staking_pool.ops_stake();
            }

            #[test]
            #[should_panic(
                expected = "[ERR] [NEAR_DEPOSIT_REQUIRED] deposit NEAR into storage balance or attach NEAR deposit"
            )]
            fn registered_account_with_zero_storage_available_balance_and_zero_attached_deposit() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // Act
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx);
                staking_pool.ops_stake();
            }

            #[test]
            fn stake_amount_too_low_too_stake() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                ctx.account_balance = env::account_balance();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() + (2 * YOCTO);
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());

                assert_eq!(staking_pool.ops_stake_token_value(None), (3 * YOCTO).into());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());
                assert_eq!(staking_pool.ops_stake_token_value(None), (3 * YOCTO).into());
                assert_eq!(
                    *staking_pool.ops_stake_pool_balances().total_staked,
                    (5 * YOCTO) - 2
                );
                println!(
                    "ops_stake_token_value = {}",
                    staking_pool.ops_stake_token_value(None)
                );
                assert_eq!(
                    staking_pool
                        .ops_stake_balance(to_valid_account_id(ACCOUNT))
                        .unwrap()
                        .storage_balance
                        .available,
                    2.into()
                );

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(balances) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                    assert_eq!(
                        logs,
                        vec![
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] Withdrawal(YoctoNear(2))",
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] Deposit(YoctoNear(2))",
                            "[INFO] [NOT_ENOUGH_TO_STAKE] ",
                        ]
                    );

                    assert_eq!(balances.storage_balance.available, 2.into());
                } else {
                    panic!("expected value")
                }
            }

            #[test]
            fn stake_when_liquidity_needed() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                ctx.account_balance = env::account_balance();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 10 * YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());

                let total_staked_balance = staking_pool.ops_stake_pool_balances().total_staked;
                assert_eq!(total_staked_balance, (10 * YOCTO).into());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() - *total_staked_balance;
                ctx.account_locked_balance = *total_staked_balance;
                ctx.attached_deposit = 0;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                staking_pool.ops_unstake(None);
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                println!(
                    "{}",
                    serde_json::to_string_pretty(&staking_pool.ops_stake_pool_balances()).unwrap()
                );

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = 5 * YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [LIQUIDITY] added=5000000000000000000000000, total=5000000000000000000000000",
                    "[INFO] [STAKE] near_amount=5000000000000000000000000, stake_token_amount=5000000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: bob, amount: 5000000000000000000000000",
                    "[INFO] [FT_BURN] account: bob, amount: 40000000000000000000000",
                    "[INFO] [FT_MINT] account: owner, amount: 40000000000000000000000",
                ]);

                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                assert_eq!(pool_balances.unstaked_liquidity, (5 * YOCTO).into());
            }

            #[test]
            fn stake_with_treasury_balance() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                ctx.account_balance = env::account_balance();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 10 * YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());
                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                assert_eq!(
                    pool_balances,
                    serde_json::from_str(
                        r#"{
  "total_staked": "10000000000000000000000000",
  "total_stake_supply": "10000000000000000000000000",
  "total_unstaked": "0",
  "unstaked_liquidity": "0",
  "treasury_balance": "0",
  "current_contract_managed_total_balance": "13172980000000000000000000",
  "last_contract_managed_total_balance": "13172980000000000000000000",
  "earnings": "0"
}"#
                    )
                    .unwrap()
                );

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() - *pool_balances.total_staked;
                ctx.account_locked_balance = *pool_balances.total_staked;
                ctx.attached_deposit = 1;
                ctx.is_view = false;
                testing_env!(ctx.clone());

                let owner_balance = staking_pool
                    .ops_stake_balance(to_valid_account_id(OWNER))
                    .unwrap();
                staking_pool.ops_stake_transfer_call(
                    to_valid_account_id(&env::current_account_id()),
                    owner_balance.staked.as_ref().unwrap().near_value,
                    None,
                    "".into(),
                );

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());
                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                assert_eq!(pool_balances.treasury_balance, YoctoNear::ZERO);
                assert_eq!(
                    staking_pool
                        .ops_stake_balance(to_valid_account_id(&env::current_account_id()))
                        .unwrap()
                        .staked
                        .unwrap()
                        .stake,
                    owner_balance.staked.as_ref().unwrap().stake
                );

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = YOCTO;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [EARNINGS] 1",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Deposit(YoctoNear(1))",
                    "[INFO] [STAKE] near_amount=999999999999999999999999, stake_token_amount=999999999999999999999999",
                    "[INFO] [FT_MINT] account: bob, amount: 999999999999999999999999",
                    "[INFO] [FT_BURN] account: bob, amount: 7999999999999999999998",
                    "[INFO] [FT_MINT] account: owner, amount: 7999999999999999999998",
                ]);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());
                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                // funds that were transferred over from the owner FT account now are picked up
                // by the treasury
                assert_eq!(
                    pool_balances.treasury_balance,
                    owner_balance.staked.as_ref().unwrap().near_value
                );

                println!(
                    "stake_token_value = {}",
                    staking_pool.ops_stake_token_value(None)
                );

                // Act
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = YOCTO;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Withdrawal(YoctoNear(1))",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Deposit(YoctoNear(1))",
                    "[INFO] [STAKE] near_amount=1000000000000000000000000, stake_token_amount=1000000000000000000000000",
                    "[INFO] [FT_MINT] account: bob, amount: 1000000000000000000000000",
                    "[INFO] [FT_BURN] account: bob, amount: 7999999999999999999999",
                    "[INFO] [FT_MINT] account: owner, amount: 7999999999999999999999",
                ]);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());
                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());

                // Act - stake again
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = YOCTO;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = 0;
                ctx.is_view = true;
                testing_env!(ctx.clone());
                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                assert_eq!(
                    pool_balances,
                    serde_json::from_str(
                        r#"{
  "total_staked": "13000000000000000000000000",
  "total_stake_supply": "12999999999999999999999999",
  "total_unstaked": "0",
  "unstaked_liquidity": "0",
  "treasury_balance": "80000000000000000000000",
  "current_contract_managed_total_balance": "16172980000000000000000000",
  "last_contract_managed_total_balance": "16172980000000000000000000",
  "earnings": "0"
}"#
                    )
                    .unwrap()
                );

                let treasury_balance = staking_pool
                    .ops_stake_balance(to_valid_account_id(&env::current_account_id()))
                    .unwrap();
                println!(
                    "treasury balance: {}",
                    serde_json::to_string_pretty(&treasury_balance).unwrap()
                );

                // Act - stake again - simulate earnings
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() + (YOCTO / 10);
                ctx.account_locked_balance = env::account_locked_balance();
                ctx.attached_deposit = YOCTO;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [EARNINGS] 100000000000000000000000",
                    "[INFO] [FT_BURN] account: contract.near, amount: 610687022900763358778",
                    "[INFO] [TREASURY_DIVIDEND] 615384615384615384615 yoctoNEAR / 610687022900763358778 yoctoSTAKE",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Withdrawal(YoctoNear(1))",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Deposit(YoctoNear(1))",
                    "[INFO] [STAKE] near_amount=1000000000000000000000000, stake_token_amount=992319794883748033331391",
                    "[INFO] [FT_MINT] account: bob, amount: 992319794883748033331391",
                    "[INFO] [FT_BURN] account: bob, amount: 7938558359069984266651",
                    "[INFO] [FT_MINT] account: owner, amount: 7938558359069984266651",
                ]);

                let pool_balances_after_dividend_payout = staking_pool.ops_stake_pool_balances();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&pool_balances_after_dividend_payout).unwrap()
                );
                let treasury_balance_after_dividend_paid = staking_pool
                    .ops_stake_balance(to_valid_account_id(&env::current_account_id()))
                    .unwrap();
                println!(
                    "treasury balance: {}",
                    serde_json::to_string_pretty(&treasury_balance_after_dividend_paid).unwrap()
                );
                assert_eq!(
                    *treasury_balance_after_dividend_paid
                        .staked
                        .as_ref()
                        .unwrap()
                        .stake,
                    *treasury_balance.staked.as_ref().unwrap().stake - 610687022900763358778
                );
                assert_eq!(
                    treasury_balance_after_dividend_paid,
                    serde_json::from_str(
                        r#"{
  "storage_balance": {
    "total": "3930000000000000000000",
    "available": "0"
  },
  "staked": {
    "stake": "79389312977099236641222",
    "near_value": "80003758250534376247857"
  },
  "unstaked": null
}"#
                    )
                    .unwrap()
                );
            }

            #[test]
            fn as_owner() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let contract_managed_total_balance = State::contract_managed_total_balance();

                let mut staking_pool = staking_pool();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                let ft_stake = ft_stake();

                ctx.account_balance = env::account_balance();
                ctx.is_view = true;
                testing_env!(ctx.clone());
                let owner_balance = staking_pool
                    .ops_stake_balance(to_valid_account_id(OWNER))
                    .unwrap();

                // Act
                ctx.predecessor_account_id = OWNER.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                ctx.is_view = false;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected promise");
                }
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Withdrawal(YoctoNear(9996819160000000000000000000))",
                    "[INFO] [STAKE] near_amount=9997819160000000000000000000, stake_token_amount=9997819160000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: owner, amount: 9997819160000000000000000000",
                ]);

                let pool_balances = staking_pool.ops_stake_pool_balances();
                assert_eq!(
                    pool_balances.total_staked,
                    owner_balance.storage_balance.available + YOCTO
                );

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 2);
                {
                    let receipt = &receipts[0];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::Stake(action) => {
                            assert_eq!(
                                action.stake,
                                *staking_pool.ops_stake_pool_balances().total_staked
                            );

                            assert_eq!(
                                action.public_key,
                                "1".to_string()
                                    + staking_pool
                                        .ops_stake_public_key()
                                        .to_string()
                                        .split(":")
                                        .last()
                                        .unwrap()
                            );
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
                {
                    let receipt = &receipts[1];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::FunctionCall(action) => {
                            assert_eq!(action.method_name, "ops_stake_finalize");
                            let args: StakeActionCallbackArgs =
                                serde_json::from_str(&action.args).unwrap();
                            assert_eq!(args.account_id, OWNER);
                            assert_eq!(action.deposit, 0);
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }

                // Assert
                assert_eq!(staking_pool.ops_stake_token_value(None), YOCTO.into());
                assert_eq!(
                    *ft_stake.ft_balance_of(to_valid_account_id(OWNER)),
                    *(owner_balance.storage_balance.available + YOCTO)
                );
                let state = StakingPoolComponent::state();
                println!("{:#?}", *state);
                assert_eq!(state.treasury_balance, YoctoNear::ZERO);

                log_contract_managed_total_balance("after staking");

                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                assert_eq!(
                    contract_managed_total_balance
                        + owner_balance.storage_balance.available
                        + YOCTO,
                    State::contract_managed_total_balance()
                );
                let stake_pool_balances = staking_pool.ops_stake_pool_balances();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&stake_pool_balances).unwrap()
                );
                assert_eq!(stake_pool_balances.treasury_balance, YoctoNear::ZERO);
                assert_eq!(
                    stake_pool_balances.total_staked,
                    staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .as_ref()
                        .unwrap()
                        .staked
                        .as_ref()
                        .unwrap()
                        .near_value
                );
                assert_eq!(stake_pool_balances.total_unstaked, YoctoNear::ZERO);
                assert_eq!(stake_pool_balances.unstaked_liquidity, YoctoNear::ZERO);
            }
        }

        #[cfg(test)]
        mod tests_unstake {
            use super::*;

            #[test]
            fn unstake_partial() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Promise(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);

                    let pool_balances = staking_pool.ops_stake_pool_balances();
                    println!(
                        "pool_balances before unstaking: {}",
                        serde_json::to_string_pretty(&pool_balances).unwrap()
                    );
                } else {
                    panic!("expected promise")
                }

                let balances = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();
                let staked_balance = balances.staked.as_ref().unwrap().near_value;

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Promise(_) =
                    staking_pool.ops_unstake(Some((*staked_balance / 4).into()))
                {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                    assert_eq!(logs, vec![
                        "[INFO] [UNSTAKE] near_amount=248000000000000000000000, stake_token_amount=248000000000000000000000",
                        "[INFO] [FT_BURN] account: bob, amount: 248000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(184)",
                    ]);

                    let balances_after_unstaking = staking_pool
                        .ops_stake_balance(to_valid_account_id(ACCOUNT))
                        .unwrap();
                    println!(
                        "account balances_after_unstaking: {}",
                        serde_json::to_string_pretty(&balances_after_unstaking).unwrap()
                    );
                    assert_eq!(
                        balances_after_unstaking.unstaked.as_ref().unwrap().total,
                        staked_balance / 4
                    );
                    assert_eq!(
                        balances_after_unstaking
                            .unstaked
                            .as_ref()
                            .unwrap()
                            .available,
                        YoctoNear::ZERO
                    );

                    let pool_balances = staking_pool.ops_stake_pool_balances();
                    println!(
                        "pool_balances: {}",
                        serde_json::to_string_pretty(&pool_balances).unwrap()
                    );
                    assert_eq!(
                        pool_balances.total_staked,
                        balances_after_unstaking.staked.as_ref().unwrap().near_value
                            + staking_pool
                                .ops_stake_balance(to_valid_account_id(OWNER))
                                .unwrap()
                                .staked
                                .unwrap()
                                .near_value
                    );
                    assert_eq!(
                        pool_balances.total_unstaked,
                        balances_after_unstaking.unstaked.as_ref().unwrap().total
                    );
                } else {
                    panic!("expected value")
                }

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 2);
                {
                    let receipt = &receipts[0];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::Stake(action) => {
                            assert_eq!(
                                action.stake,
                                *staking_pool.ops_stake_pool_balances().total_staked
                            );

                            assert_eq!(
                                action.public_key,
                                "1".to_string()
                                    + staking_pool
                                        .ops_stake_public_key()
                                        .to_string()
                                        .split(":")
                                        .last()
                                        .unwrap()
                            );
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
                {
                    let receipt = &receipts[1];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::FunctionCall(action) => {
                            assert_eq!(action.method_name, "ops_stake_finalize");
                            let args: StakeActionCallbackArgs =
                                serde_json::from_str(&action.args).unwrap();
                            assert_eq!(args.account_id, ACCOUNT);
                            assert_eq!(action.deposit, 0);
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
            }

            #[test]
            fn unstake_all() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected value")
                }

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!(
                    "pool_balances before unstaking: {}",
                    serde_json::to_string_pretty(&pool_balances).unwrap()
                );

                let balances = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();
                let staked_balance = balances.staked.as_ref().unwrap().near_value;

                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    panic!("expected Promise")
                }

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [UNSTAKE] near_amount=992000000000000000000000, stake_token_amount=992000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-104)",
                    "[INFO] [FT_BURN] account: bob, amount: 992000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(184)",
                ]);

                let balances_after_unstaking = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();
                println!(
                    "account balances_after_unstaking: {}",
                    serde_json::to_string_pretty(&balances_after_unstaking).unwrap()
                );
                assert_eq!(
                    balances_after_unstaking.unstaked.as_ref().unwrap().total,
                    staked_balance
                );
                assert_eq!(
                    balances_after_unstaking
                        .unstaked
                        .as_ref()
                        .unwrap()
                        .available,
                    YoctoNear::ZERO
                );

                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!(
                    "pool_balances: {}",
                    serde_json::to_string_pretty(&pool_balances).unwrap()
                );
                assert_eq!(
                    pool_balances.total_staked,
                    staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .unwrap()
                        .staked
                        .unwrap()
                        .near_value
                );
                assert_eq!(
                    pool_balances.total_unstaked,
                    balances_after_unstaking.unstaked.as_ref().unwrap().total
                );

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 2);
                {
                    let receipt = &receipts[0];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::Stake(action) => {
                            assert_eq!(
                                action.stake,
                                *staking_pool.ops_stake_pool_balances().total_staked
                            );

                            assert_eq!(
                                action.public_key,
                                "1".to_string()
                                    + staking_pool
                                        .ops_stake_public_key()
                                        .to_string()
                                        .split(":")
                                        .last()
                                        .unwrap()
                            );
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
                {
                    let receipt = &receipts[1];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::FunctionCall(action) => {
                            assert_eq!(action.method_name, "ops_stake_finalize");
                            let args: StakeActionCallbackArgs =
                                serde_json::from_str(&action.args).unwrap();
                            assert_eq!(args.account_id, ACCOUNT);
                            assert_eq!(action.deposit, 0);
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
            }

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn account_not_registered() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx);
                staking_pool.ops_unstake(None);
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_FUNDS] STAKE balance is zero")]
            fn zero_stake_balance_and_unstake_specified_amount() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx);
                staking_pool.ops_unstake(Some(YOCTO.into()));
            }

            #[test]
            fn zero_stake_balance_and_unstake_all() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // Act
                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx);
                if let PromiseOrValue::Value(balances) = staking_pool.ops_unstake(None) {
                    assert!(balances.staked.is_none());
                } else {
                    panic!("expected value")
                }
            }

            #[test]
            fn unstake_with_earnings_received() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // stake
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());

                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected promise")
                }
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() - *EARNINGS;
                ctx.account_locked_balance = *EARNINGS;
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let staked_balance = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                let staking_fee = staking_pool.ops_stake_fees().staking_fee * YOCTO;
                assert_eq!(
                    staked_balance.staked.as_ref().unwrap().near_value,
                    (YOCTO - *staking_fee).into()
                );

                // Act
                const EARNINGS: YoctoNear = YoctoNear(YOCTO);
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() + *EARNINGS;
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    panic!("expected promise")
                }

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [EARNINGS] 1000000000000000000000000",
                    "[INFO] [UNSTAKE] near_amount=1984000000000000000000000, stake_token_amount=992000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-104)",
                    "[INFO] [FT_BURN] account: bob, amount: 992000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(184)",
                ]);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() - *EARNINGS;
                ctx.account_locked_balance = *EARNINGS;
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let balance = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                let unstaked_near_value = staking_pool
                    .ops_stake_token_value(Some(staked_balance.staked.as_ref().unwrap().stake));
                assert_eq!(
                    balance.unstaked.as_ref().unwrap().total,
                    unstaked_near_value
                );
            }

            #[test]
            fn unstake_with_dividend_received() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // stake
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected promise")
                }
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() - *EARNINGS;
                ctx.account_locked_balance = *EARNINGS;
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let staked_balance = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                let staking_fee = staking_pool.ops_stake_fees().staking_fee * YOCTO;
                assert_eq!(
                    staked_balance.staked.as_ref().unwrap().near_value,
                    (YOCTO - *staking_fee).into()
                );

                // deposit into treasury
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_treasury_deposit();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                // Act
                const EARNINGS: YoctoNear = YoctoNear(YOCTO);
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() + *EARNINGS;
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    panic!("expected promise")
                }

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [EARNINGS] 1000000000000000000000000",
                    "[INFO] [FT_BURN] account: contract.near, amount: 333333333333333333333333",
                    "[INFO] [TREASURY_DIVIDEND] 500000000000000000000000 yoctoNEAR / 333333333333333333333333 yoctoSTAKE",
                    "[INFO] [UNSTAKE] near_amount=1785599999999999999999999, stake_token_amount=992000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-104)",
                    "[INFO] [FT_BURN] account: bob, amount: 992000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(184)",
                ]);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() - *EARNINGS;
                ctx.account_locked_balance = *EARNINGS;
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let balance = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                println!("{}", serde_json::to_string_pretty(&balance).unwrap());
                let unstaked_near_value = staking_pool
                    .ops_stake_token_value(Some(staked_balance.staked.as_ref().unwrap().stake));
                assert_eq!(
                    balance.unstaked.as_ref().unwrap().total + balance.storage_balance.available,
                    unstaked_near_value
                );
            }
        }

        #[cfg(test)]
        mod tests_withdraw {
            use super::*;

            #[test]
            fn withdraw_partial() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Promise(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected Promise");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Promise(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected Promise");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(Some((1000).into()));

                // Assert
                assert!(test_utils::get_logs().is_empty());

                assert_eq!(
                    balances.unstaked.as_ref().unwrap().total,
                    balances_before_withdrawal.unstaked.as_ref().unwrap().total - 1000
                );

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 1);
                match &receipts[0].actions[0] {
                    Action::Transfer(action) => {
                        assert_eq!(action.deposit, 1000);
                    }
                    _ => panic!("expected transfer action"),
                }
            }

            #[test]
            fn withdraw_all() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Promise(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected Promise");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Promise(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected Promise");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(None);

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec!["[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-184)",]
                );

                assert!(balances.unstaked.is_none());

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 1);
                match &receipts[0].actions[0] {
                    Action::Transfer(action) => {
                        assert_eq!(
                            action.deposit,
                            *balances_before_withdrawal.unstaked.as_ref().unwrap().total
                        );
                    }
                    _ => panic!("expected transfer action"),
                }
            }

            #[test]
            fn withdraw_available_using_liquidity() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected promise");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    panic!("expected promise");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                State::add_liquidity((YOCTO / 2).into());
                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(None);

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec!["[INFO] [LIQUIDITY] removed=500000000000000000000000, total=0",]
                );

                assert_eq!(
                    balances.unstaked.as_ref().unwrap().total,
                    balances_before_withdrawal.unstaked.as_ref().unwrap().total - (YOCTO / 2)
                );
                assert_eq!(
                    balances.unstaked.as_ref().unwrap().available,
                    YoctoNear::ZERO
                );

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 1);
                match &receipts[0].actions[0] {
                    Action::Transfer(action) => {
                        assert_eq!(action.deposit, (YOCTO / 2));
                    }
                    _ => panic!("expected transfer action"),
                }
            }

            #[test]
            fn withdraw_available_using_full_liquidity() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected promise");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    panic!("expected Promise");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // staking adds liquidity
                ctx.predecessor_account_id = OWNER.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = *balances_before_withdrawal.unstaked.as_ref().unwrap().total;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                // Act
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(None);

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec![
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-184)",
                        "[INFO] [LIQUIDITY] removed=992000000000000000000000, total=0",
                    ]
                );

                assert!(balances.unstaked.is_none(),);

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 1);
                match &receipts[0].actions[0] {
                    Action::Transfer(action) => {
                        assert_eq!(action.deposit, 992000000000000000000000);
                    }
                    _ => panic!("expected transfer action"),
                }
            }

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED] ")]
            fn withdraw_all_not_registered() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                testing_env!(ctx);
                staking_pool.ops_stake_withdraw(None);
            }

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED] ")]
            fn withdraw_some_not_registered() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                testing_env!(ctx);
                staking_pool.ops_stake_withdraw(Some(YOCTO.into()));
            }

            #[test]
            fn withdraw_all_with_zero_unstaked_funds() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected promise");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(None);

                // Assert
                assert!(test_utils::get_logs().is_empty());

                assert_eq!(balances_before_withdrawal, balances);
                assert!(deserialize_receipts().is_empty());
            }

            #[test]
            fn withdraw_all_with_zero_stake_funds() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(None);

                // Assert
                assert!(test_utils::get_logs().is_empty());

                assert_eq!(balances_before_withdrawal, balances);
                assert!(deserialize_receipts().is_empty());
            }

            #[test]
            #[should_panic(expected = "[ERR] [INVALID] amount must be > 0")]
            fn withdraw_some_specifying_zero() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected promise");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    panic!("expected promise");
                }

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_withdraw(Some(YoctoNear::ZERO));
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_FUNDS] ")]
            fn withdraw_some_insufficient_funds() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected Promise");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    panic!("expected Promise");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_withdraw(Some(
                    balances_before_withdrawal.unstaked.as_ref().unwrap().total + 1,
                ));
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_FUNDS] ")]
            fn withdraw_some_but_funds_not_available() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected promise");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    panic!("expected promise");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 3;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_withdraw(Some(
                    balances_before_withdrawal.unstaked.as_ref().unwrap().total,
                ));
            }

            #[test]
            fn withdraw_some_with_earnings_received() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                // unstake 1/2
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(Some((YOCTO / 2).into()))
                {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected value");
                }

                let balances_before_withdrawal = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();
                let state_before_withdrawal = StakingPoolComponent::state();

                // Act
                ctx.account_balance = env::account_balance() + (YOCTO / 2);
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                let balances = staking_pool.ops_stake_withdraw(Some((1000).into()));

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec!["[INFO] [EARNINGS] 500000000000000000000000",]);

                assert_eq!(
                    balances.unstaked.as_ref().unwrap().total,
                    balances_before_withdrawal.unstaked.as_ref().unwrap().total - 1000
                );

                let state = StakingPoolComponent::state();
                assert_eq!(
                    state.last_contract_managed_total_balance,
                    state_before_withdrawal.last_contract_managed_total_balance - 1000
                        + (YOCTO / 2),
                );

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 1);
                match &receipts[0].actions[0] {
                    Action::Transfer(action) => {
                        assert_eq!(action.deposit, 1000);
                    }
                    _ => panic!("expected transfer action"),
                }
            }
        }

        #[cfg(test)]
        mod tests_restake {
            use super::*;

            #[test]
            fn restake_partial() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Promise(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected Promise");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Promise(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected Promise");
                }

                let balances_before_restaking = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_restake(Some((1000).into())) {
                    panic!("expected Promise")
                }

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec![
                        "[INFO] [STAKE] near_amount=1000, stake_token_amount=1000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                        "[INFO] [FT_MINT] account: bob, amount: 1000",
                        "[INFO] [FT_BURN] account: bob, amount: 8",
                        "[INFO] [FT_MINT] account: owner, amount: 8",
                    ]
                );

                let balances = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();
                println!("{}", serde_json::to_string_pretty(&balances).unwrap());
                assert_eq!(
                    balances.unstaked.as_ref().unwrap().total,
                    balances_before_restaking.unstaked.as_ref().unwrap().total - 1000
                );
                let staking_fee = staking_pool.ops_stake_fees().staking_fee * 1000;
                assert_eq!(
                    balances.staked.as_ref().unwrap().near_value,
                    (1000 - *staking_fee).into()
                );

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 2);
                {
                    let receipt = &receipts[0];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::Stake(action) => {
                            assert_eq!(
                                action.stake,
                                *staking_pool.ops_stake_pool_balances().total_staked
                            );

                            assert_eq!(
                                action.public_key,
                                "1".to_string()
                                    + staking_pool
                                        .ops_stake_public_key()
                                        .to_string()
                                        .split(":")
                                        .last()
                                        .unwrap()
                            );
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
                {
                    let receipt = &receipts[1];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::FunctionCall(action) => {
                            assert_eq!(action.method_name, "ops_stake_finalize");
                            let args: StakeActionCallbackArgs =
                                serde_json::from_str(&action.args).unwrap();
                            assert_eq!(args.account_id, ACCOUNT);
                            assert_eq!(action.deposit, 0);
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
            }

            #[test]
            fn restake_all() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.account_balance = env::account_balance();
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));
                account_manager.storage_deposit(None, Some(false));

                // stake storage deposit
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Promise(_) = staking_pool.ops_stake() {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected Promise");
                }

                // unstake all
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Promise(_) = staking_pool.ops_unstake(None) {
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                } else {
                    panic!("expected Promise");
                }

                let balance_before_restaking = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();

                // Act
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_restake(None) {
                    panic!("expected Promise")
                }

                // Assert
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec![
                        "[INFO] [STAKE] near_amount=992000000000000000000000, stake_token_amount=992000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                        "[INFO] [FT_MINT] account: bob, amount: 992000000000000000000000",
                        "[INFO] [FT_BURN] account: bob, amount: 7936000000000000000000",
                        "[INFO] [FT_MINT] account: owner, amount: 7936000000000000000000",
                    ]
                );

                let balances = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();
                println!("{}", serde_json::to_string_pretty(&balances).unwrap());
                assert!(balances.unstaked.is_none());
                let staking_fee = staking_pool.ops_stake_fees().staking_fee
                    * balance_before_restaking.unstaked.as_ref().unwrap().total;
                assert_eq!(
                    balances.staked.as_ref().unwrap().near_value,
                    balance_before_restaking.unstaked.as_ref().unwrap().total - staking_fee
                );

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 2);
                {
                    let receipt = &receipts[0];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::Stake(action) => {
                            assert_eq!(
                                action.stake,
                                *staking_pool.ops_stake_pool_balances().total_staked
                            );

                            assert_eq!(
                                action.public_key,
                                "1".to_string()
                                    + staking_pool
                                        .ops_stake_public_key()
                                        .to_string()
                                        .split(":")
                                        .last()
                                        .unwrap()
                            );
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
                {
                    let receipt = &receipts[1];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::FunctionCall(action) => {
                            assert_eq!(action.method_name, "ops_stake_finalize");
                            let args: StakeActionCallbackArgs =
                                serde_json::from_str(&action.args).unwrap();
                            assert_eq!(args.account_id, ACCOUNT);
                            assert_eq!(action.deposit, 0);
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
            }

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn restake_all_with_unregistered_account() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                ctx.account_balance = env::account_balance();
                testing_env!(ctx);
                staking_pool.ops_restake(None);
            }

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn restake_some_with_unregistered_account() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                ctx.account_balance = env::account_balance();
                testing_env!(ctx);
                staking_pool.ops_restake(Some(YOCTO.into()));
            }

            #[test]
            fn restake_all_with_earnings_received() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // stake
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());

                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected promise")
                }
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                // Unstake
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    panic!("expected promise")
                }
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                println!(
                    "{}",
                    serde_json::to_string_pretty(&staking_pool.ops_stake_pool_balances()).unwrap()
                );

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &staking_pool.ops_stake_balance(to_valid_account_id(ACCOUNT))
                    )
                    .unwrap()
                );

                let earnings: YoctoNear = staking_pool.ops_stake_pool_balances().total_staked;
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() + *earnings;
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                println!(
                    "{}",
                    serde_json::to_string_pretty(&staking_pool.ops_stake_pool_balances()).unwrap()
                );

                // Act
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_restake(None) {
                    panic!("expected value")
                }
                let balance = staking_pool.ops_stake_balance(to_valid_account_id(ACCOUNT));
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [EARNINGS] 8000000000000000000000",
                    "[INFO] [STAKE] near_amount=992000000000000000000000, stake_token_amount=496000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: bob, amount: 496000000000000000000000",
                    "[INFO] [FT_BURN] account: bob, amount: 3968000000000000000000",
                    "[INFO] [FT_MINT] account: owner, amount: 3968000000000000000000",
                ]);

                println!("{}", serde_json::to_string_pretty(&balance).unwrap());
            }

            #[test]
            fn restake_all_after_withdrawing_half_with_earnings_received() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();
                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // stake
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());

                if let PromiseOrValue::Value(_) = staking_pool.ops_stake() {
                    panic!("expected promise")
                }
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                // Unstake
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_unstake(None) {
                    panic!("expected promise")
                }

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                println!(
                    "{}",
                    serde_json::to_string_pretty(&staking_pool.ops_stake_pool_balances()).unwrap()
                );

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let balance = staking_pool.ops_stake_balance(to_valid_account_id(ACCOUNT));
                println!("{}", serde_json::to_string_pretty(&balance).unwrap());

                // Withdraw 1/2
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_withdraw(Some(
                    balance.as_ref().unwrap().unstaked.as_ref().unwrap().total / 2,
                ));

                let earnings: YoctoNear = staking_pool.ops_stake_pool_balances().total_staked;
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance() + *earnings;
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                println!(
                    "{}",
                    serde_json::to_string_pretty(&staking_pool.ops_stake_pool_balances()).unwrap()
                );

                // Act

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Value(_) = staking_pool.ops_restake(None) {
                    panic!("expected value")
                }
                let balance = staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [EARNINGS] 8000000000000000000000",
                    "[INFO] [STAKE] near_amount=496000000000000000000000, stake_token_amount=248000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: bob, amount: 248000000000000000000000",
                    "[INFO] [FT_BURN] account: bob, amount: 1984000000000000000000",
                    "[INFO] [FT_MINT] account: owner, amount: 1984000000000000000000",
                ]);

                println!("{}", serde_json::to_string_pretty(&balance).unwrap());
            }
        }
    }

    #[cfg(test)]
    mod tests_callbacks {
        use super::*;

        #[cfg(test)]
        mod tests_stake_callback {
            use super::*;

            mod tests_online {
                use super::*;

                #[test]
                fn promise_success_with_zero_earnings() {
                    // Arrange
                    let mut ctx = new_context(ACCOUNT);
                    ctx.predecessor_account_id = OWNER.to_string();
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());

                    let mut account_manager = account_manager();
                    let mut staking_pool = staking_pool();

                    // start staking
                    ctx.predecessor_account_id = OWNER.to_string();
                    testing_env!(ctx.clone());
                    staking_pool
                        .ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                    assert!(staking_pool.ops_stake_status().is_online());

                    // register account
                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    account_manager.storage_deposit(None, Some(true));

                    // stake
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake();

                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);

                    let receipts = deserialize_receipts();
                    match &receipts[1].actions[0] {
                        Action::FunctionCall(action) => {
                            let args: StakeActionCallbackArgs =
                                serde_json::from_str(&action.args).unwrap();

                            ctx.predecessor_account_id = (&receipts[1]).receiver_id.to_string();
                            ctx.account_balance = env::account_balance()
                                - *staking_pool.ops_stake_pool_balances().total_staked;
                            ctx.account_locked_balance =
                                *staking_pool.ops_stake_pool_balances().total_staked;
                            testing_env_with_promise_result_success(ctx.clone());
                            let state_before_callback = staking_pool.state_with_updated_earnings();
                            let balances = staking_pool.ops_stake_finalize(args.account_id.clone());
                            println!("{}", serde_json::to_string_pretty(&balances).unwrap());
                            assert_eq!(
                                balances,
                                staking_pool
                                    .ops_stake_balance(to_valid_account_id(&args.account_id))
                                    .unwrap()
                            );
                            let state_after_callback = staking_pool.state_with_updated_earnings();
                            assert_eq!(
                                state_before_callback.last_contract_managed_total_balance,
                                state_after_callback.last_contract_managed_total_balance
                            );
                        }
                        _ => panic!("expected function call"),
                    }
                }

                #[test]
                fn promise_failure() {
                    // Arrange
                    let mut ctx = new_context(ACCOUNT);
                    ctx.predecessor_account_id = OWNER.to_string();
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());

                    let mut account_manager = account_manager();
                    let mut staking_pool = staking_pool();

                    // start staking
                    ctx.predecessor_account_id = OWNER.to_string();
                    testing_env!(ctx.clone());
                    staking_pool
                        .ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                    assert!(staking_pool.ops_stake_status().is_online());

                    // register account
                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    account_manager.storage_deposit(None, Some(true));

                    // stake
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake();

                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);

                    let receipts = deserialize_receipts();
                    match &receipts[1].actions[0] {
                        Action::FunctionCall(action) => {
                            let args: StakeActionCallbackArgs =
                                serde_json::from_str(&action.args).unwrap();

                            ctx.predecessor_account_id = (&receipts[1]).receiver_id.to_string();
                            ctx.account_balance = env::account_balance()
                                - *staking_pool.ops_stake_pool_balances().total_staked;
                            ctx.account_locked_balance =
                                *staking_pool.ops_stake_pool_balances().total_staked;
                            testing_env_with_promise_result_failure(ctx.clone());
                            let state_before_callback = staking_pool.state_with_updated_earnings();
                            let balances = staking_pool.ops_stake_finalize(args.account_id.clone());
                            println!("{}", serde_json::to_string_pretty(&balances).unwrap());
                            assert_eq!(
                                balances,
                                staking_pool
                                    .ops_stake_balance(to_valid_account_id(&args.account_id))
                                    .unwrap()
                            );
                            let state_after_callback = staking_pool.state_with_updated_earnings();
                            assert_eq!(
                                state_before_callback.last_contract_managed_total_balance,
                                state_after_callback.last_contract_managed_total_balance
                            );

                            let receipts = deserialize_receipts();
                            assert_eq!(receipts.len(), 2);
                            {
                                let receipt = &receipts[0];
                                assert_eq!(receipt.receiver_id, env::current_account_id());
                                assert_eq!(receipt.actions.len(), 1);
                                match &receipt.actions[0] {
                                    Action::Stake(action) => {
                                        assert_eq!(action.stake, 0);

                                        assert_eq!(
                                            action.public_key,
                                            "1".to_string()
                                                + staking_pool
                                                    .ops_stake_public_key()
                                                    .to_string()
                                                    .split(":")
                                                    .last()
                                                    .unwrap()
                                        );
                                    }
                                    _ => panic!("expected StakeAction"),
                                }
                            }
                            {
                                let receipt = &receipts[1];
                                assert_eq!(receipt.receiver_id, env::current_account_id());
                                assert_eq!(receipt.actions.len(), 1);
                                match &receipt.actions[0] {
                                    Action::FunctionCall(action) => {
                                        assert_eq!(action.method_name, "ops_stake_stop_finalize");
                                        assert!(action.args.is_empty());
                                        assert_eq!(action.deposit, 0);
                                    }
                                    _ => panic!("expected StakeAction"),
                                }
                            }
                        }
                        _ => panic!("expected function call"),
                    }
                }
            }
        }

        #[cfg(test)]
        mod tests_start_finalize_callback {
            use super::*;

            #[test]
            fn promise_success() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // Act
                ctx.predecessor_account_id = env::current_account_id();
                testing_env_with_promise_result_success(ctx.clone());
                staking_pool.ops_stake_start_finalize();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec!["[INFO] [STATUS_ONLINE] staked"])
            }

            #[test]
            fn promise_failure_with_zero_staked_balance() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // Act
                ctx.predecessor_account_id = env::current_account_id();
                testing_env_with_promise_result_failure(ctx.clone());
                staking_pool.ops_stake_start_finalize();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec![
                        "[ERR] [STAKE_ACTION_FAILED] ",
                        "[WARN] [STATUS_OFFLINE] StakeActionFailed",
                    ]
                );

                assert!(deserialize_receipts().is_empty());
            }

            #[test]
            fn promise_failure_with_non_zero_staked_balance() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_treasury_deposit();
                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());

                // Act
                ctx.predecessor_account_id = env::current_account_id();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = *pool_balances.total_staked;
                ctx.attached_deposit = 0;
                testing_env_with_promise_result_failure(ctx.clone());
                staking_pool.ops_stake_start_finalize();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec![
                        "[ERR] [STAKE_ACTION_FAILED] ",
                        "[WARN] [STATUS_OFFLINE] StakeActionFailed",
                    ]
                );

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 2);
                {
                    let receipt = &receipts[0];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    match &receipt.actions[0] {
                        Action::Stake(action) => {
                            assert_eq!(action.stake, 0);
                        }
                        _ => panic!("expected stake action"),
                    }
                }
                {
                    let receipt = &receipts[1];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    match &receipt.actions[0] {
                        Action::FunctionCall(action) => {
                            assert_eq!(action.method_name, "ops_stake_stop_finalize");
                        }
                        _ => panic!("expected stake action"),
                    }
                }
            }
        }

        #[cfg(test)]
        mod tests_stop_finalize_callback {
            use super::*;

            #[test]
            fn promise_success() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();

                // Act
                ctx.predecessor_account_id = env::current_account_id();
                testing_env_with_promise_result_success(ctx.clone());
                staking_pool.ops_stake_stop_finalize();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec!["[WARN] [STATUS_OFFLINE] all NEAR has been unstaked"]
                )
            }

            #[test]
            fn promise_failure() {
                // Arrange
                let mut ctx = new_context(ACCOUNT);
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // Act
                ctx.predecessor_account_id = env::current_account_id();
                testing_env_with_promise_result_failure(ctx.clone());
                staking_pool.ops_stake_stop_finalize();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec![
                        "[ERR] [STAKE_ACTION_FAILED] failed to unstake when trying to stop staking pool",
                    ]
                );
            }
        }
    }

    #[cfg(test)]
    mod tests_operator_commands {
        use super::*;

        #[cfg(test)]
        mod tests_start_staking {
            use super::*;

            #[test]
            #[should_panic(expected = "[ERR] [NOT_AUTHORIZED]")]
            fn not_as_operator() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
            }

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn account_not_registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
            }

            #[test]
            fn initial_startup_with_zero_staked() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec!["[INFO] [STATUS_ONLINE] ",]);

                let pool_balances = staking_pool.ops_stake_pool_balances();
                println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                assert_eq!(pool_balances.total_staked, YoctoNear::ZERO);
                assert_eq!(pool_balances.total_unstaked, YoctoNear::ZERO);
                assert_eq!(pool_balances.unstaked_liquidity, YoctoNear::ZERO);
                assert_eq!(pool_balances.treasury_balance, YoctoNear::ZERO);

                assert!(deserialize_receipts().is_empty());
            }

            #[test]
            fn startup_with_non_zero_staked_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let contract_managed_total_balance = State::contract_managed_total_balance();

                let mut account_manager = account_manager();
                let mut staking_pool = staking_pool();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                // assert that there was no net change to contract_managed_total_balance
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                assert_eq!(
                    contract_managed_total_balance,
                    State::contract_managed_total_balance()
                );

                assert_eq!(
                    account_manager
                        .storage_balance_of(to_valid_account_id(ACCOUNT))
                        .unwrap()
                        .available,
                    YoctoNear::ZERO
                );

                // stake
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Promise(_) = staking_pool.ops_stake() {
                    panic!("expected Value");
                }
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                // Act - start staking
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);

                // Assert
                assert!(staking_pool.ops_stake_status().is_online());

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec!["[INFO] [STATUS_ONLINE] ",]);

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 2);
                {
                    let receipt = &receipts[0];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::Stake(action) => {
                            assert_eq!(
                                action.stake,
                                *staking_pool.ops_stake_pool_balances().total_staked
                            );

                            assert_eq!(
                                action.public_key,
                                "1".to_string()
                                    + staking_pool
                                        .ops_stake_public_key()
                                        .to_string()
                                        .split(":")
                                        .last()
                                        .unwrap()
                            );
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
                {
                    let receipt = &receipts[1];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::FunctionCall(action) => {
                            assert_eq!(action.method_name, "ops_stake_start_finalize");
                            assert!(action.args.is_empty());
                            assert_eq!(action.deposit, 0);
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
            }

            #[test]
            fn start_while_already_started() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // stake
                ctx.predecessor_account_id = OWNER.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                if let PromiseOrValue::Promise(_) = staking_pool.ops_stake() {
                    panic!("expected Value");
                }
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                // Act - start staking 2x
                for i in 0..2 {
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = 0;
                    ctx.predecessor_account_id = OWNER.to_string();
                    testing_env!(ctx.clone());
                    staking_pool
                        .ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);

                    let receipts = deserialize_receipts();
                    if i == 1 {
                        assert_eq!(logs, vec!["[INFO] [STATUS_ONLINE] already online",]);
                        assert!(receipts.is_empty());
                    }
                }

                assert!(staking_pool.ops_stake_status().is_online());
            }
        }

        #[cfg(test)]
        mod tests_stop_staking {
            use super::*;

            #[test]
            #[should_panic(expected = "[ERR] [NOT_AUTHORIZED]")]
            fn not_as_operator() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StopStaking);
            }

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn account_not_registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StopStaking);
            }

            #[test]
            fn start_then_stop_with_zero_staked_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // Act - stop staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StopStaking);
                assert!(!staking_pool.ops_stake_status().is_online());

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec!["[WARN] [STATUS_OFFLINE] Stopped",]);

                assert!(deserialize_receipts().is_empty());
            }

            #[test]
            fn start_then_stop_with_nonzero_staked_balance_and_nonzero_locked_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // stake
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = OWNER.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = *staking_pool.ops_stake_pool_balances().total_staked;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StopStaking);
                assert!(!staking_pool.ops_stake_status().is_online());

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec!["[WARN] [STATUS_OFFLINE] Stopped",]);

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 2);
                {
                    let receipt = &receipts[0];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::Stake(action) => {
                            assert_eq!(action.stake, 0);

                            assert_eq!(
                                action.public_key,
                                "1".to_string()
                                    + staking_pool
                                        .ops_stake_public_key()
                                        .to_string()
                                        .split(":")
                                        .last()
                                        .unwrap()
                            );
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
                {
                    let receipt = &receipts[1];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::FunctionCall(action) => {
                            assert_eq!(action.method_name, "ops_stake_stop_finalize");
                            assert!(action.args.is_empty());
                            assert_eq!(action.deposit, 0);
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
            }

            #[test]
            fn start_then_stop_with_nonzero_staked_balance_and_zero_locked_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // stake
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                // Act
                ctx.predecessor_account_id = OWNER.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = 0;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StopStaking);

                // Assert
                assert!(!staking_pool.ops_stake_status().is_online());

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec!["[WARN] [STATUS_OFFLINE] Stopped",]);

                assert!(deserialize_receipts().is_empty());
            }

            #[test]
            fn already_stopped_with_nonzero_locked_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                assert!(!staking_pool.ops_stake_status().is_online());

                // start staking
                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                assert!(staking_pool.ops_stake_status().is_online());

                // stake
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                // stop staking
                ctx.predecessor_account_id = OWNER.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = *staking_pool.ops_stake_pool_balances().total_staked;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StopStaking);
                assert!(!staking_pool.ops_stake_status().is_online());
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(deserialize_receipts().len(), 2);

                // Act
                ctx.predecessor_account_id = OWNER.to_string();
                ctx.account_balance = env::account_balance();
                ctx.account_locked_balance = *staking_pool.ops_stake_pool_balances().total_staked;
                testing_env!(ctx.clone());
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StopStaking);

                // Assert
                assert!(!staking_pool.ops_stake_status().is_online());
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec!["[WARN] [STATUS_OFFLINE] Stopped - but was already offline",]
                );

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 2);
                {
                    let receipt = &receipts[0];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::Stake(action) => {
                            assert_eq!(action.stake, 0);

                            assert_eq!(
                                action.public_key,
                                "1".to_string()
                                    + staking_pool
                                        .ops_stake_public_key()
                                        .to_string()
                                        .split(":")
                                        .last()
                                        .unwrap()
                            );
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
                {
                    let receipt = &receipts[1];
                    assert_eq!(receipt.receiver_id, env::current_account_id());
                    assert_eq!(receipt.actions.len(), 1);
                    match &receipt.actions[0] {
                        Action::FunctionCall(action) => {
                            assert_eq!(action.method_name, "ops_stake_stop_finalize");
                            assert!(action.args.is_empty());
                            assert_eq!(action.deposit, 0);
                        }
                        _ => panic!("expected StakeAction"),
                    }
                }
            }
        }

        #[cfg(test)]
        mod tests_update_public_key {
            use super::*;

            #[test]
            fn update_public_key_when_pool_offline() {
                let pk1: PublicKey = serde_json::from_str(
                    r#""ed25519:AC1pVDXsE8sZiLAqLTDa3sD8DH74U5yUDaYKWeBwwyJj""#,
                )
                .unwrap();

                let pk2: PublicKey = serde_json::from_str(
                    r#""ed25519:AC1pVDXsE8sZiLAqLTDa3sD8DH74U5yUDaYKWeBwwyJp""#,
                )
                .unwrap();

                // Arrange
                let ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();

                testing_env!(ctx.clone());
                staking_pool
                    .ops_stake_operator_command(StakingPoolOperatorCommand::UpdatePublicKey(pk1));
                assert_eq!(staking_pool.ops_stake_public_key(), pk1);

                testing_env!(ctx.clone());
                staking_pool
                    .ops_stake_operator_command(StakingPoolOperatorCommand::UpdatePublicKey(pk2));
                assert_eq!(staking_pool.ops_stake_public_key(), pk2);
            }

            #[test]
            #[should_panic(
                expected = "[ERR] [ILLEGAL_STATE] staking pool must be offline to update the staking public key"
            )]
            fn update_public_key_when_pool_online() {
                let pk1: PublicKey = serde_json::from_str(
                    r#""ed25519:AC1pVDXsE8sZiLAqLTDa3sD8DH74U5yUDaYKWeBwwyJj""#,
                )
                .unwrap();

                // Arrange
                let ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);

                testing_env!(ctx.clone());
                staking_pool
                    .ops_stake_operator_command(StakingPoolOperatorCommand::UpdatePublicKey(pk1));
            }
        }

        #[cfg(test)]
        mod tests_update_staking_fee {
            use super::*;

            #[test]
            fn update_fee() {
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                let mut staking_pool = staking_pool();
                let mut fees = staking_pool.ops_stake_fees();
                fees.staking_fee += 1;
                staking_pool
                    .ops_stake_operator_command(StakingPoolOperatorCommand::UpdateFees(fees));
                assert_eq!(
                    *staking_pool.ops_stake_fees().staking_fee,
                    *fees.staking_fee
                );

                fees.staking_fee = MAX_FEE;
                staking_pool
                    .ops_stake_operator_command(StakingPoolOperatorCommand::UpdateFees(fees));
                assert_eq!(staking_pool.ops_stake_fees().staking_fee, MAX_FEE);
            }

            #[test]
            #[should_panic(expected = "[ERR] [INVALID] max staking fee is 1000 BPS (10%)")]
            fn update_fee_above_max() {
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                let mut staking_pool = staking_pool();
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::UpdateFees(
                    Fees {
                        staking_fee: (*MAX_FEE + 1).into(),
                        earnings_fee: 0.into(),
                    },
                ));
            }

            #[test]
            #[should_panic(
                expected = "[ERR] [INVALID] min fee is 1 BPS (0.01%) for at least 1 fee type"
            )]
            fn update_fee_to_zero() {
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = OWNER.to_string();
                testing_env!(ctx.clone());

                let mut staking_pool = staking_pool();
                staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::UpdateFees(
                    Fees {
                        earnings_fee: 0.into(),
                        staking_fee: 0.into(),
                    },
                ));
            }
        }
    }

    #[cfg(test)]
    mod tests_treasury {
        use super::*;

        #[cfg(test)]
        mod tests_online {
            use super::*;

            #[cfg(test)]
            mod tests_deposit {
                use super::*;

                #[test]
                fn nonzero_attached_deposit() {
                    // Arrange
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());

                    let mut staking_pool = staking_pool();

                    // start staking
                    ctx.predecessor_account_id = OWNER.to_string();
                    testing_env!(ctx.clone());
                    staking_pool
                        .ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                    assert!(staking_pool.ops_stake_status().is_online());

                    let ft_stake = ft_stake();
                    assert_eq!(
                        ft_stake.ft_balance_of(to_valid_account_id(&env::current_account_id())),
                        TokenAmount::ZERO
                    );
                    let state = StakingPoolComponent::state();
                    assert_eq!(state.treasury_balance, YoctoNear::ZERO);

                    // Act
                    {
                        ctx.predecessor_account_id = ACCOUNT.to_string();
                        ctx.attached_deposit = YOCTO;
                        ctx.account_balance = env::account_balance();
                        testing_env!(ctx.clone());
                        if let PromiseOrValue::Value(_) = staking_pool.ops_stake_treasury_deposit()
                        {
                            panic!("expected Promise")
                        }
                    }

                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                    assert_eq!(logs, vec![
                        "[INFO] [STAKE] near_amount=1000000000000000000000000, stake_token_amount=1000000000000000000000000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                        "[INFO] [FT_MINT] account: contract.near, amount: 1000000000000000000000000",
                    ]);

                    assert_eq!(
                        ft_stake.ft_balance_of(to_valid_account_id(&env::current_account_id())),
                        YOCTO.into()
                    );

                    let state = StakingPoolComponent::state();
                    assert_eq!(state.treasury_balance, YOCTO.into());

                    let receipts = deserialize_receipts();
                    assert_eq!(receipts.len(), 2);
                    {
                        let receipt = &receipts[0];
                        assert_eq!(receipt.receiver_id, env::current_account_id());
                        assert_eq!(receipt.actions.len(), 1);
                        match &receipt.actions[0] {
                            Action::Stake(action) => {
                                assert_eq!(
                                    action.stake,
                                    *staking_pool.ops_stake_pool_balances().total_staked
                                );

                                assert_eq!(
                                    action.public_key,
                                    "1".to_string()
                                        + staking_pool
                                            .ops_stake_public_key()
                                            .to_string()
                                            .split(":")
                                            .last()
                                            .unwrap()
                                );
                            }
                            _ => panic!("expected StakeAction"),
                        }
                    }
                    {
                        let receipt = &receipts[1];
                        assert_eq!(receipt.receiver_id, env::current_account_id());
                        assert_eq!(receipt.actions.len(), 1);
                        match &receipt.actions[0] {
                            Action::FunctionCall(action) => {
                                assert_eq!(action.method_name, "ops_stake_finalize");
                                let args: StakeActionCallbackArgs =
                                    serde_json::from_str(&action.args).unwrap();
                                assert_eq!(args.account_id, env::current_account_id());
                                assert_eq!(action.deposit, 0);
                            }
                            _ => panic!("expected StakeAction"),
                        }
                    }

                    // Act - deposit again
                    {
                        ctx.predecessor_account_id = ACCOUNT.to_string();
                        ctx.attached_deposit = YOCTO;
                        ctx.account_balance = env::account_balance();
                        testing_env!(ctx.clone());
                        if let PromiseOrValue::Value(_) = staking_pool.ops_stake_treasury_deposit()
                        {
                            panic!("expected Promise")
                        }
                    }

                    // Assert
                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                    assert_eq!(logs, vec![
                        "[INFO] [STAKE] near_amount=1000000000000000000000000, stake_token_amount=1000000000000000000000000",
                        "[INFO] [FT_MINT] account: contract.near, amount: 1000000000000000000000000",
                    ]);

                    assert_eq!(
                        ft_stake.ft_balance_of(to_valid_account_id(&env::current_account_id())),
                        (2 * YOCTO).into()
                    );

                    let state = StakingPoolComponent::state();
                    assert_eq!(state.treasury_balance, (2 * YOCTO).into());

                    assert_eq!(
                        *staking_pool.ops_stake_pool_balances().total_staked,
                        2 * YOCTO
                    );

                    let receipts = deserialize_receipts();
                    assert_eq!(receipts.len(), 2);
                    {
                        let receipt = &receipts[0];
                        assert_eq!(receipt.receiver_id, env::current_account_id());
                        assert_eq!(receipt.actions.len(), 1);
                        match &receipt.actions[0] {
                            Action::Stake(action) => {
                                assert_eq!(
                                    action.stake,
                                    *staking_pool.ops_stake_pool_balances().total_staked
                                );

                                assert_eq!(
                                    action.public_key,
                                    "1".to_string()
                                        + staking_pool
                                            .ops_stake_public_key()
                                            .to_string()
                                            .split(":")
                                            .last()
                                            .unwrap()
                                );
                            }
                            _ => panic!("expected StakeAction"),
                        }
                    }
                    {
                        let receipt = &receipts[1];
                        assert_eq!(receipt.receiver_id, env::current_account_id());
                        assert_eq!(receipt.actions.len(), 1);
                        match &receipt.actions[0] {
                            Action::FunctionCall(action) => {
                                assert_eq!(action.method_name, "ops_stake_finalize");
                                let args: StakeActionCallbackArgs =
                                    serde_json::from_str(&action.args).unwrap();
                                assert_eq!(args.account_id, env::current_account_id());
                                assert_eq!(action.deposit, 0);
                            }
                            _ => panic!("expected StakeAction"),
                        }
                    }
                }

                #[test]
                #[should_panic(expected = "[ERR] [NEAR_DEPOSIT_REQUIRED] ")]
                fn zero_attached_deposit() {
                    // Arrange
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());

                    let mut staking_pool = staking_pool();

                    // Act
                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = 0;
                    ctx.account_balance = env::account_balance();
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_deposit();
                }
            }

            #[cfg(test)]
            mod tests_distribution {
                use super::*;

                #[test]
                fn nonzero_attached_deposit() {
                    // Arrange
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());

                    let mut staking_pool = staking_pool();

                    // start staking
                    ctx.predecessor_account_id = OWNER.to_string();
                    testing_env!(ctx.clone());
                    staking_pool
                        .ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                    assert!(staking_pool.ops_stake_status().is_online());

                    let ft_stake = ft_stake();
                    assert_eq!(
                        ft_stake.ft_balance_of(to_valid_account_id(&env::current_account_id())),
                        TokenAmount::ZERO
                    );

                    // Act
                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = YOCTO;
                    ctx.account_balance = env::account_balance();
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_distribution();

                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);

                    assert_eq!(
                        ft_stake.ft_balance_of(to_valid_account_id(&env::current_account_id())),
                        TokenAmount::ZERO
                    );

                    let receipts = deserialize_receipts();
                    assert_eq!(receipts.len(), 2);
                    {
                        let receipt = &receipts[0];
                        assert_eq!(receipt.receiver_id, env::current_account_id());
                        assert_eq!(receipt.actions.len(), 1);
                        match &receipt.actions[0] {
                            Action::Stake(action) => {
                                assert_eq!(
                                    action.stake,
                                    *staking_pool.ops_stake_pool_balances().total_staked
                                );

                                assert_eq!(
                                    action.public_key,
                                    "1".to_string()
                                        + staking_pool
                                            .ops_stake_public_key()
                                            .to_string()
                                            .split(":")
                                            .last()
                                            .unwrap()
                                );
                            }
                            _ => panic!("expected StakeAction"),
                        }
                    }
                    {
                        let receipt = &receipts[1];
                        assert_eq!(receipt.receiver_id, env::current_account_id());
                        assert_eq!(receipt.actions.len(), 1);
                        match &receipt.actions[0] {
                            Action::FunctionCall(action) => {
                                assert_eq!(action.method_name, "ops_stake_finalize");
                                let args: StakeActionCallbackArgs =
                                    serde_json::from_str(&action.args).unwrap();
                                assert_eq!(args.account_id, env::current_account_id());
                                assert_eq!(action.deposit, 0);
                            }
                            _ => panic!("expected StakeAction"),
                        }
                    }
                }

                #[test]
                fn zero_attached_deposit_with_earnings_with_zero_total_supply() {
                    // Arrange
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());
                    let mut staking_pool = staking_pool();

                    // start staking
                    ctx.predecessor_account_id = OWNER.to_string();
                    testing_env!(ctx.clone());
                    staking_pool
                        .ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                    assert!(staking_pool.ops_stake_status().is_online());

                    // Act
                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = 0;
                    ctx.account_balance = env::account_balance() + YOCTO;
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_distribution();

                    // Assert - when STAKE total supply is zero, then earnings are not distribute and
                    // we expect no stake action
                    assert!(test_utils::get_logs().is_empty());
                    assert!(deserialize_receipts().is_empty());
                }

                #[test]
                fn zero_attached_deposit_with_earnings_with_nonzero_total_supply() {
                    // Arrange
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());
                    let mut staking_pool = staking_pool();

                    // start staking
                    ctx.predecessor_account_id = OWNER.to_string();
                    testing_env!(ctx.clone());
                    staking_pool
                        .ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                    assert!(staking_pool.ops_stake_status().is_online());

                    // register account
                    let mut account_manager = account_manager();
                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = YOCTO;
                    ctx.account_balance = env::account_balance();
                    testing_env!(ctx.clone());
                    account_manager.storage_deposit(None, None);

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = YOCTO;
                    ctx.account_balance = env::account_balance();
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake();

                    // Act
                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = 0;
                    ctx.account_balance = env::account_balance() + YOCTO;
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_distribution();

                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                    assert_eq!(logs, vec!["[INFO] [EARNINGS] 1000000000000000000000000",]);

                    let receipts = deserialize_receipts();
                    assert_eq!(receipts.len(), 2);
                    {
                        let receipt = &receipts[0];
                        assert_eq!(receipt.receiver_id, env::current_account_id());
                        match &receipt.actions[0] {
                            Action::Stake(action) => {
                                assert_eq!(action.stake, *State::total_staked_balance());
                            }
                            _ => panic!("expected Stake"),
                        }
                    }
                    {
                        let receipt = &receipts[1];
                        assert_eq!(receipt.receiver_id, env::current_account_id());
                        match &receipt.actions[0] {
                            Action::FunctionCall(action) => {
                                assert_eq!(action.method_name, "ops_stake_finalize");
                                let args: StakeActionCallbackArgs =
                                    serde_json::from_str(&action.args).unwrap();
                                assert_eq!(args.account_id, env::current_account_id());
                            }
                            _ => panic!("expected FunctionCall"),
                        }
                    }
                }

                #[test]
                fn zero_attached_deposit_with_zero_earnings_with_nonzero_total_supply() {
                    // Arrange
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());
                    let mut staking_pool = staking_pool();

                    // start staking
                    ctx.predecessor_account_id = OWNER.to_string();
                    testing_env!(ctx.clone());
                    staking_pool
                        .ops_stake_operator_command(StakingPoolOperatorCommand::StartStaking);
                    assert!(staking_pool.ops_stake_status().is_online());

                    // register account
                    let mut account_manager = account_manager();
                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = YOCTO;
                    ctx.account_balance = env::account_balance();
                    testing_env!(ctx.clone());
                    account_manager.storage_deposit(None, None);

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = YOCTO;
                    ctx.account_balance = env::account_balance();
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake();

                    // Act
                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = 0;
                    ctx.account_balance = env::account_balance();
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_distribution();

                    assert!(test_utils::get_logs().is_empty());
                    assert!(deserialize_receipts().is_empty());
                }
            }

            #[cfg(test)]
            mod tests_transfer_to_owner {
                use super::*;

                #[test]
                #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
                fn not_registered() {
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());
                    let mut staking_pool = staking_pool();

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_transfer_to_owner(None);
                }

                #[test]
                #[should_panic(expected = "[ERR] [NOT_AUTHORIZED]")]
                fn not_authorized() {
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());
                    let mut staking_pool = staking_pool();
                    let mut account_manager = account_manager();

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    account_manager.storage_deposit(None, None);

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_transfer_to_owner(None);
                }

                #[test]
                fn as_treasurer_transfer_all_with_empty_treasury() {
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());
                    let mut staking_pool = staking_pool();
                    let mut account_manager = account_manager();

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    account_manager.storage_deposit(None, None);

                    ctx.predecessor_account_id = OWNER.to_string();
                    ctx.attached_deposit = 0;
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_grant_treasurer(to_valid_account_id(ACCOUNT));

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_transfer_to_owner(None);
                }

                #[test]
                fn as_owner_transfer_all_with_empty_treasury() {
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());
                    let mut staking_pool = staking_pool();

                    ctx.predecessor_account_id = OWNER.to_string();
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_transfer_to_owner(None);
                }

                #[test]
                fn as_treasurer_transfer_all() {
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());
                    let mut staking_pool = staking_pool();
                    let mut account_manager = account_manager();

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    account_manager.storage_deposit(None, None);

                    ctx.predecessor_account_id = OWNER.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = 0;
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_grant_treasurer(to_valid_account_id(ACCOUNT));

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_deposit();

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = 0;
                    testing_env!(ctx.clone());
                    let pool_balances = staking_pool.ops_stake_pool_balances();
                    println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                    assert_eq!(pool_balances.treasury_balance, YOCTO.into());
                    let owner_balance = staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .unwrap();
                    assert!(owner_balance.staked.is_none());

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_transfer_to_owner(None);

                    let pool_balances = staking_pool.ops_stake_pool_balances();
                    println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                    assert_eq!(pool_balances.treasury_balance, YoctoNear::ZERO);
                    let owner_balance = staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .unwrap();
                    assert_eq!(owner_balance.staked.as_ref().unwrap().stake, YOCTO.into());
                }

                #[test]
                fn as_treasurer_transfer_some() {
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());
                    let mut staking_pool = staking_pool();
                    let mut account_manager = account_manager();

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    account_manager.storage_deposit(None, None);

                    ctx.predecessor_account_id = OWNER.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = 0;
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_grant_treasurer(to_valid_account_id(ACCOUNT));

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_deposit();

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = 0;
                    testing_env!(ctx.clone());
                    let pool_balances = staking_pool.ops_stake_pool_balances();
                    println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                    assert_eq!(pool_balances.treasury_balance, YOCTO.into());
                    let owner_balance = staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .unwrap();
                    assert!(owner_balance.staked.is_none());

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_transfer_to_owner(Some((YOCTO / 4).into()));

                    let pool_balances = staking_pool.ops_stake_pool_balances();
                    println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                    assert_eq!(pool_balances.treasury_balance, (YOCTO * 3 / 4).into());
                    let owner_balance = staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .unwrap();
                    assert_eq!(
                        owner_balance.staked.as_ref().unwrap().stake,
                        (YOCTO / 4).into()
                    );
                }

                #[test]
                fn as_owner_transfer_all() {
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());
                    let mut staking_pool = staking_pool();
                    let mut account_manager = account_manager();

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    account_manager.storage_deposit(None, None);

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_deposit();

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = 0;
                    testing_env!(ctx.clone());
                    let pool_balances = staking_pool.ops_stake_pool_balances();
                    println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                    assert_eq!(pool_balances.treasury_balance, YOCTO.into());
                    let owner_balance = staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .unwrap();
                    assert!(owner_balance.staked.is_none());

                    ctx.predecessor_account_id = OWNER.to_string();
                    ctx.account_balance = env::account_balance();
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_transfer_to_owner(None);

                    let pool_balances = staking_pool.ops_stake_pool_balances();
                    println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                    assert_eq!(pool_balances.treasury_balance, YoctoNear::ZERO);
                    let owner_balance = staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .unwrap();
                    assert_eq!(owner_balance.staked.as_ref().unwrap().stake, YOCTO.into());
                }

                #[test]
                fn as_owner_transfer_some() {
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());
                    let mut staking_pool = staking_pool();
                    let mut account_manager = account_manager();

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    account_manager.storage_deposit(None, None);

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_deposit();

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = 0;
                    testing_env!(ctx.clone());
                    let pool_balances = staking_pool.ops_stake_pool_balances();
                    println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                    assert_eq!(pool_balances.treasury_balance, YOCTO.into());
                    let owner_balance = staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .unwrap();
                    assert!(owner_balance.staked.is_none());

                    ctx.predecessor_account_id = OWNER.to_string();
                    ctx.account_balance = env::account_balance();
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_transfer_to_owner(Some((YOCTO / 4).into()));

                    let pool_balances = staking_pool.ops_stake_pool_balances();
                    println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                    assert_eq!(pool_balances.treasury_balance, (YOCTO * 3 / 4).into());
                    let owner_balance = staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .unwrap();
                    assert_eq!(
                        owner_balance.staked.as_ref().unwrap().stake,
                        (YOCTO / 4).into()
                    );
                }

                #[test]
                #[should_panic(expected = "[ERR] [INSUFFICIENT_FUNDS]")]
                fn as_owner_transfer_some_insufficient_funds() {
                    let mut ctx = new_context(OWNER);
                    testing_env!(ctx.clone());

                    deploy_stake_contract(staking_public_key());
                    let mut staking_pool = staking_pool();
                    let mut account_manager = account_manager();

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    account_manager.storage_deposit(None, None);

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_deposit();

                    ctx.predecessor_account_id = ACCOUNT.to_string();
                    ctx.account_balance = env::account_balance();
                    ctx.attached_deposit = 0;
                    testing_env!(ctx.clone());
                    let pool_balances = staking_pool.ops_stake_pool_balances();
                    println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
                    assert_eq!(pool_balances.treasury_balance, YOCTO.into());
                    let owner_balance = staking_pool
                        .ops_stake_balance(to_valid_account_id(OWNER))
                        .unwrap();
                    assert!(owner_balance.staked.is_none());

                    ctx.predecessor_account_id = OWNER.to_string();
                    ctx.account_balance = env::account_balance();
                    testing_env!(ctx.clone());
                    staking_pool.ops_stake_treasury_transfer_to_owner(Some((YOCTO + 1).into()));
                }
            }

            #[test]
            fn treasurer_access_control() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());
                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = OWNER.to_string();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                assert!(!staking_pool.ops_stake_is_treasurer(to_valid_account_id(ACCOUNT)));

                staking_pool.ops_stake_grant_treasurer(to_valid_account_id(ACCOUNT));
                assert!(staking_pool.ops_stake_is_treasurer(to_valid_account_id(ACCOUNT)));

                staking_pool.ops_stake_revoke_treasurer(to_valid_account_id(ACCOUNT));
                assert!(!staking_pool.ops_stake_is_treasurer(to_valid_account_id(ACCOUNT)));
            }
        }
    }

    #[cfg(test)]
    mod tests_ft_transfer {
        use super::*;

        #[test]
        fn transfer_with_earnings_received() {
            // Arrange
            let mut ctx = new_context(OWNER);
            testing_env!(ctx.clone());

            deploy_stake_contract(staking_public_key());

            let mut staking_pool = staking_pool();
            let mut account_manager = account_manager();

            // register account
            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = YOCTO;
            testing_env!(ctx.clone());
            account_manager.storage_deposit(None, Some(true));

            // register account
            ctx.predecessor_account_id = "alice".to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = YOCTO;
            testing_env!(ctx.clone());
            account_manager.storage_deposit(None, Some(true));

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = YOCTO;
            testing_env!(ctx.clone());
            let initial_balance = if let PromiseOrValue::Value(balance) = staking_pool.ops_stake() {
                balance
            } else {
                panic!("expected vale")
            };

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance() + YOCTO;
            ctx.attached_deposit = 1;
            testing_env!(ctx.clone());
            let transfer_amount =
                staking_pool.ops_stake_transfer(to_valid_account_id("alice"), YOCTO.into(), None);

            let logs = test_utils::get_logs();
            println!("{:#?}", logs);
            assert_eq!(
                logs,
                vec![
                    "[INFO] [EARNINGS] 1000000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                ]
            );

            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = 0;
            ctx.is_view = true;
            testing_env!(ctx.clone());

            println!("transfer_amount={}", transfer_amount);
            assert_eq!(
                staking_pool.ops_stake_token_value(Some(transfer_amount)),
                YOCTO.into()
            );

            assert_eq!(
                staking_pool
                    .stake_token
                    .ft_balance_of(to_valid_account_id("alice")),
                transfer_amount
            );

            assert_eq!(
                staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .as_ref()
                    .unwrap()
                    .staked
                    .as_ref()
                    .unwrap()
                    .stake
                    + transfer_amount,
                initial_balance.staked.as_ref().unwrap().stake
            )
        }

        #[test]
        fn ft_transfer() {
            // Arrange
            let mut ctx = new_context(OWNER);
            testing_env!(ctx.clone());

            deploy_stake_contract(staking_public_key());

            let mut staking_pool = staking_pool();
            let mut account_manager = account_manager();
            let mut ft_stake = ft_stake();

            // register account
            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = YOCTO;
            testing_env!(ctx.clone());
            account_manager.storage_deposit(None, Some(true));

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = 10 * YOCTO;
            testing_env!(ctx.clone());
            staking_pool.ops_stake();

            // Act - transfer 1 STAKE to treasury
            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = 1;
            testing_env!(ctx.clone());
            ft_stake.ft_transfer(
                to_valid_account_id(env::current_account_id().as_str()),
                YOCTO.into(),
                None,
            );

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = 0;
            testing_env!(ctx.clone());

            let pool_balances = staking_pool.ops_stake_pool_balances();
            println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
            assert_eq!(pool_balances.treasury_balance, YoctoNear::ZERO);

            let stake_account_balances = staking_pool
                .ops_stake_balance(to_valid_account_id(env::current_account_id().as_str()))
                .unwrap();
            assert_eq!(
                stake_account_balances.staked.as_ref().unwrap().stake,
                YOCTO.into()
            );

            // process earnings
            staking_pool.ops_stake_token_value_with_earnings(None);

            // Assert - earnings have now been deposited into the treasury for the dividend
            let pool_balances = staking_pool.ops_stake_pool_balances();
            println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
            assert_eq!(
                pool_balances.treasury_balance,
                stake_account_balances.staked.as_ref().unwrap().near_value
            );

            // Act - transfer another 1 STAKE to treasury
            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = 1;
            testing_env!(ctx.clone());
            ft_stake.ft_transfer(
                to_valid_account_id(env::current_account_id().as_str()),
                YOCTO.into(),
                None,
            );

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = 0;
            testing_env!(ctx.clone());

            let pool_balances = staking_pool.ops_stake_pool_balances();
            println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());

            // process earnings
            staking_pool.ops_stake_token_value_with_earnings(None);

            // Assert - 2nd transfer was treated as a distribution, i.e., transferred STAKE was burned
            let logs = test_utils::get_logs();
            println!("{:#?}", logs);
            assert_eq!(logs, vec![
                "[INFO] [EARNINGS] 1",
                "[INFO] [FT_BURN] account: contract.near, amount: 999999999999999999999999",
                "[INFO] [TREASURY_DIVIDEND] 1000000000000000000000000 yoctoNEAR / 999999999999999999999999 yoctoSTAKE",
            ]);

            let pool_balances = staking_pool.ops_stake_pool_balances();
            println!("{}", serde_json::to_string_pretty(&pool_balances).unwrap());
            assert_eq!(
                pool_balances,
                serde_json::from_str(
                    r#"{
  "total_staked": "10000000000000000000000002",
  "total_stake_supply": "9000000000000000000000001",
  "total_unstaked": "0",
  "unstaked_liquidity": "0",
  "treasury_balance": "1111111111111111111111112",
  "current_contract_managed_total_balance": "13172980000000000000000002",
  "last_contract_managed_total_balance": "13172980000000000000000002",
  "earnings": "0"
}"#
                )
                .unwrap()
            );

            let treasury_stake_account = staking_pool
                .ops_stake_balance(to_valid_account_id(env::current_account_id().as_str()))
                .unwrap();
            assert_eq!(
                treasury_stake_account,
                serde_json::from_str(
                    r#"{
  "storage_balance": {
    "total": "3930000000000000000000",
    "available": "0"
  },
  "staked": {
    "stake": "1000000000000000000000001",
    "near_value": "1111111111111111111111112"
  },
  "unstaked": null
}"#
                )
                .unwrap()
            );
        }

        #[test]
        fn transfer_call_with_earnings_received() {
            // Arrange
            let mut ctx = new_context(OWNER);
            testing_env!(ctx.clone());

            deploy_stake_contract(staking_public_key());

            let mut staking_pool = staking_pool();
            let mut account_manager = account_manager();

            // register account
            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = YOCTO;
            testing_env!(ctx.clone());
            account_manager.storage_deposit(None, Some(true));

            // register account
            ctx.predecessor_account_id = "alice".to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = YOCTO;
            testing_env!(ctx.clone());
            account_manager.storage_deposit(None, Some(true));

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = YOCTO;
            testing_env!(ctx.clone());
            let initial_balance = if let PromiseOrValue::Value(balance) = staking_pool.ops_stake() {
                balance
            } else {
                panic!("expected vale")
            };

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance() + YOCTO;
            ctx.attached_deposit = 1;
            testing_env!(ctx.clone());
            staking_pool.ops_stake_transfer_call(
                to_valid_account_id("alice"),
                YOCTO.into(),
                None,
                TransferCallMessage("".to_string()),
            );

            let logs = test_utils::get_logs();
            println!("{:#?}", logs);
            assert_eq!(
                logs,
                vec![
                    "[INFO] [EARNINGS] 1000000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                ]
            );

            let receipts = deserialize_receipts();
            assert_eq!(receipts.len(), 2);

            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = 0;
            ctx.is_view = true;
            testing_env!(ctx.clone());

            let transfer_amount = TokenAmount::from(YOCTO / 2);
            assert_eq!(
                staking_pool.ops_stake_token_value(Some(transfer_amount)),
                YOCTO.into()
            );

            assert_eq!(
                staking_pool
                    .stake_token
                    .ft_balance_of(to_valid_account_id("alice")),
                transfer_amount
            );

            assert_eq!(
                staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .as_ref()
                    .unwrap()
                    .staked
                    .as_ref()
                    .unwrap()
                    .stake
                    + transfer_amount,
                initial_balance.staked.as_ref().unwrap().stake
            );
        }

        #[test]
        fn ft_on_transfer() {
            // Arrange
            let mut ctx = new_context(OWNER);
            testing_env!(ctx.clone());

            deploy_stake_contract(staking_public_key());

            let mut staking_pool = staking_pool();
            let mut account_manager = account_manager();

            // register account
            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = YOCTO;
            testing_env!(ctx.clone());
            account_manager.storage_deposit(None, Some(true));

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = 10 * YOCTO;
            testing_env!(ctx.clone());
            staking_pool.ops_stake();

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = 1;
            testing_env!(ctx.clone());
            let msg = TransferCallMessage("".to_string());
            staking_pool.ops_stake_transfer_call(
                to_valid_account_id(&env::current_account_id()),
                YOCTO.into(),
                None,
                msg.clone(),
            );

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = 0;
            testing_env!(ctx.clone());
            assert_eq!(
                staking_pool.ops_stake_pool_balances().treasury_balance,
                YoctoNear::ZERO
            );

            ctx.predecessor_account_id = env::current_account_id();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = 0;
            testing_env!(ctx.clone());
            if let PromiseOrValue::Value(refund) =
                staking_pool.ft_on_transfer(to_valid_account_id(ACCOUNT), YOCTO.into(), msg)
            {
                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec![
                        "[INFO] [EARNINGS] 1",
                        "[INFO] [TREASURY_DEPOSIT] treasury balance = 1000000000000000000000000",
                    ]
                );

                assert_eq!(refund, TokenAmount::ZERO);

                assert_eq!(
                    staking_pool.ops_stake_pool_balances().treasury_balance,
                    YOCTO.into()
                );
            } else {
                panic!("expected value");
            }
        }

        #[test]
        #[should_panic(
            expected = "[ERR] [NOT_AUTHORIZED] this method can only be invoked by the STAKE pool contract internally"
        )]
        fn ft_on_transfer_not_invoked_by_self() {
            // Arrange
            let ctx = new_context(OWNER);
            testing_env!(ctx.clone());

            deploy_stake_contract(staking_public_key());

            let mut staking_pool = staking_pool();
            staking_pool.ft_on_transfer(
                to_valid_account_id(ACCOUNT),
                YOCTO.into(),
                TransferCallMessage("".to_string()),
            );
        }
    }

    #[cfg(test)]
    mod tests_fees {
        use super::*;

        #[test]
        fn with_zero_staking_fee_nonzero_earnings_fee_with_zero_stake_supply() {
            let mut ctx = new_context(OWNER);
            testing_env!(ctx.clone());

            deploy_stake_contract(staking_public_key());
            let mut staking_pool = staking_pool();

            staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::UpdateFees(Fees {
                staking_fee: 0.into(),
                earnings_fee: 100.into(),
            }));

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance() + YOCTO;
            ctx.attached_deposit = 0;
            testing_env!(ctx.clone());
            let stake_token_value = staking_pool.ops_stake_token_value_with_earnings(None);

            // Assert - no earnings are distributed
            assert!(test_utils::get_logs().is_empty());

            assert_eq!(stake_token_value, YOCTO.into());
        }

        #[test]
        fn with_zero_staking_fee_nonzero_earnings_fee_with_some_stake_supply() {
            let mut ctx = new_context(OWNER);
            testing_env!(ctx.clone());

            deploy_stake_contract(staking_public_key());
            let mut staking_pool = staking_pool();
            let mut account_manager = account_manager();

            staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::UpdateFees(Fees {
                staking_fee: 0.into(),
                earnings_fee: 100.into(),
            }));

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = YOCTO;
            testing_env!(ctx.clone());
            account_manager.storage_deposit(None, None);

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            ctx.attached_deposit = 0;
            testing_env!(ctx.clone());
            staking_pool.ops_stake();

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance() + YOCTO;
            testing_env!(ctx.clone());
            let stake_token_value = staking_pool.ops_stake_token_value_with_earnings(None);
            println!("stake_token_value = {}", stake_token_value);

            // Assert
            let logs = test_utils::get_logs();
            println!("{:#?}", logs);
            assert_eq!(
                logs,
                vec![
                    "[INFO] [EARNINGS] 1000000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: owner, amount: 5015281435196141122921",
                ]
            );

            ctx.predecessor_account_id = ACCOUNT.to_string();
            ctx.account_balance = env::account_balance();
            testing_env!(ctx.clone());
            let owner_earnings = staking_pool.ops_stake_fees().earnings_fee * YOCTO;
            let (_owner_earnings, remainder) = staking_pool.near_to_stake(owner_earnings);
            assert_eq!(
                staking_pool
                    .ops_stake_balance(to_valid_account_id(OWNER))
                    .as_ref()
                    .unwrap()
                    .staked
                    .as_ref()
                    .unwrap()
                    .near_value,
                owner_earnings - remainder
            );
        }

        #[test]
        #[should_panic(expected = "[ERR] [INVALID] max earnings fee is 1000 BPS (10%)")]
        fn update_to_above_max_earnings_fee() {
            let ctx = new_context(OWNER);
            testing_env!(ctx.clone());

            deploy_stake_contract(staking_public_key());
            let mut staking_pool = staking_pool();

            staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::UpdateFees(Fees {
                staking_fee: 0.into(),
                earnings_fee: MAX_FEE + 1,
            }));
        }

        #[test]
        #[should_panic(
            expected = "[ERR] [INVALID] min fee is 1 BPS (0.01%) for at least 1 fee type"
        )]
        fn update_to_zero_fees() {
            let ctx = new_context(OWNER);
            testing_env!(ctx.clone());

            deploy_stake_contract(staking_public_key());
            let mut staking_pool = staking_pool();

            staking_pool.ops_stake_operator_command(StakingPoolOperatorCommand::UpdateFees(Fees {
                staking_fee: 0.into(),
                earnings_fee: 0.into(),
            }));
        }
    }

    #[cfg(test)]
    mod test_near_staking_pool_interface {
        use super::*;

        #[cfg(test)]
        mod tests_get_account_staked_balance {
            use super::*;

            #[test]
            fn not_registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                let staking_pool = staking_pool();
                assert_eq!(
                    staking_pool.get_account_staked_balance(to_valid_account_id(ACCOUNT)),
                    YoctoNear::ZERO,
                );
            }

            #[test]
            fn with_zero_staked() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                assert_eq!(
                    staking_pool.get_account_staked_balance(to_valid_account_id(ACCOUNT)),
                    YoctoNear::ZERO,
                );
            }

            #[test]
            fn with_nonzero_staked() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                let balance = if let PromiseOrValue::Value(balance) = staking_pool.ops_stake() {
                    balance
                } else {
                    panic!("expected value");
                };

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                assert_eq!(
                    staking_pool.get_account_staked_balance(to_valid_account_id(ACCOUNT)),
                    balance.staked.as_ref().unwrap().near_value,
                );
            }
        }

        #[cfg(test)]
        mod tests_get_account_unstaked_balance {
            use super::*;

            #[test]
            fn not_registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                let staking_pool = staking_pool();
                assert_eq!(
                    staking_pool.get_account_unstaked_balance(to_valid_account_id(ACCOUNT)),
                    YoctoNear::ZERO,
                );
            }

            #[test]
            fn with_zero_unstaked_zero_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                assert_eq!(
                    staking_pool.get_account_unstaked_balance(to_valid_account_id(ACCOUNT)),
                    YoctoNear::ZERO,
                );
            }

            #[test]
            fn with_zero_unstaked_with_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                assert_eq!(
                    staking_pool.get_account_unstaked_balance(to_valid_account_id(ACCOUNT)),
                    account_manager
                        .storage_balance_of(to_valid_account_id(ACCOUNT))
                        .unwrap()
                        .available
                );
            }

            #[test]
            fn with_nonzero_unstaked() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let balance = if let PromiseOrValue::Value(balance) = staking_pool.ops_unstake(None)
                {
                    balance
                } else {
                    panic!("expected value");
                };

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                assert_eq!(
                    staking_pool.get_account_unstaked_balance(to_valid_account_id(ACCOUNT)),
                    balance.unstaked.unwrap().total,
                );
            }
        }

        #[cfg(test)]
        mod tests_is_account_unstaked_balance_available {
            use super::*;

            #[test]
            fn not_registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                let staking_pool = staking_pool();
                assert!(staking_pool
                    .is_account_unstaked_balance_available(to_valid_account_id(ACCOUNT)),);
            }

            #[test]
            fn with_zero_unstaked_zero_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                assert!(staking_pool
                    .is_account_unstaked_balance_available(to_valid_account_id(ACCOUNT)),);
            }

            #[test]
            fn with_zero_unstaked_with_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                assert!(staking_pool
                    .is_account_unstaked_balance_available(to_valid_account_id(ACCOUNT)),);
            }

            #[test]
            fn with_nonzero_unstaked() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                staking_pool.ops_unstake(None);

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                assert!(!staking_pool
                    .is_account_unstaked_balance_available(to_valid_account_id(ACCOUNT)),);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                assert!(staking_pool
                    .is_account_unstaked_balance_available(to_valid_account_id(ACCOUNT)),);
            }
        }

        #[cfg(test)]
        mod tests_get_account_total_balance {
            use super::*;
            use oysterpack_smart_near::near_sdk::PromiseOrValue;

            #[test]
            fn not_registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                let staking_pool = staking_pool();
                assert_eq!(
                    staking_pool.get_account_total_balance(to_valid_account_id(ACCOUNT)),
                    YoctoNear::ZERO
                );
            }

            #[test]
            fn with_zero_unstaked_zero_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                assert_eq!(
                    staking_pool.get_account_total_balance(to_valid_account_id(ACCOUNT)),
                    YoctoNear::ZERO
                );
            }

            #[test]
            fn with_zero_unstaked_with_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                assert_eq!(
                    staking_pool.get_account_total_balance(to_valid_account_id(ACCOUNT)),
                    account_manager
                        .storage_balance_of(to_valid_account_id(ACCOUNT))
                        .unwrap()
                        .available
                );
            }

            #[test]
            fn with_nonzero_balances() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let balance = if let PromiseOrValue::Value(balance) =
                    staking_pool.ops_unstake(Some(1000.into()))
                {
                    balance
                } else {
                    panic!("expected value");
                };

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                assert_eq!(
                    staking_pool.get_account_total_balance(to_valid_account_id(ACCOUNT)),
                    balance.staked.as_ref().unwrap().near_value
                        + balance.unstaked.as_ref().unwrap().total
                        + balance.storage_balance.available
                );

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                assert_eq!(
                    staking_pool.get_account_total_balance(to_valid_account_id(ACCOUNT)),
                    balance.staked.unwrap().near_value
                        + balance.unstaked.unwrap().total
                        + balance.storage_balance.available
                );
            }
        }

        #[cfg(test)]
        mod tests_get_account {
            use super::*;
            use oysterpack_smart_near::near_sdk::PromiseOrValue;

            #[test]
            fn not_registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                let staking_pool = staking_pool();
                assert_eq!(
                    staking_pool.get_account(to_valid_account_id(ACCOUNT)),
                    NearStakingPoolAccount {
                        account_id: ACCOUNT.to_string(),
                        unstaked_balance: YoctoNear::ZERO,
                        staked_balance: YoctoNear::ZERO,
                        can_withdraw: true
                    }
                );
            }

            #[test]
            fn with_zero_unstaked_zero_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                assert_eq!(
                    staking_pool.get_account(to_valid_account_id(ACCOUNT)),
                    NearStakingPoolAccount {
                        account_id: ACCOUNT.to_string(),
                        unstaked_balance: YoctoNear::ZERO,
                        staked_balance: YoctoNear::ZERO,
                        can_withdraw: true
                    }
                );
            }

            #[test]
            fn with_zero_unstaked_with_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                assert_eq!(
                    staking_pool.get_account(to_valid_account_id(ACCOUNT)),
                    NearStakingPoolAccount {
                        account_id: ACCOUNT.to_string(),
                        unstaked_balance: account_manager
                            .storage_balance_of(to_valid_account_id(ACCOUNT))
                            .unwrap()
                            .available,
                        staked_balance: YoctoNear::ZERO,
                        can_withdraw: true
                    }
                );
            }

            #[test]
            fn with_nonzero_balances() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let balance = if let PromiseOrValue::Value(balance) =
                    staking_pool.ops_unstake(Some(1000.into()))
                {
                    balance
                } else {
                    panic!("expected value");
                };

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                assert_eq!(
                    staking_pool.get_account(to_valid_account_id(ACCOUNT)),
                    NearStakingPoolAccount {
                        account_id: ACCOUNT.to_string(),
                        unstaked_balance: balance.unstaked.as_ref().unwrap().total
                            + balance.storage_balance.available,
                        staked_balance: balance.staked.as_ref().unwrap().near_value,
                        can_withdraw: false
                    }
                );

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                assert_eq!(
                    staking_pool.get_account(to_valid_account_id(ACCOUNT)),
                    NearStakingPoolAccount {
                        account_id: ACCOUNT.to_string(),
                        unstaked_balance: balance.unstaked.as_ref().unwrap().total
                            + balance.storage_balance.available,
                        staked_balance: balance.staked.as_ref().unwrap().near_value,
                        can_withdraw: true
                    }
                );
            }
        }

        #[cfg(test)]
        mod tests_deposit {
            use super::*;

            #[test]
            fn not_registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                let mut staking_pool = staking_pool();
                staking_pool.deposit();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(97)",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Registered(StorageBalance { total: YoctoNear(1000000000000000000000000), available: YoctoNear(996070000000000000000000) })",
                ]);

                let account_manager = account_manager();
                assert!(account_manager
                    .storage_balance_of(to_valid_account_id(ACCOUNT))
                    .is_some());
            }
        }

        #[cfg(test)]
        mod tests_deposit_and_stake {
            use super::*;

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn not_registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                let mut staking_pool = staking_pool();
                staking_pool.deposit_and_stake();
            }

            #[test]
            fn registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                let mut staking_pool = staking_pool();
                staking_pool.deposit();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(97)",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Registered(StorageBalance { total: YoctoNear(1000000000000000000000000), available: YoctoNear(996070000000000000000000) })",
                ]);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.deposit_and_stake();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(logs, vec![
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] Withdrawal(YoctoNear(996070000000000000000000))",
                    "[INFO] [STAKE] near_amount=1996070000000000000000000, stake_token_amount=1996070000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: bob, amount: 1996070000000000000000000",
                    "[INFO] [FT_BURN] account: bob, amount: 15968560000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                    "[INFO] [FT_MINT] account: owner, amount: 15968560000000000000000",
                    "[WARN] [STATUS_OFFLINE] ",
                ]);
            }
        }

        #[cfg(test)]
        mod tests_withdraw {
            use super::*;

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn not_registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                let mut staking_pool = staking_pool();
                staking_pool.withdraw(1000.into());
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_FUNDS]")]
            fn with_zero_unstaked_zero_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                staking_pool.withdraw(1000.into());
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_FUNDS]")]
            fn with_zero_unstaked_with_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                staking_pool.withdraw(1000.into());
            }

            #[test]
            fn with_nonzero_balances() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                staking_pool.ops_unstake(Some(1000.into()));

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                staking_pool.withdraw(1000.into());

                assert!(staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap()
                    .unstaked
                    .is_none());

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 1);
            }
        }

        #[cfg(test)]
        mod tests_withdraw_all {
            use super::*;

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn not_registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                let mut staking_pool = staking_pool();
                staking_pool.withdraw_all();
            }

            #[test]
            fn with_zero_unstaked_zero_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                staking_pool.withdraw_all();

                let receipts = deserialize_receipts();
                assert!(receipts.is_empty());
            }

            #[test]
            fn with_zero_unstaked_with_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                staking_pool.withdraw_all();

                let receipts = deserialize_receipts();
                assert!(receipts.is_empty());
            }

            #[test]
            fn with_nonzero_balances() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut staking_pool = staking_pool();
                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool.ops_stake();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                staking_pool.ops_unstake(Some(1000.into()));

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                staking_pool.withdraw_all();

                assert!(staking_pool
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap()
                    .unstaked
                    .is_none());

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                let receipts = deserialize_receipts();
                assert_eq!(receipts.len(), 1);
            }
        }

        #[cfg(test)]
        mod tests_stake {
            use super::*;

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn not_registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                let mut staking_pool: Box<dyn NearStakingPool> = Box::new(staking_pool());
                staking_pool.stake(1000.into());
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_FUNDS]")]
            fn with_zero_unstaked_zero_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                let mut staking_pool: Box<dyn NearStakingPool> = Box::new(staking_pool());
                staking_pool.stake(1000.into());
            }

            #[test]
            fn with_zero_unstaked_with_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                let mut staking_pool: Box<dyn NearStakingPool> = Box::new(staking_pool());
                staking_pool.stake(1000.into());

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec![
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] Withdrawal(YoctoNear(1000))",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(184)",
                        "[INFO] [STAKE] near_amount=1000, stake_token_amount=1000",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                        "[INFO] [FT_MINT] account: bob, amount: 1000",
                        "[INFO] [FT_BURN] account: bob, amount: 8",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(104)",
                        "[INFO] [FT_MINT] account: owner, amount: 8",
                        "[WARN] [STATUS_OFFLINE] ",
                    ]
                );
            }

            #[test]
            fn with_nonzero_balances() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool().ops_stake();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                staking_pool().ops_unstake(Some(1000.into()));

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                let mut pool: Box<dyn NearStakingPool> = Box::new(staking_pool());
                pool.stake(1000.into());

                assert!(staking_pool()
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap()
                    .unstaked
                    .is_none());

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
            }
        }

        #[cfg(test)]
        mod tests_unstake {
            use super::*;

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn not_registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                let mut staking_pool: Box<dyn NearStakingPool> = Box::new(staking_pool());
                staking_pool.unstake(1000.into());
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_FUNDS]")]
            fn with_zero_unstaked_zero_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                let mut staking_pool: Box<dyn NearStakingPool> = Box::new(staking_pool());
                staking_pool.unstake(1000.into());
            }

            #[test]
            fn with_zero_unstaked_with_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let mut staking_pool: Box<dyn NearStakingPool> = Box::new(staking_pool());
                staking_pool.stake(1000.into());

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                staking_pool.unstake(500.into());

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec![
                        "[INFO] [UNSTAKE] near_amount=500, stake_token_amount=500",
                        "[INFO] [FT_BURN] account: bob, amount: 500",
                        "[WARN] [STATUS_OFFLINE] ",
                    ]
                );
            }

            #[test]
            fn with_nonzero_balances() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool().ops_stake();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                staking_pool().ops_unstake(Some(1000.into()));

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.epoch_height = env::epoch_height() + 4;
                testing_env!(ctx.clone());
                let mut pool: Box<dyn NearStakingPool> = Box::new(staking_pool());
                pool.stake(1000.into());

                assert!(staking_pool()
                    .ops_stake_balance(to_valid_account_id(ACCOUNT))
                    .unwrap()
                    .unstaked
                    .is_none());

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
            }
        }

        #[cfg(test)]
        mod tests_unstake_all {
            use super::*;

            #[test]
            #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
            fn not_registered() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                let mut staking_pool: Box<dyn NearStakingPool> = Box::new(staking_pool());
                staking_pool.unstake_all();
            }

            #[test]
            fn with_zero_unstaked_zero_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                testing_env!(ctx.clone());
                let mut staking_pool: Box<dyn NearStakingPool> = Box::new(staking_pool());
                staking_pool.unstake_all();
            }

            #[test]
            fn with_zero_unstaked_with_storage_available_balance() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, None);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                let mut staking_pool: Box<dyn NearStakingPool> = Box::new(staking_pool());
                staking_pool.stake(1000.into());

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                staking_pool.unstake_all();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);
                assert_eq!(
                    logs,
                    vec![
                        "[INFO] [UNSTAKE] near_amount=992, stake_token_amount=992",
                        "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-104)",
                        "[INFO] [FT_BURN] account: bob, amount: 992",
                        "[WARN] [STATUS_OFFLINE] ",
                    ]
                );
            }

            #[test]
            fn with_nonzero_balances() {
                // Arrange
                let mut ctx = new_context(OWNER);
                testing_env!(ctx.clone());

                deploy_stake_contract(staking_public_key());

                let mut account_manager = account_manager();

                // register account
                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                account_manager.storage_deposit(None, Some(true));

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                staking_pool().ops_stake();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                ctx.predecessor_account_id = ACCOUNT.to_string();
                ctx.account_balance = env::account_balance();
                ctx.attached_deposit = 0;
                testing_env!(ctx.clone());
                staking_pool().unstake_all();

                let logs = test_utils::get_logs();
                println!("{:#?}", logs);

                assert_eq!(logs, vec![
                    "[INFO] [UNSTAKE] near_amount=992000000000000000000000, stake_token_amount=992000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-104)",
                    "[INFO] [FT_BURN] account: bob, amount: 992000000000000000000000",
                    "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(184)",
                    "[WARN] [STATUS_OFFLINE] ",
                ]);
            }
        }
    }
}
