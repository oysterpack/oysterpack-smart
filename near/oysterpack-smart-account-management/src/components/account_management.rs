//! [`AccountManagementComponent`]
//! - constructor: [`AccountManagementComponent::new`]
//!   - [`ContractPermissions`]
//! - deployment: [`AccountManagementComponent::deploy`]
//!   - config: [`AccountManagementComponentConfig`]

use crate::*;
use oysterpack_smart_near::near_sdk::{
    borsh::{BorshDeserialize, BorshSerialize},
    env,
    json_types::ValidAccountId,
    Promise,
};
use oysterpack_smart_near::{
    asserts::{assert_min_near_attached, assert_yocto_near_attached},
    domain::YoctoNear,
    eventbus, ErrCode, ErrorConst,
};
use std::{fmt::Debug, ops::Deref};

use crate::components::account_repository::AccountRepositoryComponent;
use crate::components::account_storage_usage::AccountStorageUsageComponent;
use oysterpack_smart_near::asserts::{assert_account_not_predecessor, ERR_INVALID};
use oysterpack_smart_near::component::Deploy;
use oysterpack_smart_near::domain::StorageUsage;
use std::collections::HashMap;
use std::marker::PhantomData;

pub const ERR_INSUFFICIENT_STORAGE_BALANCE: ErrorConst = ErrorConst(
    ErrCode("INSUFFICIENT_STORAGE_BALANCE"),
    "account's available storage balance is insufficient to satisfy request",
);

/// Core account management component implements the following interfaces:
/// 1. [`AccountRepository`]
/// 2. [`StorageManagement`] - NEP-145
/// 3. [`AccountStorageUsage`]
/// 4. [`PermissionsManagement`]
///
/// ## Deployment
/// - [`AccountManagementComponent::deploy`]
/// - [`AccountManagementComponentConfig`]
///
/// ## Constructor
/// - [AccountManagementComponent::new]
pub struct AccountManagementComponent<T>
where
    T: BorshSerialize + BorshDeserialize + Clone + Debug + PartialEq + Default,
{
    contract_permissions: ContractPermissions,
    account_repository: AccountRepositoryComponent<T>,

    _phantom_data: PhantomData<T>,
}

impl<T> Default for AccountManagementComponent<T>
where
    T: BorshSerialize + BorshDeserialize + Clone + Debug + PartialEq + Default,
{
    fn default() -> Self {
        Self::new(Default::default())
    }
}

impl<T> AccountManagementComponent<T>
where
    T: BorshSerialize + BorshDeserialize + Clone + Debug + PartialEq + Default,
{
    pub fn new(contract_permissions: ContractPermissions) -> Self {
        AccountMetrics::register_account_storage_event_handler();
        Self {
            contract_permissions,
            account_repository: Default::default(),
            _phantom_data: Default::default(),
        }
    }
}

impl<T> AccountManagementComponent<T>
where
    T: BorshSerialize + BorshDeserialize + Clone + Debug + PartialEq + Default,
{
    /// helper method used to measure the amount of storage needed to store the specified data.
    pub fn measure_storage_usage(account_data: T) -> StorageUsage {
        let mut account_manager: Self = Self::new(Default::default());

        // seeds the storage required to store metrics
        {
            let account_id = "1953717115592535419708657925195464285";
            account_manager.delete_account(account_id);
            account_manager.create_account(account_id, 0.into(), Some(account_data.clone()));
            account_manager.delete_account(account_id);
        }

        let account_id = "1953718041838591893489340663938715635";
        account_manager.delete_account(account_id);
        let initial_storage_usage = env::storage_usage();
        let (mut account, _data) =
            account_manager.create_account(account_id, 0.into(), Some(account_data));
        account.grant_operator();
        account.save();
        let storage_usage = env::storage_usage() - initial_storage_usage;

        // clean up storage
        account_manager.delete_account(account_id);
        // ensure all data is cleaned up
        assert_eq!(initial_storage_usage, env::storage_usage());

        storage_usage.into()
    }

    pub fn account_metrics() -> AccountMetrics {
        AccountMetrics::load()
    }

    /// if the account is not registered, then the contract will register the account and pay for its
    /// storage
    pub fn get_or_register_account(account_id: &str) -> AccountNearDataObject {
        AccountNearDataObject::load(account_id).unwrap_or_else(|| {
            // register the account
            {
                AccountMetrics::register_account_storage_event_handler();
                let storage_balance_bounds: StorageBalanceBounds = AccountStorageUsageComponent
                    .ops_storage_usage_bounds()
                    .into();
                let account = AccountNearDataObject::new(account_id, storage_balance_bounds.min);
                let storage_balance = account.storage_balance(storage_balance_bounds.min);
                account.save();
                eventbus::post(&AccountStorageEvent::Registered(storage_balance));
            }
            // the account storage usage is updated by the storage event handler - thus the object state
            // becomes stale, and we need to return a fresh updated instance from storage
            AccountNearDataObject::registered_account(account_id)
        })
    }

    pub fn register_account_if_not_exists(account_id: &str) {
        if !AccountNearDataObject::exists(account_id) {
            AccountMetrics::register_account_storage_event_handler();
            let storage_balance_bounds: StorageBalanceBounds = AccountStorageUsageComponent
                .ops_storage_usage_bounds()
                .into();
            let account = AccountNearDataObject::new(account_id, storage_balance_bounds.min);
            let storage_balance = account.storage_balance(storage_balance_bounds.min);
            account.save();
            eventbus::post(&AccountStorageEvent::Registered(storage_balance));
        }
    }
}

impl<T> Deploy for AccountManagementComponent<T>
where
    T: BorshSerialize + BorshDeserialize + Clone + Debug + PartialEq + Default,
{
    type Config = AccountManagementComponentConfig;

    fn deploy(config: Self::Config) {
        // configure storage usage bounds
        {
            let mut storage_usage_bounds =
                config
                    .storage_usage_bounds
                    .unwrap_or_else(|| StorageUsageBounds {
                        min: Self::measure_storage_usage(Default::default()),
                        max: None,
                    });

            let storage_usage_bounds =
                config
                    .component_account_storage_mins
                    .map_or(storage_usage_bounds, |funcs| {
                        let account_storage_min: StorageUsage = funcs
                            .iter()
                            .fold(storage_usage_bounds.min, |sum, f| (sum + f()));
                        storage_usage_bounds.min = account_storage_min;
                        storage_usage_bounds
                    });

            AccountStorageUsageComponent::deploy(storage_usage_bounds);
        }

        // create admin account
        {
            let mut account = Self::get_or_register_account(config.admin_account.as_ref().as_str());
            account.grant_admin();
            account.save();
        }
    }
}

/// [`AccountManagementComponent::deploy`] deployment config
pub struct AccountManagementComponentConfig {
    /// if not specified then the default min will be measured and max will be unbounded
    pub storage_usage_bounds: Option<StorageUsageBounds>,

    /// components that manage account data must register functions that provide min account storage requirements
    pub component_account_storage_mins: Option<Vec<fn() -> StorageUsage>>,

    /// required to seed the contract with an admin account
    /// - storage usage costs will be paid for by the contract owner - normally the initial admin
    ///   account will be the contract owner
    pub admin_account: ValidAccountId,
}

impl AccountManagementComponentConfig {
    pub fn new(admin_account: ValidAccountId) -> Self {
        Self {
            admin_account,
            storage_usage_bounds: None,
            component_account_storage_mins: None,
        }
    }
}

impl<T> AccountRepository<T> for AccountManagementComponent<T>
where
    T: BorshSerialize + BorshDeserialize + Clone + Debug + PartialEq + Default,
{
    fn create_account(
        &mut self,
        account_id: &str,
        near_balance: YoctoNear,
        data: Option<T>,
    ) -> Account<T> {
        self.account_repository
            .create_account(account_id, near_balance, data)
    }

    fn load_account(&self, account_id: &str) -> Option<Account<T>> {
        self.account_repository.load_account(account_id)
    }

    fn load_account_data(&self, account_id: &str) -> Option<AccountDataObject<T>> {
        self.account_repository.load_account_data(account_id)
    }

    fn load_account_near_data(&self, account_id: &str) -> Option<AccountNearDataObject> {
        self.account_repository.load_account_near_data(account_id)
    }

    fn registered_account(&self, account_id: &str) -> Account<T> {
        self.account_repository.registered_account(account_id)
    }

    fn registered_account_near_data(&self, account_id: &str) -> AccountNearDataObject {
        self.account_repository
            .registered_account_near_data(account_id)
    }

    fn registered_account_data(&self, account_id: &str) -> AccountDataObject<T> {
        self.account_repository.registered_account_data(account_id)
    }

    fn account_exists(&self, account_id: &str) -> bool {
        self.account_repository.account_exists(account_id)
    }

    fn delete_account(&mut self, account_id: &str) {
        self.account_repository.delete_account(account_id)
    }
}

/// exposes [`AccountStorageUsage`] interface on the component
impl<T> AccountStorageUsage for AccountManagementComponent<T>
where
    T: BorshSerialize + BorshDeserialize + Clone + Debug + PartialEq + Default,
{
    fn ops_storage_usage_bounds(&self) -> StorageUsageBounds {
        AccountStorageUsageComponent.ops_storage_usage_bounds()
    }

    fn ops_storage_usage(&self, account_id: ValidAccountId) -> Option<StorageUsage> {
        AccountStorageUsageComponent.ops_storage_usage(account_id)
    }
}

impl<T> StorageManagement for AccountManagementComponent<T>
where
    T: BorshSerialize + BorshDeserialize + Clone + Debug + PartialEq + Default + 'static,
{
    fn storage_deposit(
        &mut self,
        account_id: Option<ValidAccountId>,
        registration_only: Option<bool>,
    ) -> StorageBalance {
        // if the account ID is not specified, then deposit is for the predecessor account ID
        let account_id = account_id.map_or_else(env::predecessor_account_id, |account_id| {
            account_id.as_ref().clone()
        });

        let storage_balance_bounds = self.storage_balance_bounds();

        let registration_only = registration_only.unwrap_or(false);
        if registration_only {
            assert_min_near_attached(storage_balance_bounds.min);
        }
        let deposit: YoctoNear = env::attached_deposit().into();

        let account: AccountNearDataObject = match self.load_account_near_data(&account_id) {
            Some(mut account) => {
                if registration_only {
                    // refund the full deposit
                    send_refund(deposit.value());
                } else {
                    if let Some(max) = storage_balance_bounds.max {
                        self.deposit_with_max_bound(
                            &mut account,
                            Deposit(deposit),
                            MaxStorageBalance(max),
                        )
                    } else {
                        self.deposit(&mut account, deposit)
                    }
                }
                account
            }
            None => self.register_account(&account_id, deposit, registration_only),
        };

        account.storage_balance(storage_balance_bounds.min)
    }

    fn storage_withdraw(&mut self, amount: Option<YoctoNear>) -> StorageBalance {
        assert_yocto_near_attached();

        let account_id = env::predecessor_account_id();
        let mut account = self.registered_account_near_data(&account_id);
        let storage_balance_bounds = self.storage_balance_bounds();
        let account_available_balance = account
            .storage_balance(storage_balance_bounds.min)
            .available;
        match amount {
            Some(amount) => {
                if amount > YoctoNear::ZERO {
                    ERR_INSUFFICIENT_STORAGE_BALANCE.assert(|| account_available_balance >= amount);
                    send_refund(amount + 1);
                    account.decr_near_balance(amount);
                    account.save();
                }
            }
            None => {
                // withdraw the total available balance
                if account_available_balance > YoctoNear::ZERO {
                    send_refund(account_available_balance + 1);
                    account.decr_near_balance(account_available_balance);
                    account.save();
                }
            }
        }

        account.storage_balance(storage_balance_bounds.min)
    }

    fn storage_unregister(&mut self, force: Option<bool>) -> bool {
        assert_yocto_near_attached();
        let account_id = env::predecessor_account_id();
        self.load_account_near_data(&account_id)
            .map_or(false, |account| {
                let account_near_balance = account.near_balance();
                eventbus::post(&StorageManagementEvent::PreUnregister {
                    account_id: account_id.clone(),
                    force: force.unwrap_or(false),
                });
                self.delete_account(&account_id);
                eventbus::post(&AccountStorageEvent::Unregistered(account_near_balance));
                send_refund(account_near_balance + 1);
                true
            })
    }

    fn storage_balance_bounds(&self) -> StorageBalanceBounds {
        AccountStorageUsageComponent
            .ops_storage_usage_bounds()
            .into()
    }

    fn storage_balance_of(&self, account_id: ValidAccountId) -> Option<StorageBalance> {
        self.load_account_near_data(account_id.as_ref())
            .map(|account| account.storage_balance(self.storage_balance_bounds().min))
    }
}

impl<T> PermissionsManagement for AccountManagementComponent<T>
where
    T: BorshSerialize + BorshDeserialize + Clone + Debug + PartialEq + Default + 'static,
{
    fn ops_permissions_is_admin(&self, account_id: ValidAccountId) -> bool {
        self.load_account_near_data(account_id.as_ref())
            .map_or(false, |account| account.is_admin())
    }

    fn ops_permissions_grant_admin(&mut self, account_id: ValidAccountId) {
        assert_account_not_predecessor(account_id.as_ref());
        self.assert_predecessor_is_admin();

        let mut account = self.registered_account_near_data(account_id.as_ref());
        if !account.is_admin() {
            account.grant_admin();
            account.save();
            LOG_EVENT_PERMISSIONS_GRANT.log("admin")
        }
    }

    fn ops_permissions_revoke_admin(&mut self, account_id: ValidAccountId) {
        assert_account_not_predecessor(account_id.as_ref());
        self.assert_predecessor_is_admin();

        let mut account = self.registered_account_near_data(account_id.as_ref());
        if account.is_admin() {
            account.revoke_admin();
            Self::clear_permissions_if_has_no_permissions(&mut account);
            account.save();
            LOG_EVENT_PERMISSIONS_REVOKE.log("admin")
        }
    }

    fn ops_permissions_is_operator(&self, account_id: ValidAccountId) -> bool {
        self.load_account_near_data(account_id.as_ref())
            .map_or(false, |account| account.is_operator())
    }

    fn ops_permissions_grant_operator(&mut self, account_id: ValidAccountId) {
        assert_account_not_predecessor(account_id.as_ref());
        self.assert_predecessor_is_admin();

        let mut account = self.registered_account_near_data(account_id.as_ref());
        if !account.is_operator() {
            account.grant_operator();
            account.save();
            LOG_EVENT_PERMISSIONS_GRANT.log("operator")
        }
    }

    fn ops_permissions_revoke_operator(&mut self, account_id: ValidAccountId) {
        assert_account_not_predecessor(account_id.as_ref());
        self.assert_predecessor_is_admin();

        let mut account = self.registered_account_near_data(account_id.as_ref());
        if account.is_operator() {
            account.revoke_operator();
            Self::clear_permissions_if_has_no_permissions(&mut account);
            account.save();
            LOG_EVENT_PERMISSIONS_REVOKE.log("operator")
        }
    }

    fn ops_permissions_grant(&mut self, account_id: ValidAccountId, permissions: Permissions) {
        self.assert_contract_supports_permissions(permissions);
        assert_account_not_predecessor(account_id.as_ref());
        self.assert_predecessor_is_admin();

        let mut account = self.registered_account_near_data(account_id.as_ref());
        if !account.contains_permissions(permissions) {
            account.grant(permissions);
            account.save();
            LOG_EVENT_PERMISSIONS_GRANT.log(format!(
                "{:?}",
                self.contract_permissions.permission_names(permissions)
            ));
        }
    }

    fn ops_permissions_grant_permissions(
        &mut self,
        account_id: ValidAccountId,
        permissions: Vec<u8>,
    ) {
        let permissions = permissions
            .iter()
            .fold(0_u64, |permissions, perm_bit| permissions | 1 << *perm_bit);
        self.ops_permissions_grant(account_id, permissions.into());
    }

    fn ops_permissions_revoke(&mut self, account_id: ValidAccountId, permissions: Permissions) {
        self.assert_contract_supports_permissions(permissions);
        assert_account_not_predecessor(account_id.as_ref());
        self.assert_predecessor_is_admin();

        let mut account = self.registered_account_near_data(account_id.as_ref());
        if account.permissions().is_some() {
            account.revoke(permissions);
            Self::clear_permissions_if_has_no_permissions(&mut account);
            account.save();
            LOG_EVENT_PERMISSIONS_REVOKE.log(format!(
                "{:?}",
                self.contract_permissions.permission_names(permissions)
            ));
        }
    }

    fn ops_permissions_revoke_permissions(
        &mut self,
        account_id: ValidAccountId,
        permissions: Vec<u8>,
    ) {
        let permissions = permissions
            .iter()
            .fold(0_u64, |permissions, perm_bit| permissions | 1 << perm_bit);
        self.ops_permissions_revoke(account_id, permissions.into());
    }

    fn ops_permissions_revoke_all(&mut self, account_id: ValidAccountId) {
        assert_account_not_predecessor(account_id.as_ref());
        self.assert_predecessor_is_admin();
        let mut account = self.registered_account_near_data(account_id.as_ref());
        if account.permissions().is_some() {
            account.revoke_all();
            account.save();
            LOG_EVENT_PERMISSIONS_REVOKE.log("all permissions were revoked");
        }
    }

    fn ops_permissions_contains(
        &self,
        account_id: ValidAccountId,
        permissions: Permissions,
    ) -> bool {
        self.load_account_near_data(account_id.as_ref())
            .map_or(false, |account| account.contains_permissions(permissions))
    }

    fn ops_permissions(&self, account_id: ValidAccountId) -> Option<Permissions> {
        self.load_account_near_data(account_id.as_ref())
            .map(|account| account.permissions())
            .flatten()
    }

    fn ops_permissions_granted(&self, account_id: ValidAccountId) -> Option<HashMap<u8, String>> {
        self.ops_permissions(account_id).map(|perms| {
            let mut account_perms = HashMap::with_capacity(self.contract_permissions.0.len() + 2);
            for (perm_bit, name) in self.contract_permissions.0.iter() {
                if perms.contains(1 << *perm_bit) {
                    account_perms.insert(*perm_bit, name.to_string());
                }
            }
            if perms.contains(Permissions::ADMIN) {
                account_perms.insert(63, "admin".to_string());
            }
            if perms.contains(Permissions::OPERATOR) {
                account_perms.insert(62, "operator".to_string());
            }

            account_perms
        })
    }

    fn ops_permissions_contract_permissions(&self) -> Option<HashMap<u8, String>> {
        if self.contract_permissions.0.is_empty() {
            return None;
        }
        let mut perms = HashMap::with_capacity(self.contract_permissions.0.len());
        for (k, value) in self.contract_permissions.0.iter() {
            perms.insert(*k, value.to_string());
        }
        Some(perms)
    }
}

impl<T> AccountManagementComponent<T>
where
    T: BorshSerialize + BorshDeserialize + Clone + Debug + PartialEq + Default + 'static,
{
    pub fn register_account(
        &mut self,
        account_id: &str,
        deposit: YoctoNear,
        registration_only: bool,
    ) -> AccountNearDataObject {
        let storage_balance_bounds = self.storage_balance_bounds();
        let deposit = Self::initial_deposit(deposit, registration_only, storage_balance_bounds);
        let (account, _data) = self.create_account(account_id, deposit, None);
        eventbus::post(&AccountStorageEvent::Registered(
            account.storage_balance(storage_balance_bounds.min),
        ));
        account
    }

    pub fn permission_by_name(&self, name: &str) -> Option<Permission> {
        if self.contract_permissions.0.is_empty() {
            return None;
        }
        for (k, v) in self.contract_permissions.0.iter() {
            if name == *v {
                return Some(1_u64 << *k);
            }
        }
        None
    }
}

/// helper functions
impl<T> AccountManagementComponent<T>
where
    T: BorshSerialize + BorshDeserialize + Clone + Debug + PartialEq + Default + 'static,
{
    fn clear_permissions_if_has_no_permissions(account: &mut AccountNearData) {
        if let Some(permissions) = account.permissions() {
            if !permissions.has_permissions() {
                account.revoke_all(); // sets permissions to NONE - frees up some storage
            }
        }
    }

    fn assert_contract_supports_permissions(&self, permissions: Permissions) {
        ERR_INVALID.assert(
            || self.contract_permissions.is_supported(permissions),
            || "contract does not support specified permissions",
        );
    }

    fn assert_predecessor_is_admin(&self) {
        let admin = self.registered_account_near_data(env::predecessor_account_id().as_str());
        ERR_NOT_AUTHORIZED.assert(|| admin.is_admin());
    }

    /// refunds deposit amount that is above the max allowed storage balance
    fn deposit_with_max_bound(
        &self,
        account: &mut AccountNearDataObject,
        deposit: Deposit,
        max: MaxStorageBalance,
    ) {
        if account.near_balance() < *max {
            let max_allowed_deposit = *max - account.near_balance();
            let deposit = if *deposit > max_allowed_deposit {
                // refund amount over the upper bound
                send_refund(*deposit - max_allowed_deposit);
                max_allowed_deposit
            } else {
                *deposit
            };

            self.deposit(account, deposit);
        } else {
            // account storage balance is already at max limit - thus refund the full deposit amount
            send_refund(deposit.value());
        }
    }

    fn deposit(&self, account: &mut AccountNearDataObject, deposit: YoctoNear) {
        account.incr_near_balance(deposit);
        account.save();
    }

    fn initial_deposit(
        deposit: YoctoNear,
        registration_only: bool,
        storage_balance_bounds: StorageBalanceBounds,
    ) -> YoctoNear {
        assert_min_near_attached(storage_balance_bounds.min);
        if registration_only {
            // only take the min required and refund the rest
            let refund_amount = deposit - storage_balance_bounds.min;
            if *refund_amount > 0 {
                send_refund(refund_amount);
            }
            storage_balance_bounds.min
        } else {
            // refund deposit that is over the max allowed
            storage_balance_bounds.max.map_or(deposit, |max| {
                if deposit > max {
                    let refund_amount = deposit - max;
                    send_refund(refund_amount);
                    max
                } else {
                    deposit
                }
            })
        }
    }
}

/// refund is always sent back to the predecessor account ID
fn send_refund<Amount: Into<YoctoNear>>(amount: Amount) {
    Promise::new(env::predecessor_account_id()).transfer(amount.into().value());
}

struct Deposit(YoctoNear);

impl Deref for Deposit {
    type Target = YoctoNear;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

struct MaxStorageBalance(YoctoNear);

impl Deref for MaxStorageBalance {
    type Target = YoctoNear;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oysterpack_smart_near::near_sdk::*;
    use oysterpack_smart_near_test::*;

    pub type AccountManager = AccountManagementComponent<()>;
    const ADMIN: &str = "admin";
    const ACCOUNT: &str = "bob";

    fn deploy(
        account_id: &str,
        config: Option<AccountManagementComponentConfig>,
    ) -> (VMContext, AccountManager) {
        let ctx = new_context(account_id);
        testing_env!(ctx.clone());

        AccountManager::deploy(config.unwrap_or_else(|| AccountManagementComponentConfig {
            storage_usage_bounds: Some(StorageUsageBounds {
                min: 1000.into(),
                max: None,
            }),
            component_account_storage_mins: None,
            admin_account: to_valid_account_id(ADMIN),
        }));

        (ctx, AccountManager::new(Default::default()))
    }

    #[test]
    fn get_or_register_account() {
        let (ctx, _account_manager) = deploy(ACCOUNT, None);
        testing_env!(ctx.clone());
        let _alice = AccountManager::get_or_register_account("alice");
        let logs = test_utils::get_logs();
        println!("{:#?}", logs);
        assert_eq!(logs, vec![
            "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(97)",
            "[INFO] [ACCOUNT_STORAGE_CHANGED] Registered(StorageBalance { total: YoctoNear(10000000000000000000000), available: YoctoNear(0) })",
        ]);

        testing_env!(ctx.clone());
        let _alice = AccountManager::get_or_register_account("alice");
        let logs = test_utils::get_logs();
        println!("{:#?}", logs);
        assert!(logs.is_empty());
    }

    #[test]
    fn register_account_if_not_exists() {
        let (ctx, _account_manager) = deploy(ACCOUNT, None);
        testing_env!(ctx.clone());
        AccountManager::register_account_if_not_exists("alice");
        let logs = test_utils::get_logs();
        println!("{:#?}", logs);
        assert_eq!(logs, vec![
            "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(97)",
            "[INFO] [ACCOUNT_STORAGE_CHANGED] Registered(StorageBalance { total: YoctoNear(10000000000000000000000), available: YoctoNear(0) })",
        ]);

        testing_env!(ctx.clone());
        AccountManager::register_account_if_not_exists("alice");
        let logs = test_utils::get_logs();
        println!("{:#?}", logs);
        assert!(logs.is_empty());
    }
}

#[cfg(test)]
mod tests_service {
    use super::*;
    use crate::StorageUsageBounds;
    use oysterpack_smart_near::near_sdk;
    use oysterpack_smart_near_test::*;

    pub type AccountManager = AccountManagementComponent<()>;

    fn comp_account_storage_min() -> StorageUsage {
        1000.into()
    }

    #[test]
    fn deploy_and_use_module() {
        // Arrange
        let account_id = "bob";
        let ctx = new_context(account_id);
        testing_env!(ctx);

        // Act
        AccountManager::deploy(AccountManagementComponentConfig {
            storage_usage_bounds: Some(StorageUsageBounds {
                min: 1000.into(),
                max: None,
            }),
            component_account_storage_mins: Some(vec![comp_account_storage_min]),
            admin_account: to_valid_account_id("owner"),
        });

        let service: AccountManager = AccountManager::new(Default::default());
        let storage_balance_bounds = service.storage_balance_bounds();
        assert_eq!(
            storage_balance_bounds.min,
            (env::storage_byte_cost() * 2000).into()
        );
        assert!(storage_balance_bounds.max.is_none());

        let _storage_usage_bounds = service.storage_balance_of(to_valid_account_id(account_id));
    }
}

#[cfg(test)]
mod tests_storage_management {
    use super::*;
    use crate::{AccountMetrics, StorageUsageBounds};
    use oysterpack_smart_near::domain::StorageUsage;
    use oysterpack_smart_near::near_sdk;
    use oysterpack_smart_near_test::*;

    const STORAGE_USAGE_BOUNDS: StorageUsageBounds = StorageUsageBounds {
        min: StorageUsage(1000),
        max: None,
    };

    fn storage_balance_min() -> YoctoNear {
        (STORAGE_USAGE_BOUNDS.min.value() as u128 * env::STORAGE_PRICE_PER_BYTE).into()
    }

    const PREDECESSOR_ACCOUNT_ID: &str = "alice";

    fn run_test<F>(
        storage_usage_bounds: StorageUsageBounds,
        account_id: Option<&str>,
        registration_only: Option<bool>,
        deposit: YoctoNear,
        already_registered: bool, // if true, then the account ID will be registered before hand using storage balance min
        test: F,
    ) where
        F: FnOnce(AccountManagementComponent<()>, StorageBalance),
    {
        let mut ctx = new_context(PREDECESSOR_ACCOUNT_ID);
        testing_env!(ctx.clone());

        AccountMetrics::register_account_storage_event_handler();
        AccountMetrics::reset();

        AccountStorageUsageComponent::deploy(storage_usage_bounds);

        let mut service: AccountManagementComponent<()> =
            AccountManagementComponent::new(Default::default());
        let storage_balance_bounds = service.storage_balance_bounds();
        println!("storage_balance_bounds = {:?}", storage_balance_bounds);

        if already_registered {
            ctx.attached_deposit = storage_balance_bounds.min.value();
            testing_env!(ctx.clone());
            let storage_balance = service.storage_deposit(
                Some(to_valid_account_id(
                    account_id.unwrap_or(PREDECESSOR_ACCOUNT_ID),
                )),
                Some(true),
            );
            println!("registered account: {:?}", storage_balance);
        }

        ctx.attached_deposit = deposit.value();
        println!("deposit amount = {}", ctx.attached_deposit);
        testing_env!(ctx.clone());

        let storage_balance =
            service.storage_deposit(account_id.map(to_valid_account_id), registration_only);
        println!("storage_balance after deposit = {:?}", storage_balance);

        test(service, storage_balance);
    }

    #[cfg(test)]
    mod tests_storage_deposit {
        use super::*;

        type AccountManager = AccountManagementComponent<()>;

        #[cfg(test)]
        mod self_registration_only {
            use super::*;

            fn run_test<F>(
                deposit: YoctoNear,
                already_registered: bool, // if true, then the account ID will be registered before hand using storage balance min
                test: F,
            ) where
                F: FnOnce(AccountManagementComponent<()>, StorageBalance),
            {
                super::run_test(
                    STORAGE_USAGE_BOUNDS,
                    None,
                    Some(true),
                    deposit,
                    already_registered,
                    test,
                );
            }

            #[test]
            fn unknown_account_with_exact_storage_deposit() {
                run_test(
                    storage_balance_min(),
                    false,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(storage_balance.total, service.storage_balance_bounds().min);
                        assert_eq!(storage_balance.available, 0.into());

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        let account = service.registered_account_near_data(PREDECESSOR_ACCOUNT_ID);
                        assert_eq!(account.near_balance(), service.storage_balance_bounds().min);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            fn unknown_account_with_over_payment() {
                run_test(
                    (storage_balance_min().value() * 3).into(),
                    false,
                    |service, storage_balance: StorageBalance| {
                        // Assert
                        assert_eq!(storage_balance.total, service.storage_balance_bounds().min);
                        assert_eq!(storage_balance.available, 0.into());

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        // Assert account was registered
                        let account = service.registered_account_near_data(PREDECESSOR_ACCOUNT_ID);
                        assert_eq!(account.near_balance(), service.storage_balance_bounds().min);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());

                        // Assert overpayment was refunded
                        let receipts = deserialize_receipts();
                        assert_eq!(receipts.len(), 1);
                        let receipt = &receipts[0];
                        assert_eq!(&receipt.receiver_id, PREDECESSOR_ACCOUNT_ID);
                        let action = &receipt.actions[0];
                        match action {
                            Action::Transfer(action) => {
                                assert_eq!(
                                    action.deposit,
                                    service.storage_balance_bounds().min.value() * 2
                                );
                            }
                            _ => panic!("expected Transfer"),
                        }
                    },
                );
            }

            #[test]
            fn already_registered() {
                run_test(
                    storage_balance_min(),
                    true,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(storage_balance.total, service.storage_balance_bounds().min);
                        assert_eq!(storage_balance.available, 0.into());

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        // Assert the deposit was refunded
                        let receipts = deserialize_receipts();
                        assert_eq!(receipts.len(), 1);
                        let receipt = &receipts[0];
                        assert_eq!(&receipt.receiver_id, PREDECESSOR_ACCOUNT_ID);
                        let action = &receipt.actions[0];
                        match action {
                            Action::Transfer(action) => {
                                assert_eq!(action.deposit, storage_balance_min().value());
                            }
                            _ => panic!("expected Transfer"),
                        }
                    },
                );
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_NEAR_DEPOSIT]")]
            fn zero_deposit_attached() {
                run_test(0.into(), false, |_service, _storage_balance| {});
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_NEAR_DEPOSIT]")]
            fn zero_deposit_attached_already_registered() {
                run_test(0.into(), true, |_service, _storage_balance| {});
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_NEAR_DEPOSIT]")]
            fn one_deposit_attached_already_registered() {
                run_test(1.into(), true, |_service, _storage_balance| {});
            }
        }

        #[cfg(test)]
        mod self_registration_only_with_max_bound_set {
            use super::*;

            const STORAGE_USAGE_BOUNDS: StorageUsageBounds = StorageUsageBounds {
                min: StorageUsage(1000),
                max: Some(StorageUsage(2000)),
            };

            fn storage_balance_min() -> YoctoNear {
                (STORAGE_USAGE_BOUNDS.min.value() as u128 * env::STORAGE_PRICE_PER_BYTE).into()
            }

            fn run_test<F>(
                deposit: YoctoNear,
                already_registered: bool, // if true, then the account ID will be registered before hand using storage balance min
                test: F,
            ) where
                F: FnOnce(AccountManagementComponent<()>, StorageBalance),
            {
                super::run_test(
                    STORAGE_USAGE_BOUNDS,
                    None,
                    Some(true),
                    deposit,
                    already_registered,
                    test,
                );
            }

            #[test]
            fn unknown_account_with_exact_storage_deposit() {
                run_test(
                    storage_balance_min(),
                    false,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(storage_balance.total, service.storage_balance_bounds().min);
                        assert_eq!(storage_balance.available, 0.into());

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        let account = service.registered_account_near_data(PREDECESSOR_ACCOUNT_ID);
                        assert_eq!(account.near_balance(), service.storage_balance_bounds().min);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            fn unknown_account_with_over_payment() {
                run_test(
                    (storage_balance_min().value() * 3).into(),
                    false,
                    |service, storage_balance: StorageBalance| {
                        // Assert
                        assert_eq!(storage_balance.total, service.storage_balance_bounds().min);
                        assert_eq!(storage_balance.available, 0.into());

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        // Assert account was registered
                        let account = service.registered_account_near_data(PREDECESSOR_ACCOUNT_ID);
                        assert_eq!(account.near_balance(), service.storage_balance_bounds().min);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());

                        // Assert overpayment was refunded
                        let receipts = deserialize_receipts();
                        assert_eq!(receipts.len(), 1);
                        let receipt = &receipts[0];
                        assert_eq!(&receipt.receiver_id, PREDECESSOR_ACCOUNT_ID);
                        let action = &receipt.actions[0];
                        match action {
                            Action::Transfer(action) => {
                                assert_eq!(
                                    action.deposit,
                                    service.storage_balance_bounds().min.value() * 2
                                );
                            }
                            _ => panic!("expected Transfer"),
                        }
                    },
                );
            }

            #[test]
            fn already_registered() {
                run_test(
                    storage_balance_min(),
                    true,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(storage_balance.total, service.storage_balance_bounds().min);
                        assert_eq!(storage_balance.available, 0.into());

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        // Assert the deposit was refunded
                        let receipts = deserialize_receipts();
                        assert_eq!(receipts.len(), 1);
                        let receipt = &receipts[0];
                        assert_eq!(&receipt.receiver_id, PREDECESSOR_ACCOUNT_ID);
                        let action = &receipt.actions[0];
                        match action {
                            Action::Transfer(action) => {
                                assert_eq!(action.deposit, storage_balance_min().value());
                            }
                            _ => panic!("expected Transfer"),
                        }
                    },
                );
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_NEAR_DEPOSIT]")]
            fn zero_deposit_attached() {
                run_test(0.into(), false, |_service, _storage_balance| {});
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_NEAR_DEPOSIT]")]
            fn zero_deposit_attached_already_registered() {
                run_test(0.into(), true, |_service, _storage_balance| {});
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_NEAR_DEPOSIT]")]
            fn one_deposit_attached_already_registered() {
                run_test(1.into(), true, |_service, _storage_balance| {});
            }
        }

        #[cfg(test)]
        mod other_registration_only {
            use super::*;

            const ACCOUNT_ID: &str = "alfio";

            fn run_test<F>(
                deposit: YoctoNear,
                already_registered: bool, // if true, then the account ID will be registered before hand using storage balance min
                test: F,
            ) where
                F: FnOnce(AccountManagementComponent<()>, StorageBalance),
            {
                super::run_test(
                    STORAGE_USAGE_BOUNDS,
                    Some(ACCOUNT_ID),
                    Some(true),
                    deposit,
                    already_registered,
                    test,
                );
            }

            #[test]
            fn unknown_account_with_exact_storage_deposit() {
                run_test(
                    storage_balance_min(),
                    false,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(storage_balance.total, service.storage_balance_bounds().min);
                        assert_eq!(storage_balance.available, 0.into());

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        let account = service.registered_account_near_data(ACCOUNT_ID);
                        assert_eq!(account.near_balance(), service.storage_balance_bounds().min);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            fn unknown_account_with_over_payment() {
                run_test(
                    (storage_balance_min().value() * 3).into(),
                    false,
                    |service, storage_balance: StorageBalance| {
                        // Assert
                        assert_eq!(storage_balance.total, service.storage_balance_bounds().min);
                        assert_eq!(storage_balance.available, 0.into());

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        // Assert account was registered
                        let account = service.registered_account_near_data(ACCOUNT_ID);
                        assert_eq!(account.near_balance(), service.storage_balance_bounds().min);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());

                        // Assert overpayment was refunded
                        let receipts = deserialize_receipts();
                        assert_eq!(receipts.len(), 1);
                        let receipt = &receipts[0];
                        assert_eq!(&receipt.receiver_id, PREDECESSOR_ACCOUNT_ID);
                        let action = &receipt.actions[0];
                        match action {
                            Action::Transfer(action) => {
                                assert_eq!(
                                    action.deposit,
                                    service.storage_balance_bounds().min.value() * 2
                                );
                            }
                            _ => panic!("expected Transfer"),
                        }
                    },
                );
            }

            #[test]
            fn already_registered() {
                run_test(
                    storage_balance_min(),
                    true,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(storage_balance.total, service.storage_balance_bounds().min);
                        assert_eq!(storage_balance.available, 0.into());

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        // Assert the deposit was refunded
                        let receipts = deserialize_receipts();
                        assert_eq!(receipts.len(), 1);
                        let receipt = &receipts[0];
                        assert_eq!(&receipt.receiver_id, PREDECESSOR_ACCOUNT_ID);
                        let action = &receipt.actions[0];
                        match action {
                            Action::Transfer(action) => {
                                assert_eq!(action.deposit, storage_balance_min().value());
                            }
                            _ => panic!("expected Transfer"),
                        }
                    },
                );
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_NEAR_DEPOSIT]")]
            fn zero_deposit_attached() {
                run_test(0.into(), false, |_service, _storage_balance| {});
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_NEAR_DEPOSIT]")]
            fn zero_deposit_attached_already_registered() {
                run_test(0.into(), true, |_service, _storage_balance| {});
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_NEAR_DEPOSIT]")]
            fn one_deposit_attached_already_registered() {
                run_test(1.into(), true, |_service, _storage_balance| {});
            }
        }

        #[cfg(test)]
        mod self_deposit_with_implied_registration_only_false {
            use super::*;
            use oysterpack_smart_near::YOCTO;

            fn run_test<F>(
                deposit: YoctoNear,
                already_registered: bool, // if true, then the account ID will be registered before hand using storage balance min
                test: F,
            ) where
                F: FnOnce(AccountManagementComponent<()>, StorageBalance),
            {
                super::run_test(
                    STORAGE_USAGE_BOUNDS,
                    None,
                    None,
                    deposit,
                    already_registered,
                    test,
                );
            }

            fn run_test_with_storage_balance_bounds<F>(
                deposit: YoctoNear,
                already_registered: bool, // if true, then the account ID will be registered before hand using storage balance min
                storage_usage_bounds: StorageUsageBounds,
                test: F,
            ) where
                F: FnOnce(AccountManagementComponent<()>, StorageBalance),
            {
                super::run_test(
                    storage_usage_bounds,
                    None,
                    None,
                    deposit,
                    already_registered,
                    test,
                );
            }

            #[test]
            fn unknown_account_with_exact_storage_deposit() {
                run_test(
                    storage_balance_min(),
                    false,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(storage_balance.total, service.storage_balance_bounds().min);
                        assert_eq!(storage_balance.available, 0.into());

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        let account = service.registered_account_near_data(PREDECESSOR_ACCOUNT_ID);
                        assert_eq!(account.near_balance(), service.storage_balance_bounds().min);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            fn unknown_account_with_over_payment() {
                let deposit_amount: YoctoNear = (storage_balance_min().value() * 3).into();
                run_test(
                    deposit_amount,
                    false,
                    |service, storage_balance: StorageBalance| {
                        // Assert
                        assert_eq!(storage_balance.total, deposit_amount);
                        assert_eq!(
                            storage_balance.available,
                            (service.storage_balance_bounds().min.value() * 2).into()
                        );

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        // Assert account was registered
                        let account = service.registered_account_near_data(PREDECESSOR_ACCOUNT_ID);
                        assert_eq!(account.near_balance(), deposit_amount);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            fn unknown_account_with_over_payment_above_max_bound() {
                let deposit_amount: YoctoNear = (storage_balance_min().value() * 3).into();
                run_test_with_storage_balance_bounds(
                    deposit_amount,
                    false,
                    StorageUsageBounds {
                        min: STORAGE_USAGE_BOUNDS.min,
                        max: Some((STORAGE_USAGE_BOUNDS.min.value() * 2).into()),
                    },
                    |service, storage_balance: StorageBalance| {
                        let storage_balance_bounds = service.storage_balance_bounds();
                        // Assert
                        assert_eq!(storage_balance.total, storage_balance_bounds.max.unwrap());
                        assert_eq!(storage_balance.available, storage_balance_bounds.min);

                        // Assert account NEAR balance was persisted
                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        // Assert account was registered
                        let account = service.registered_account_near_data(PREDECESSOR_ACCOUNT_ID);
                        assert_eq!(account.near_balance(), storage_balance_bounds.max.unwrap());

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());

                        let receipts = deserialize_receipts();
                        let receipt = &receipts[0];
                        assert_eq!(receipt.receiver_id, PREDECESSOR_ACCOUNT_ID);
                        match &receipt.actions[0] {
                            Action::Transfer(transfer) => {
                                assert_eq!(transfer.deposit, storage_balance_bounds.min.value());
                            }
                            _ => panic!("expected Transfer action"),
                        }
                    },
                );
            }

            #[test]
            fn deposit_with_account_already_maxed_out() {
                // Arrange
                let account = "alfio";
                let mut ctx = new_context(account);
                testing_env!(ctx.clone());

                AccountManagementComponent::<()>::deploy(AccountManagementComponentConfig {
                    storage_usage_bounds: Some(StorageUsageBounds {
                        min: 1000.into(),
                        max: Some(2000.into()),
                    }),
                    admin_account: to_valid_account_id("admin"),
                    component_account_storage_mins: None,
                });

                let mut service = AccountManagementComponent::<()>::new(Default::default());

                ctx.attached_deposit = YOCTO;
                testing_env!(ctx.clone());
                let storage_balance_1 = service.storage_deposit(None, None);
                testing_env!(ctx.clone());
                let storage_balance_2 = service.storage_deposit(None, None);
                assert_eq!(storage_balance_1, storage_balance_2);
                assert_eq!(
                    storage_balance_1.total,
                    service.storage_balance_bounds().max.unwrap()
                );
                let receipts = deserialize_receipts();
                assert_eq!(&receipts[0].receiver_id, account);
                match &receipts[0].actions[0] {
                    Action::Transfer(transfer) => assert_eq!(transfer.deposit, YOCTO),
                    _ => panic!("expected TransferAction"),
                }
            }

            #[test]
            fn already_registered() {
                run_test(
                    storage_balance_min(),
                    true,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(
                            storage_balance.total.value(),
                            service.storage_balance_bounds().min.value() * 2
                        );
                        assert_eq!(
                            storage_balance.available,
                            service.storage_balance_bounds().min
                        );

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);
                    },
                );
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_NEAR_DEPOSIT]")]
            fn zero_deposit_attached() {
                run_test(0.into(), false, |_service, _storage_balance| {});
            }

            #[test]
            fn zero_deposit_attached_already_registered() {
                run_test(0.into(), true, |service, storage_balance| {
                    let storage_balance_bounds = service.storage_balance_bounds();
                    assert_eq!(storage_balance.total, storage_balance_bounds.min);
                    assert_eq!(storage_balance.available, 0.into());
                });
            }

            #[test]
            fn one_deposit_attached_already_registered() {
                run_test(1.into(), true, |service, storage_balance| {
                    let storage_balance_bounds = service.storage_balance_bounds();
                    assert_eq!(
                        storage_balance.total.value(),
                        storage_balance_bounds.min.value() + 1
                    );
                    assert_eq!(storage_balance.available, 1.into());

                    // Assert account NEAR balance was persisted
                    let storage_balance_2 = service
                        .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                        .unwrap();
                    assert_eq!(storage_balance, storage_balance_2);
                });
            }
        }

        #[cfg(test)]
        mod self_deposit_with_implied_registration_only_false_with_max_bound_set {
            use super::*;

            const STORAGE_USAGE_BOUNDS: StorageUsageBounds = StorageUsageBounds {
                min: StorageUsage(1000),
                max: Some(StorageUsage(1500)),
            };

            fn storage_balance_min() -> YoctoNear {
                (STORAGE_USAGE_BOUNDS.min.value() as u128 * env::STORAGE_PRICE_PER_BYTE).into()
            }

            fn storage_balance_max() -> YoctoNear {
                (STORAGE_USAGE_BOUNDS.max.unwrap().value() as u128 * env::STORAGE_PRICE_PER_BYTE)
                    .into()
            }

            fn run_test<F>(
                deposit: YoctoNear,
                already_registered: bool, // if true, then the account ID will be registered before hand using storage balance min
                test: F,
            ) where
                F: FnOnce(AccountManagementComponent<()>, StorageBalance),
            {
                super::run_test(
                    STORAGE_USAGE_BOUNDS,
                    None,
                    None,
                    deposit,
                    already_registered,
                    test,
                );
            }

            fn run_test_with_storage_balance_bounds<F>(
                deposit: YoctoNear,
                already_registered: bool, // if true, then the account ID will be registered before hand using storage balance min
                storage_usage_bounds: StorageUsageBounds,
                test: F,
            ) where
                F: FnOnce(AccountManagementComponent<()>, StorageBalance),
            {
                super::run_test(
                    storage_usage_bounds,
                    None,
                    None,
                    deposit,
                    already_registered,
                    test,
                );
            }

            #[test]
            fn unknown_account_with_exact_storage_deposit() {
                run_test(
                    storage_balance_min(),
                    false,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(storage_balance.total, service.storage_balance_bounds().min);
                        assert_eq!(storage_balance.available, 0.into());

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        let account = service.registered_account_near_data(PREDECESSOR_ACCOUNT_ID);
                        assert_eq!(account.near_balance(), service.storage_balance_bounds().min);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            fn unknown_account_with_over_payment() {
                let deposit_amount: YoctoNear = (storage_balance_min().value() * 3).into();
                run_test(
                    deposit_amount,
                    false,
                    |service, storage_balance: StorageBalance| {
                        // Assert
                        assert_eq!(storage_balance.total, storage_balance_max());
                        assert_eq!(
                            storage_balance.available,
                            storage_balance_max() - storage_balance_min()
                        );

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        // Assert account was registered
                        let account = service.registered_account_near_data(PREDECESSOR_ACCOUNT_ID);
                        assert_eq!(account.near_balance(), storage_balance_max());

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            fn unknown_account_with_over_payment_above_max_bound() {
                let deposit_amount: YoctoNear = (storage_balance_min().value() * 3).into();
                run_test_with_storage_balance_bounds(
                    deposit_amount,
                    false,
                    StorageUsageBounds {
                        min: STORAGE_USAGE_BOUNDS.min,
                        max: Some((STORAGE_USAGE_BOUNDS.min.value() * 2).into()),
                    },
                    |service, storage_balance: StorageBalance| {
                        let storage_balance_bounds = service.storage_balance_bounds();
                        // Assert
                        assert_eq!(storage_balance.total, storage_balance_bounds.max.unwrap());
                        assert_eq!(storage_balance.available, storage_balance_bounds.min);

                        // Assert account NEAR balance was persisted
                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        // Assert account was registered
                        let account = service.registered_account_near_data(PREDECESSOR_ACCOUNT_ID);
                        assert_eq!(account.near_balance(), storage_balance_bounds.max.unwrap());

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());

                        let receipts = deserialize_receipts();
                        let receipt = &receipts[0];
                        assert_eq!(receipt.receiver_id, PREDECESSOR_ACCOUNT_ID);
                        match &receipt.actions[0] {
                            Action::Transfer(transfer) => {
                                assert_eq!(transfer.deposit, storage_balance_bounds.min.value());
                            }
                            _ => panic!("expected Transfer action"),
                        }
                    },
                );
            }

            #[test]
            fn already_registered() {
                run_test(
                    storage_balance_min(),
                    true,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(storage_balance.total, storage_balance_max());
                        assert_eq!(
                            storage_balance.available,
                            storage_balance_max() - storage_balance_min()
                        );

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);
                    },
                );
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_NEAR_DEPOSIT]")]
            fn zero_deposit_attached() {
                run_test(0.into(), false, |_service, _storage_balance| {});
            }

            #[test]
            fn zero_deposit_attached_already_registered() {
                run_test(0.into(), true, |service, storage_balance| {
                    let storage_balance_bounds = service.storage_balance_bounds();
                    assert_eq!(storage_balance.total, storage_balance_bounds.min);
                    assert_eq!(storage_balance.available, 0.into());
                });
            }

            #[test]
            fn one_deposit_attached_already_registered() {
                run_test(1.into(), true, |service, storage_balance| {
                    let storage_balance_bounds = service.storage_balance_bounds();
                    assert_eq!(
                        storage_balance.total.value(),
                        storage_balance_bounds.min.value() + 1
                    );
                    assert_eq!(storage_balance.available, 1.into());

                    // Assert account NEAR balance was persisted
                    let storage_balance_2 = service
                        .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                        .unwrap();
                    assert_eq!(storage_balance, storage_balance_2);
                });
            }
        }

        #[cfg(test)]
        mod deposit_for_account_with_implied_registration_only_false {
            use super::*;

            const ACCOUNT_ID: &str = "alfio.near";

            fn run_test<F>(
                deposit: YoctoNear,
                already_registered: bool, // if true, then the account ID will be registered before hand using storage balance min
                test: F,
            ) where
                F: FnOnce(AccountManagementComponent<()>, StorageBalance),
            {
                super::run_test(
                    STORAGE_USAGE_BOUNDS,
                    Some(ACCOUNT_ID),
                    None,
                    deposit,
                    already_registered,
                    test,
                );
            }

            fn run_test_with_storage_balance_bounds<F>(
                deposit: YoctoNear,
                already_registered: bool, // if true, then the account ID will be registered before hand using storage balance min
                storage_usage_bounds: StorageUsageBounds,
                test: F,
            ) where
                F: FnOnce(AccountManagementComponent<()>, StorageBalance),
            {
                super::run_test(
                    storage_usage_bounds,
                    Some(ACCOUNT_ID),
                    None,
                    deposit,
                    already_registered,
                    test,
                );
            }

            #[test]
            fn unknown_account_with_exact_storage_deposit() {
                run_test(
                    storage_balance_min(),
                    false,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(storage_balance.total, service.storage_balance_bounds().min);
                        assert_eq!(storage_balance.available, 0.into());

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        let account = service.registered_account_near_data(ACCOUNT_ID);
                        assert_eq!(account.near_balance(), service.storage_balance_bounds().min);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            fn unknown_account_with_over_payment() {
                let deposit_amount: YoctoNear = (storage_balance_min().value() * 3).into();
                run_test(
                    deposit_amount,
                    false,
                    |service, storage_balance: StorageBalance| {
                        // Assert
                        assert_eq!(storage_balance.total, deposit_amount);
                        assert_eq!(
                            storage_balance.available,
                            (service.storage_balance_bounds().min.value() * 2).into()
                        );

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        // Assert account was registered
                        let account = service.registered_account_near_data(ACCOUNT_ID);
                        assert_eq!(account.near_balance(), deposit_amount);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            fn unknown_account_with_over_payment_above_max_bound() {
                let deposit_amount: YoctoNear = (storage_balance_min().value() * 3).into();
                run_test_with_storage_balance_bounds(
                    deposit_amount,
                    false,
                    StorageUsageBounds {
                        min: STORAGE_USAGE_BOUNDS.min,
                        max: Some((STORAGE_USAGE_BOUNDS.min.value() * 2).into()),
                    },
                    |service, storage_balance: StorageBalance| {
                        let storage_balance_bounds = service.storage_balance_bounds();
                        // Assert
                        assert_eq!(storage_balance.total, storage_balance_bounds.max.unwrap());
                        assert_eq!(storage_balance.available, storage_balance_bounds.min);

                        // Assert account NEAR balance was persisted
                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);

                        // Assert account was registered
                        let account = service.registered_account_near_data(ACCOUNT_ID);
                        assert_eq!(account.near_balance(), storage_balance_bounds.max.unwrap());

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());

                        let receipts = deserialize_receipts();
                        let receipt = &receipts[0];
                        assert_eq!(receipt.receiver_id, PREDECESSOR_ACCOUNT_ID);
                        match &receipt.actions[0] {
                            Action::Transfer(transfer) => {
                                assert_eq!(transfer.deposit, storage_balance_bounds.min.value());
                            }
                            _ => panic!("expected Transfer action"),
                        }
                    },
                );
            }

            #[test]
            fn already_registered() {
                run_test(
                    storage_balance_min(),
                    true,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(
                            storage_balance.total.value(),
                            service.storage_balance_bounds().min.value() * 2
                        );
                        assert_eq!(
                            storage_balance.available,
                            service.storage_balance_bounds().min
                        );

                        let storage_balance_2 = service
                            .storage_balance_of(to_valid_account_id(ACCOUNT_ID))
                            .unwrap();
                        assert_eq!(storage_balance, storage_balance_2);
                    },
                );
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_NEAR_DEPOSIT]")]
            fn zero_deposit_attached() {
                run_test(0.into(), false, |_service, _storage_balance| {});
            }

            #[test]
            fn zero_deposit_attached_already_registered() {
                run_test(0.into(), true, |service, storage_balance| {
                    let storage_balance_bounds = service.storage_balance_bounds();
                    assert_eq!(storage_balance.total, storage_balance_bounds.min);
                    assert_eq!(storage_balance.available, 0.into());
                });
            }

            #[test]
            fn one_deposit_attached_already_registered() {
                run_test(1.into(), true, |service, storage_balance| {
                    let storage_balance_bounds = service.storage_balance_bounds();
                    assert_eq!(
                        storage_balance.total.value(),
                        storage_balance_bounds.min.value() + 1
                    );
                    assert_eq!(storage_balance.available, 1.into());

                    // Assert account NEAR balance was persisted
                    let storage_balance_2 = service
                        .storage_balance_of(to_valid_account_id(ACCOUNT_ID))
                        .unwrap();
                    assert_eq!(storage_balance, storage_balance_2);
                });
            }
        }

        #[cfg(test)]
        mod self_deposit_with_registration_only_false {
            use super::*;

            fn run_test<F>(
                deposit: YoctoNear,
                already_registered: bool, // if true, then the account ID will be registered before hand using storage balance min
                test: F,
            ) where
                F: FnOnce(AccountManagementComponent<()>, StorageBalance),
            {
                super::run_test(
                    STORAGE_USAGE_BOUNDS,
                    None,
                    Some(false),
                    deposit,
                    already_registered,
                    test,
                );
            }

            #[test]
            fn unknown_account_with_exact_storage_deposit() {
                run_test(
                    storage_balance_min(),
                    false,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(storage_balance.total, service.storage_balance_bounds().min);
                        assert_eq!(storage_balance.available, 0.into());

                        let account = service.registered_account_near_data(PREDECESSOR_ACCOUNT_ID);
                        assert_eq!(account.near_balance(), service.storage_balance_bounds().min);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            fn unknown_account_with_over_payment() {
                let deposit_amount: YoctoNear = (storage_balance_min().value() * 3).into();
                run_test(
                    deposit_amount,
                    false,
                    |service, storage_balance: StorageBalance| {
                        // Assert
                        assert_eq!(storage_balance.total, deposit_amount);
                        assert_eq!(
                            storage_balance.available,
                            (service.storage_balance_bounds().min.value() * 2).into()
                        );

                        // Assert account was registered
                        let account = service.registered_account_near_data(PREDECESSOR_ACCOUNT_ID);
                        assert_eq!(account.near_balance(), deposit_amount);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            fn already_registered() {
                run_test(
                    storage_balance_min(),
                    true,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(
                            storage_balance.total.value(),
                            service.storage_balance_bounds().min.value() * 2
                        );
                        assert_eq!(
                            storage_balance.available,
                            service.storage_balance_bounds().min
                        );

                        let account = service.registered_account_near_data(PREDECESSOR_ACCOUNT_ID);
                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_NEAR_DEPOSIT]")]
            fn zero_deposit_attached() {
                run_test(0.into(), false, |_service, _storage_balance| {});
            }

            #[test]
            fn zero_deposit_attached_already_registered() {
                run_test(0.into(), true, |service, storage_balance| {
                    let storage_balance_bounds = service.storage_balance_bounds();
                    assert_eq!(storage_balance.total, storage_balance_bounds.min);
                    assert_eq!(storage_balance.available, 0.into());
                });
            }

            #[test]
            fn one_deposit_attached_already_registered() {
                run_test(1.into(), true, |service, storage_balance| {
                    let storage_balance_bounds = service.storage_balance_bounds();
                    assert_eq!(
                        storage_balance.total.value(),
                        storage_balance_bounds.min.value() + 1
                    );
                    assert_eq!(storage_balance.available, 1.into());
                });
            }
        }

        #[cfg(test)]
        mod deposit_for_other_with_registration_only_false {
            use super::*;

            const ACCOUNT_ID: &str = "alfio.near";

            fn run_test<F>(
                deposit: YoctoNear,
                already_registered: bool, // if true, then the account ID will be registered before hand using storage balance min
                test: F,
            ) where
                F: FnOnce(AccountManagementComponent<()>, StorageBalance),
            {
                super::run_test(
                    STORAGE_USAGE_BOUNDS,
                    Some(ACCOUNT_ID),
                    Some(false),
                    deposit,
                    already_registered,
                    test,
                );
            }

            #[test]
            fn unknown_account_with_exact_storage_deposit() {
                run_test(
                    storage_balance_min(),
                    false,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(storage_balance.total, service.storage_balance_bounds().min);
                        assert_eq!(storage_balance.available, 0.into());

                        let account = service.registered_account_near_data(ACCOUNT_ID);
                        assert_eq!(account.near_balance(), service.storage_balance_bounds().min);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            fn unknown_account_with_over_payment() {
                let deposit_amount: YoctoNear = (storage_balance_min().value() * 3).into();
                run_test(
                    deposit_amount,
                    false,
                    |service, storage_balance: StorageBalance| {
                        // Assert
                        assert_eq!(storage_balance.total, deposit_amount);
                        assert_eq!(
                            storage_balance.available,
                            (service.storage_balance_bounds().min.value() * 2).into()
                        );

                        // Assert account was registered
                        let account = service.registered_account_near_data(ACCOUNT_ID);
                        assert_eq!(account.near_balance(), deposit_amount);

                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            fn already_registered() {
                run_test(
                    storage_balance_min(),
                    true,
                    |service, storage_balance: StorageBalance| {
                        assert_eq!(
                            storage_balance.total.value(),
                            service.storage_balance_bounds().min.value() * 2
                        );
                        assert_eq!(
                            storage_balance.available,
                            service.storage_balance_bounds().min
                        );

                        let account = service.registered_account_near_data(ACCOUNT_ID);
                        // AccountStorageEvent:Registered event should have been published to update stats
                        let account_stats = AccountManager::account_metrics();
                        assert_eq!(account_stats.total_registered_accounts, 1.into());
                        assert_eq!(account_stats.total_near_balance, account.near_balance());
                    },
                );
            }

            #[test]
            #[should_panic(expected = "[ERR] [INSUFFICIENT_NEAR_DEPOSIT]")]
            fn zero_deposit_attached() {
                run_test(0.into(), false, |_service, _storage_balance| {});
            }

            #[test]
            fn zero_deposit_attached_already_registered() {
                run_test(0.into(), true, |service, storage_balance| {
                    let storage_balance_bounds = service.storage_balance_bounds();
                    assert_eq!(storage_balance.total, storage_balance_bounds.min);
                    assert_eq!(storage_balance.available, 0.into());
                });
            }

            #[test]
            fn one_deposit_attached_already_registered() {
                run_test(1.into(), true, |service, storage_balance| {
                    let storage_balance_bounds = service.storage_balance_bounds();
                    assert_eq!(
                        storage_balance.total.value(),
                        storage_balance_bounds.min.value() + 1
                    );
                    assert_eq!(storage_balance.available, 1.into());
                });
            }
        }
    }

    #[cfg(test)]
    mod test_storage_withdraw {
        use super::*;

        pub type AccountManager = AccountManagementComponent<()>;

        fn run_test<F>(
            storage_usage_bounds: StorageUsageBounds,
            deposit: YoctoNear,
            withdraw_deposit: YoctoNear,
            withdrawal: Option<YoctoNear>,
            test: F,
        ) where
            F: FnOnce(AccountManagementComponent<()>, StorageBalance),
        {
            let mut ctx = new_context(PREDECESSOR_ACCOUNT_ID);
            testing_env!(ctx.clone());

            AccountMetrics::register_account_storage_event_handler();
            AccountMetrics::reset();

            AccountStorageUsageComponent::deploy(storage_usage_bounds);

            let mut service: AccountManagementComponent<()> =
                AccountManagementComponent::new(Default::default());

            if deposit.value() > 0 {
                ctx.attached_deposit = deposit.value();
                testing_env!(ctx.clone());
                service.storage_deposit(None, None);
            }

            ctx.attached_deposit = withdraw_deposit.value();
            testing_env!(ctx.clone());
            let storage_balance = service.storage_withdraw(withdrawal);
            test(service, storage_balance);
        }

        #[test]
        fn withdraw_amount_success() {
            run_test(
                STORAGE_USAGE_BOUNDS,
                storage_balance_min() * 2,
                1.into(),
                Some(storage_balance_min() / 2),
                |service, storage_balance| {
                    assert_eq!(
                        storage_balance.total,
                        storage_balance_min() + (storage_balance_min() / 2).value()
                    );
                    assert_eq!(storage_balance.available, storage_balance_min() / 2);

                    // Assert account NEAR balance was persisted
                    let storage_balance_2 = service
                        .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                        .unwrap();
                    assert_eq!(storage_balance, storage_balance_2);

                    // check refund was sent
                    let receipts = deserialize_receipts();
                    let receipt = &receipts[0];
                    assert_eq!(&receipt.receiver_id, PREDECESSOR_ACCOUNT_ID);
                    let action = &receipt.actions[0];
                    match action {
                        Action::Transfer(transfer) => {
                            assert_eq!(transfer.deposit, storage_balance_min().value() / 2 + 1);
                        }
                        _ => panic!("expected TransferAction"),
                    }

                    // check account stats
                    let stats = AccountManager::account_metrics();
                    assert_eq!(stats.total_near_balance, storage_balance.total);
                },
            );
        }

        #[test]
        fn withdraw_all_available_balance_success() {
            run_test(
                STORAGE_USAGE_BOUNDS,
                storage_balance_min() * 2,
                1.into(),
                None,
                |service, storage_balance| {
                    assert_eq!(storage_balance.total, storage_balance_min());
                    assert_eq!(storage_balance.available, 0.into());

                    // Assert account NEAR balance was persisted
                    let storage_balance_2 = service
                        .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                        .unwrap();
                    assert_eq!(storage_balance, storage_balance_2);

                    // check refund was sent
                    let receipts = deserialize_receipts();
                    let receipt = &receipts[0];
                    assert_eq!(&receipt.receiver_id, PREDECESSOR_ACCOUNT_ID);
                    let action = &receipt.actions[0];
                    match action {
                        Action::Transfer(transfer) => {
                            assert_eq!(transfer.deposit, storage_balance_min().value() + 1);
                        }
                        _ => panic!("expected TransferAction"),
                    }

                    // check account stats
                    let stats = AccountManager::account_metrics();
                    assert_eq!(stats.total_near_balance, storage_balance.total);
                },
            );
        }

        #[test]
        fn withdraw_zero() {
            run_test(
                STORAGE_USAGE_BOUNDS,
                storage_balance_min() * 2,
                1.into(),
                Some(0.into()),
                |service, storage_balance| {
                    assert_eq!(storage_balance.total, storage_balance_min() * 2);
                    assert_eq!(storage_balance.available, storage_balance_min());

                    // Assert account NEAR balance was persisted
                    let storage_balance_2 = service
                        .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                        .unwrap();
                    assert_eq!(storage_balance, storage_balance_2);

                    // check refund was sent
                    let receipts = deserialize_receipts();
                    assert!(receipts.is_empty());

                    // check account stats
                    let stats = AccountManager::account_metrics();
                    assert_eq!(stats.total_near_balance, storage_balance.total);
                },
            );
        }

        #[test]
        #[should_panic(expected = "[ERR] [YOCTONEAR_DEPOSIT_REQUIRED]")]
        fn no_attached_deposit() {
            run_test(
                STORAGE_USAGE_BOUNDS,
                storage_balance_min() * 2,
                0.into(),
                Some(0.into()),
                |_service, _storage_balance| {},
            );
        }

        #[test]
        #[should_panic(expected = "[ERR] [YOCTONEAR_DEPOSIT_REQUIRED]")]
        fn two_yoctonear_attached() {
            run_test(
                STORAGE_USAGE_BOUNDS,
                storage_balance_min() * 2,
                2.into(),
                Some(0.into()),
                |_service, _storage_balance| {},
            );
        }

        #[test]
        #[should_panic(expected = "[ERR] [INSUFFICIENT_STORAGE_BALANCE]")]
        fn insufficient_funds() {
            run_test(
                STORAGE_USAGE_BOUNDS,
                storage_balance_min(),
                1.into(),
                Some(1.into()),
                |_service, _storage_balance| {},
            );
        }

        #[test]
        #[should_panic(expected = "[ERR] [ACCOUNT_NOT_REGISTERED]")]
        fn account_not_registered() {
            run_test(
                STORAGE_USAGE_BOUNDS,
                0.into(),
                1.into(),
                Some(0.into()),
                |_service, _storage_balance| {},
            );
        }
    }

    #[cfg(test)]
    mod test_storage_unregister_with_default_unregister_delegate {
        use super::*;

        pub type AccountManager = AccountManagementComponent<()>;

        fn run_test<F>(
            storage_usage_bounds: StorageUsageBounds,
            deposit: YoctoNear,
            unregister_deposit: YoctoNear,
            force: Option<bool>,
            test: F,
        ) where
            F: FnOnce(AccountManagementComponent<()>, bool),
        {
            let mut ctx = new_context(PREDECESSOR_ACCOUNT_ID);
            testing_env!(ctx.clone());

            AccountMetrics::register_account_storage_event_handler();
            AccountMetrics::reset();
            StorageManagementEvent::clear_event_handlers();

            AccountStorageUsageComponent::deploy(storage_usage_bounds);

            let mut service: AccountManagementComponent<()> =
                AccountManagementComponent::new(Default::default());

            if deposit.value() > 0 {
                ctx.attached_deposit = deposit.value();
                testing_env!(ctx.clone());
                service.storage_deposit(None, None);
            }

            ctx.attached_deposit = unregister_deposit.value();
            testing_env!(ctx.clone());
            StorageManagementEvent::clear_event_handlers();
            let result = service.storage_unregister(force);
            test(service, result);
        }

        #[test]
        fn unregister_force_none_success() {
            run_test(
                STORAGE_USAGE_BOUNDS,
                storage_balance_min() * 2,
                1.into(),
                None,
                |service, unregistered| {
                    assert!(unregistered);

                    // Assert account NEAR balance was persisted
                    let storage_balance =
                        service.storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID));
                    assert!(storage_balance.is_none());

                    // check refund was sent
                    let receipts = deserialize_receipts();
                    let receipt = &receipts[0];
                    assert_eq!(&receipt.receiver_id, PREDECESSOR_ACCOUNT_ID);
                    let action = &receipt.actions[0];
                    match action {
                        Action::Transfer(transfer) => {
                            assert_eq!(transfer.deposit, storage_balance_min().value() * 2 + 1);
                        }
                        _ => panic!("expected TransferAction"),
                    }

                    // check account stats
                    let stats = AccountManager::account_metrics();
                    assert_eq!(stats.total_registered_accounts, 0.into());
                    assert_eq!(stats.total_near_balance, 0.into());
                    assert_eq!(stats.total_storage_usage, 0.into());

                    assert!(service
                        .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                        .is_none());
                },
            );
        }

        #[test]
        fn unregister_force_success() {
            run_test(
                STORAGE_USAGE_BOUNDS,
                storage_balance_min() * 2,
                1.into(),
                Some(true),
                |service, unregistered| {
                    assert!(unregistered);

                    // Assert account NEAR balance was persisted
                    let storage_balance =
                        service.storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID));
                    assert!(storage_balance.is_none());

                    // check refund was sent
                    let receipts = deserialize_receipts();
                    let receipt = &receipts[0];
                    assert_eq!(&receipt.receiver_id, PREDECESSOR_ACCOUNT_ID);
                    let action = &receipt.actions[0];
                    match action {
                        Action::Transfer(transfer) => {
                            assert_eq!(transfer.deposit, storage_balance_min().value() * 2 + 1);
                        }
                        _ => panic!("expected TransferAction"),
                    }

                    // check account stats
                    let stats = AccountManager::account_metrics();
                    assert_eq!(stats.total_registered_accounts, 0.into());
                    assert_eq!(stats.total_near_balance, 0.into());
                    assert_eq!(stats.total_storage_usage, 0.into());

                    assert!(service
                        .storage_balance_of(to_valid_account_id(PREDECESSOR_ACCOUNT_ID))
                        .is_none());
                },
            );
        }

        #[test]
        #[should_panic(expected = "[ERR] [YOCTONEAR_DEPOSIT_REQUIRED]")]
        fn no_attached_deposit() {
            run_test(
                STORAGE_USAGE_BOUNDS,
                storage_balance_min() * 2,
                0.into(),
                None,
                |_service, _storage_balance| {},
            );
        }

        #[test]
        #[should_panic(expected = "[ERR] [YOCTONEAR_DEPOSIT_REQUIRED]")]
        fn two_yoctonear_attached() {
            run_test(
                STORAGE_USAGE_BOUNDS,
                storage_balance_min() * 2,
                2.into(),
                None,
                |_service, _storage_balance| {},
            );
        }

        #[test]
        fn account_not_registered() {
            run_test(
                STORAGE_USAGE_BOUNDS,
                0.into(),
                1.into(),
                None,
                |_service, unregistered| assert!(!unregistered),
            );
        }
    }

    #[cfg(test)]
    mod test_storage_unregister {
        use super::*;
        use oysterpack_smart_near::YOCTO;

        pub type AccountManager = AccountManagementComponent<()>;

        fn on_unregister_panic(event: &StorageManagementEvent) {
            match event {
                StorageManagementEvent::PreUnregister { force, .. } => {
                    println!("force = {}", force);
                    ERR_CODE_UNREGISTER_FAILURE.assert(|| *force, || "BOOM");
                }
                _ => {}
            }
        }

        #[test]
        #[should_panic(expected = "[ERR] [UNREGISTER_FAILURE]")]
        fn unregister_panics() {
            // Arrange
            let account = "alfio";
            let mut ctx = new_context(account);
            testing_env!(ctx.clone());

            AccountManager::deploy(AccountManagementComponentConfig {
                admin_account: to_valid_account_id("admin"),
                storage_usage_bounds: Some(StorageUsageBounds {
                    min: 1000.into(),
                    max: None,
                }),
                component_account_storage_mins: None,
            });

            let mut service = AccountManager::new(Default::default());
            ctx.attached_deposit = YOCTO;
            testing_env!(ctx.clone());
            service.storage_deposit(None, None);

            // Act
            ctx.attached_deposit = 1;
            testing_env!(ctx.clone());
            eventbus::register(on_unregister_panic);
            service.storage_unregister(None);
        }

        #[test]
        fn force_unregister_panics() {
            // Arrange
            let account = "alfio";
            let mut ctx = new_context(account);
            testing_env!(ctx.clone());

            AccountManager::deploy(AccountManagementComponentConfig {
                storage_usage_bounds: Some(StorageUsageBounds {
                    min: 1000.into(),
                    max: None,
                }),
                admin_account: to_valid_account_id("admin"),
                component_account_storage_mins: None,
            });
            eventbus::register(on_unregister_panic);

            let mut service = AccountManager::new(Default::default());
            ctx.attached_deposit = YOCTO;
            testing_env!(ctx.clone());
            service.storage_deposit(None, None);

            // Act
            ctx.attached_deposit = 1;
            testing_env!(ctx.clone());
            service.storage_unregister(Some(true));
            assert!(service
                .storage_balance_of(to_valid_account_id(account))
                .is_none());
        }
    }
}

#[cfg(test)]
mod tests_account_storage_usage {
    use super::*;
    use oysterpack_smart_near::near_sdk;
    use oysterpack_smart_near::YOCTO;
    use oysterpack_smart_near_test::*;

    type AccountManager = AccountManagementComponent<()>;

    #[test]
    fn test() {
        let account = "alfio";
        let mut ctx = new_context(account);
        testing_env!(ctx.clone());

        let storage_usage_bounds = StorageUsageBounds {
            min: AccountManager::measure_storage_usage(()),
            max: None,
        };
        println!("measured storage_usage_bounds = {:?}", storage_usage_bounds);
        AccountManager::deploy(AccountManagementComponentConfig {
            storage_usage_bounds: Some(storage_usage_bounds),
            admin_account: to_valid_account_id("admin"),
            component_account_storage_mins: None,
        });

        let mut service = AccountManager::new(Default::default());
        assert_eq!(storage_usage_bounds, service.ops_storage_usage_bounds());

        assert!(service
            .storage_balance_of(to_valid_account_id(account))
            .is_none());

        ctx.attached_deposit = YOCTO;
        testing_env!(ctx.clone());
        let storage_balance = service.storage_deposit(None, None);
        assert_eq!(
            service
                .storage_balance_of(to_valid_account_id(account))
                .unwrap(),
            storage_balance
        );
        let storage_usage = service
            .ops_storage_usage(to_valid_account_id(account))
            .unwrap();
        assert_eq!(
            storage_usage,
            service
                .registered_account_near_data(account)
                .storage_usage()
        );
    }
}

#[cfg(test)]
mod tests_account_metrics {
    use super::*;
    use oysterpack_smart_near::near_sdk::{self, test_utils};
    use oysterpack_smart_near::YOCTO;
    use oysterpack_smart_near_test::*;

    type AccountManager = AccountManagementComponent<()>;

    #[test]
    fn test() {
        StorageManagementEvent::clear_event_handlers();
        // Arrange - 0 accounts register
        let account = "alfio";
        let mut ctx = new_context(account);
        testing_env!(ctx.clone());

        let metrics = AccountManager::account_metrics();
        println!("{:?}", metrics);
        let storage_usage_bounds = StorageUsageBounds {
            min: AccountManager::measure_storage_usage(()),
            max: None,
        };
        let metrics = AccountManager::account_metrics();
        println!("{:?}", metrics);
        println!("measured storage_usage_bounds = {:?}", storage_usage_bounds);

        let metrics = AccountManager::account_metrics();
        println!("before deploy: {:?}", metrics);
        AccountManager::deploy(AccountManagementComponentConfig {
            storage_usage_bounds: Some(storage_usage_bounds),
            admin_account: to_valid_account_id("admin"),
            component_account_storage_mins: None,
        });
        let metrics = AccountManager::account_metrics();
        println!("after deploy: {:?}", metrics);
        println!("{:#?}", test_utils::get_logs());

        let mut service = AccountManager::new(Default::default());
        let admin_account = service.registered_account_near_data("admin");
        // Act
        let metrics = AccountManager::account_metrics();
        println!("{:?}", metrics);
        // Assert
        assert_eq!(metrics.total_registered_accounts.value(), 1);
        assert_eq!(metrics.total_near_balance, admin_account.near_balance());
        assert_eq!(metrics.total_storage_usage, admin_account.storage_usage());

        // Arrange - register account
        ctx.attached_deposit = YOCTO;
        testing_env!(ctx.clone());
        let storage_balance = service.storage_deposit(None, None);
        let account_data = service.load_account_data(account);
        assert!(account_data.is_none());
        let mut account_data: AccountDataObject<()> = AccountDataObject::new(account, ());
        account_data.save();
        let mut account_near_data = service.registered_account_near_data(account);
        account_near_data.grant_operator();
        account_near_data.save();

        // Act
        let metrics = AccountManager::account_metrics();
        // Assert
        assert_eq!(metrics.total_registered_accounts.value(), 2);
        assert_eq!(
            metrics.total_near_balance,
            storage_balance.total + admin_account.near_balance()
        );
        assert_eq!(
            metrics.total_storage_usage,
            storage_usage_bounds.min + admin_account.storage_usage()
        );

        // Arrange - deposit more funds
        ctx.attached_deposit = YOCTO;
        testing_env!(ctx.clone());
        let storage_balance = service.storage_deposit(None, None);
        // Act
        let metrics = AccountManager::account_metrics();
        // Assert
        assert_eq!(metrics.total_registered_accounts.value(), 2);
        assert_eq!(
            metrics.total_near_balance,
            storage_balance.total + admin_account.near_balance()
        );
        assert_eq!(
            metrics.total_storage_usage,
            storage_usage_bounds.min + admin_account.storage_usage()
        );

        // Arrange - register another account
        ctx.attached_deposit = YOCTO;
        testing_env!(ctx.clone());
        let bob_storage_balance = service.storage_deposit(Some(to_valid_account_id("bob")), None);
        let mut account_data: AccountDataObject<()> = AccountDataObject::new("bob", ());
        account_data.save();
        let mut account_near_data = service.registered_account_near_data("bob");
        account_near_data.grant_operator();
        account_near_data.save();
        // Act
        let metrics = AccountManager::account_metrics();
        // Assert
        assert_eq!(metrics.total_registered_accounts.value(), 3);
        assert_eq!(
            metrics.total_near_balance,
            storage_balance.total + bob_storage_balance.total + admin_account.near_balance()
        );
        assert_eq!(
            metrics.total_storage_usage.value(),
            (storage_usage_bounds.min.value() * 2) + admin_account.storage_usage().value()
        );

        // Arrange - unregister account
        ctx.attached_deposit = 1;
        testing_env!(ctx.clone());
        StorageManagementEvent::clear_event_handlers();
        service.storage_unregister(None);
        // Act
        let metrics = AccountManager::account_metrics();
        // Assert
        assert_eq!(metrics.total_registered_accounts.value(), 2);
        assert_eq!(
            metrics.total_near_balance,
            bob_storage_balance.total + admin_account.near_balance()
        );
        assert_eq!(
            metrics.total_storage_usage,
            storage_usage_bounds.min + admin_account.storage_usage()
        );
    }
}

#[cfg(test)]
mod test_permission_management {
    use super::*;
    use oysterpack_smart_near::near_sdk;
    use oysterpack_smart_near::near_sdk::{test_utils, VMContext};
    use oysterpack_smart_near::YOCTO;
    use oysterpack_smart_near_test::*;
    use std::convert::TryInto;

    type AccountManager = AccountManagementComponent<()>;

    const PREDECESSOR_ACCOUNT: &str = "predecessor";

    const PERM_0: u64 = 1 << 0;
    const PERM_1: u64 = 1 << 1;
    const PERMISSIONS: [(u8, &'static str); 2] = [(0, "perm_0"), (1, "perm_1")];

    fn permissions() -> ContractPermissions {
        let perms: Vec<(u8, &'static str)> = PERMISSIONS.try_into().unwrap();
        perms.into()
    }

    /// if admin is true, then the predecessor account is granted admin permission.
    fn test<F>(admin: bool, permissions: ContractPermissions, f: F)
    where
        F: FnOnce(VMContext, AccountManager),
    {
        let mut ctx = new_context(PREDECESSOR_ACCOUNT);
        ctx.predecessor_account_id = PREDECESSOR_ACCOUNT.to_string();
        testing_env!(ctx.clone());

        let storage_usage_bounds = StorageUsageBounds {
            min: AccountManager::measure_storage_usage(()),
            max: None,
        };
        AccountManager::deploy(AccountManagementComponentConfig {
            storage_usage_bounds: Some(storage_usage_bounds),
            admin_account: to_valid_account_id("admin"),
            component_account_storage_mins: None,
        });

        let mut account_manager = AccountManager::new(permissions);

        {
            let mut ctx = ctx.clone();
            ctx.attached_deposit = YOCTO;
            testing_env!(ctx);
            account_manager.storage_deposit(None, None);
        }

        if admin {
            let mut account = account_manager.registered_account_near_data(PREDECESSOR_ACCOUNT);
            account.grant_admin();
            account.save();
        }

        f(ctx, account_manager);
    }

    #[cfg(test)]
    mod as_admin {
        use super::*;

        #[cfg(test)]
        mod ops_permissions_is_admin {
            use super::*;

            #[test]
            fn basic_grants_revokes() {
                test(true, permissions(), |mut ctx, mut account_manager| {
                    // Arrange
                    let bob = "bob";
                    ctx.predecessor_account_id = bob.to_string();
                    ctx.attached_deposit = YOCTO;
                    testing_env!(ctx.clone());
                    account_manager.storage_deposit(None, None);

                    ctx.predecessor_account_id = PREDECESSOR_ACCOUNT.to_string();
                    ctx.attached_deposit = 0;
                    testing_env!(ctx.clone());

                    // account with admin permission implies all permissions
                    assert!(account_manager
                        .ops_permissions_is_operator(to_valid_account_id(PREDECESSOR_ACCOUNT)));
                    assert!(account_manager.ops_permissions_contains(
                        to_valid_account_id(PREDECESSOR_ACCOUNT),
                        PERM_0.into()
                    ));
                    let accounts_perms = account_manager
                        .ops_permissions_granted(to_valid_account_id(PREDECESSOR_ACCOUNT))
                        .unwrap();
                    assert_eq!(accounts_perms.len(), 1);
                    assert_eq!(accounts_perms.get(&63).unwrap(), "admin");

                    // grant admin
                    account_manager.ops_permissions_grant_admin(to_valid_account_id(bob));
                    assert!(account_manager.ops_permissions_is_admin(to_valid_account_id(bob)));
                    assert!(account_manager.ops_permissions_is_operator(to_valid_account_id(bob)));
                    assert!(account_manager
                        .ops_permissions_contains(to_valid_account_id(bob), PERM_0.into()));

                    // revoke admin
                    account_manager.ops_permissions_revoke_admin(to_valid_account_id(bob));
                    assert!(!account_manager.ops_permissions_is_admin(to_valid_account_id(bob)));
                    assert!(!account_manager.ops_permissions_is_operator(to_valid_account_id(bob)));
                    let accounts_perms =
                        account_manager.ops_permissions_granted(to_valid_account_id(bob));
                    println!("accounts_perms = {:?}", accounts_perms);
                    assert!(accounts_perms.is_none());

                    // grant operator
                    account_manager.ops_permissions_grant_operator(to_valid_account_id(bob));
                    assert!(account_manager.ops_permissions_is_operator(to_valid_account_id(bob)));
                    assert!(!account_manager.ops_permissions_is_admin(to_valid_account_id(bob)));
                    let accounts_perms = account_manager
                        .ops_permissions_granted(to_valid_account_id(bob))
                        .unwrap();
                    assert_eq!(accounts_perms.len(), 1);
                    assert_eq!(accounts_perms.get(&62).unwrap(), "operator");

                    // revoke operator
                    account_manager.ops_permissions_revoke_operator(to_valid_account_id(bob));
                    assert!(!account_manager.ops_permissions_is_operator(to_valid_account_id(bob)));

                    // grant permissions
                    account_manager
                        .ops_permissions_grant(to_valid_account_id(bob), (PERM_0 | PERM_1).into());
                    assert!(account_manager
                        .ops_permissions_contains(to_valid_account_id(bob), PERM_0.into()));
                    assert!(account_manager
                        .ops_permissions_contains(to_valid_account_id(bob), PERM_1.into()));
                    assert!(account_manager.ops_permissions_contains(
                        to_valid_account_id(bob),
                        (PERM_0 | PERM_1).into()
                    ));
                    let accounts_perms = account_manager
                        .ops_permissions_granted(to_valid_account_id(bob))
                        .unwrap();
                    assert_eq!(accounts_perms.len(), 2);
                    assert_eq!(accounts_perms.get(&0).unwrap(), "perm_0");
                    assert_eq!(accounts_perms.get(&1).unwrap(), "perm_1");

                    // revoke permissions
                    account_manager.ops_permissions_revoke(to_valid_account_id(bob), PERM_0.into());
                    assert!(!account_manager
                        .ops_permissions_contains(to_valid_account_id(bob), PERM_0.into()));
                    assert!(account_manager
                        .ops_permissions_contains(to_valid_account_id(bob), PERM_1.into()));
                    assert!(!account_manager.ops_permissions_contains(
                        to_valid_account_id(bob),
                        (PERM_0 | PERM_1).into()
                    ));

                    account_manager.ops_permissions_revoke(to_valid_account_id(bob), PERM_1.into());
                    assert!(!account_manager
                        .ops_permissions_contains(to_valid_account_id(bob), PERM_0.into()));
                    assert!(!account_manager
                        .ops_permissions_contains(to_valid_account_id(bob), PERM_1.into()));
                    assert!(!account_manager.ops_permissions_contains(
                        to_valid_account_id(bob),
                        (PERM_0 | PERM_1).into()
                    ));

                    // grant permissions
                    account_manager
                        .ops_permissions_grant(to_valid_account_id(bob), (PERM_0 | PERM_1).into());
                    account_manager.ops_permissions_grant_operator(to_valid_account_id(bob));
                    assert!(account_manager
                        .ops_permissions_contains(to_valid_account_id(bob), PERM_0.into()));
                    assert!(account_manager
                        .ops_permissions_contains(to_valid_account_id(bob), PERM_1.into()));
                    assert!(account_manager.ops_permissions_contains(
                        to_valid_account_id(bob),
                        (PERM_0 | PERM_1).into()
                    ));
                    assert!(account_manager.ops_permissions_is_operator(to_valid_account_id(bob)));
                    let accounts_perms = account_manager
                        .ops_permissions_granted(to_valid_account_id(bob))
                        .unwrap();
                    assert_eq!(accounts_perms.len(), 3);
                    assert_eq!(accounts_perms.get(&0).unwrap(), "perm_0");
                    assert_eq!(accounts_perms.get(&1).unwrap(), "perm_1");
                    assert_eq!(accounts_perms.get(&62).unwrap(), "operator");

                    // revoke all permissions
                    account_manager.ops_permissions_revoke_all(to_valid_account_id(bob));
                    assert!(account_manager
                        .ops_permissions(to_valid_account_id(bob))
                        .is_none());
                    assert!(account_manager
                        .ops_permissions_granted(to_valid_account_id(bob))
                        .is_none());

                    let logs = test_utils::get_logs();
                    println!("{:#?}", logs);
                });
            }

            #[test]
            fn account_not_registered() {
                test(true, Default::default(), |_ctx, account_manager| {
                    assert!(!account_manager.ops_permissions_is_admin(to_valid_account_id("bob")));
                    assert!(
                        !account_manager.ops_permissions_is_operator(to_valid_account_id("bob"))
                    );
                    assert!(!account_manager
                        .ops_permissions_contains(to_valid_account_id("bob"), (1 << 10).into()));
                    assert!(account_manager
                        .ops_permissions(to_valid_account_id("bob"))
                        .is_none());
                });
            }

            #[cfg(test)]
            mod grant_to_self {
                use super::*;

                #[test]
                #[should_panic(
                    expected = "[ERR] [INVALID] `account_id` cannot be the same as the predecessor account ID"
                )]
                fn grant_admin_to_self() {
                    test(true, Default::default(), |_ctx, mut account_manager| {
                        account_manager
                            .ops_permissions_grant_admin(to_valid_account_id(PREDECESSOR_ACCOUNT));
                    });
                }

                #[test]
                #[should_panic(
                    expected = "[ERR] [INVALID] `account_id` cannot be the same as the predecessor account ID"
                )]
                fn grant_operator_to_self() {
                    test(true, Default::default(), |_ctx, mut account_manager| {
                        account_manager.ops_permissions_grant_operator(to_valid_account_id(
                            PREDECESSOR_ACCOUNT,
                        ));
                    });
                }

                #[test]
                #[should_panic(
                    expected = "[ERR] [INVALID] `account_id` cannot be the same as the predecessor account ID"
                )]
                fn grant_to_self() {
                    test(true, permissions(), |_ctx, mut account_manager| {
                        account_manager.ops_permissions_grant(
                            to_valid_account_id(PREDECESSOR_ACCOUNT),
                            PERM_0.into(),
                        );
                    });
                }

                #[test]
                #[should_panic(
                    expected = "[ERR] [INVALID] `account_id` cannot be the same as the predecessor account ID"
                )]
                fn revoke_admin_to_self() {
                    test(true, Default::default(), |_ctx, mut account_manager| {
                        account_manager
                            .ops_permissions_revoke_admin(to_valid_account_id(PREDECESSOR_ACCOUNT));
                    });
                }

                #[test]
                #[should_panic(
                    expected = "[ERR] [INVALID] `account_id` cannot be the same as the predecessor account ID"
                )]
                fn revoke_operator_to_self() {
                    test(true, Default::default(), |_ctx, mut account_manager| {
                        account_manager.ops_permissions_revoke_operator(to_valid_account_id(
                            PREDECESSOR_ACCOUNT,
                        ));
                    });
                }

                #[test]
                #[should_panic(
                    expected = "[ERR] [INVALID] `account_id` cannot be the same as the predecessor account ID"
                )]
                fn revoke_to_self() {
                    test(true, permissions(), |_ctx, mut account_manager| {
                        account_manager.ops_permissions_revoke(
                            to_valid_account_id(PREDECESSOR_ACCOUNT),
                            PERM_0.into(),
                        );
                    });
                }

                #[test]
                #[should_panic(
                    expected = "[ERR] [INVALID] `account_id` cannot be the same as the predecessor account ID"
                )]
                fn revoke_all_to_self() {
                    test(true, permissions(), |_ctx, mut account_manager| {
                        account_manager
                            .ops_permissions_revoke_all(to_valid_account_id(PREDECESSOR_ACCOUNT));
                    });
                }
            }

            #[cfg(test)]
            mod test_log_events {
                use super::*;

                #[test]
                fn grant_revoke_admin() {
                    test(true, permissions(), |ctx, mut account_manager| {
                        // Arrange
                        let bob = "bob";
                        {
                            // register account
                            let mut ctx = ctx.clone();
                            ctx.attached_deposit = YOCTO;
                            testing_env!(ctx.clone());
                            account_manager
                                .storage_deposit(Some(to_valid_account_id(bob)), Some(true));
                        }

                        // Act - grant
                        testing_env!(ctx.clone());
                        account_manager.ops_permissions_grant_admin(to_valid_account_id(bob));
                        let logs = test_utils::get_logs();
                        println!("{:#?}", logs);
                        assert_eq!(logs.len(), 2);
                        assert_eq!(
                            &logs[0],
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(8)"
                        );
                        assert_eq!(&logs[1], "[INFO] [PERMISSIONS_GRANT] admin");

                        // Act - grant admin again to user should have no effect
                        testing_env!(ctx.clone());
                        account_manager.ops_permissions_grant_admin(to_valid_account_id(bob));
                        let logs = test_utils::get_logs();
                        assert!(logs.is_empty());

                        // Act - revoke
                        testing_env!(ctx.clone());
                        account_manager.ops_permissions_revoke_admin(to_valid_account_id(bob));
                        let logs = test_utils::get_logs();
                        println!("{:#?}", logs);
                        assert_eq!(logs.len(), 2);
                        assert_eq!(
                            &logs[0],
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-8)"
                        );
                        assert_eq!(&logs[1], "[INFO] [PERMISSIONS_REVOKE] admin");

                        // Act - revoke again
                        testing_env!(ctx.clone());
                        account_manager.ops_permissions_revoke_admin(to_valid_account_id(bob));
                        let logs = test_utils::get_logs();
                        assert!(logs.is_empty());
                    });
                }

                #[test]
                fn grant_revoke_operator() {
                    test(true, permissions(), |ctx, mut account_manager| {
                        // Arrange
                        let bob = "bob";
                        {
                            // register account
                            let mut ctx = ctx.clone();
                            ctx.attached_deposit = YOCTO;
                            testing_env!(ctx.clone());
                            account_manager
                                .storage_deposit(Some(to_valid_account_id(bob)), Some(true));
                        }

                        // Act - grant
                        testing_env!(ctx.clone());
                        account_manager.ops_permissions_grant_operator(to_valid_account_id(bob));
                        let logs = test_utils::get_logs();
                        println!("{:#?}", logs);
                        assert_eq!(logs.len(), 2);
                        assert_eq!(
                            &logs[0],
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(8)"
                        );
                        assert_eq!(&logs[1], "[INFO] [PERMISSIONS_GRANT] operator");

                        // Act - grant admin again to user should have no effect
                        testing_env!(ctx.clone());
                        account_manager.ops_permissions_grant_operator(to_valid_account_id(bob));
                        let logs = test_utils::get_logs();
                        assert!(logs.is_empty());

                        // Act - revoke
                        testing_env!(ctx.clone());
                        account_manager.ops_permissions_revoke_operator(to_valid_account_id(bob));
                        let logs = test_utils::get_logs();
                        println!("{:#?}", logs);
                        assert_eq!(logs.len(), 2);
                        assert_eq!(
                            &logs[0],
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-8)"
                        );
                        assert_eq!(&logs[1], "[INFO] [PERMISSIONS_REVOKE] operator");

                        // Act - revoke again
                        testing_env!(ctx.clone());
                        account_manager.ops_permissions_revoke_operator(to_valid_account_id(bob));
                        let logs = test_utils::get_logs();
                        assert!(logs.is_empty());
                    });
                }

                #[test]
                fn grant_revoke_contract_permissions() {
                    test(true, permissions(), |ctx, mut account_manager| {
                        // Arrange
                        let bob = "bob";
                        {
                            // register account
                            let mut ctx = ctx.clone();
                            ctx.attached_deposit = YOCTO;
                            testing_env!(ctx.clone());
                            account_manager
                                .storage_deposit(Some(to_valid_account_id(bob)), Some(true));
                        }

                        // Act - grant
                        testing_env!(ctx.clone());
                        account_manager
                            .ops_permissions_grant(to_valid_account_id(bob), PERM_0.into());
                        let logs = test_utils::get_logs();
                        println!("{:#?}", logs);
                        assert_eq!(logs.len(), 2);
                        assert_eq!(
                            &logs[0],
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(8)"
                        );
                        assert_eq!(&logs[1], "[INFO] [PERMISSIONS_GRANT] [\"perm_0\"]");

                        // Act - grant admin again to user should have no effect
                        testing_env!(ctx.clone());
                        account_manager
                            .ops_permissions_grant(to_valid_account_id(bob), PERM_0.into());
                        let logs = test_utils::get_logs();
                        assert!(logs.is_empty());

                        testing_env!(ctx.clone());
                        account_manager
                            .ops_permissions_grant(to_valid_account_id(bob), PERM_1.into());
                        let logs = test_utils::get_logs();
                        println!("{:#?}", logs);
                        assert_eq!(logs.len(), 1);
                        assert_eq!(&logs[0], "[INFO] [PERMISSIONS_GRANT] [\"perm_1\"]");

                        // Act - revoke
                        testing_env!(ctx.clone());
                        account_manager.ops_permissions_revoke(
                            to_valid_account_id(bob),
                            (PERM_0 | PERM_1).into(),
                        );
                        let logs = test_utils::get_logs();
                        println!("{:#?}", logs);
                        assert_eq!(logs.len(), 2);
                        assert_eq!(
                            &logs[0],
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-8)"
                        );
                        assert_eq!(
                            &logs[1],
                            "[INFO] [PERMISSIONS_REVOKE] [\"perm_0\", \"perm_1\"]"
                        );

                        // Act - revoke again
                        testing_env!(ctx.clone());
                        account_manager
                            .ops_permissions_revoke(to_valid_account_id(bob), PERM_0.into());
                        let logs = test_utils::get_logs();
                        assert!(logs.is_empty());

                        // Act - grant
                        testing_env!(ctx.clone());
                        account_manager.ops_permissions_grant(
                            to_valid_account_id(bob),
                            (PERM_0 | PERM_1).into(),
                        );

                        testing_env!(ctx.clone());
                        account_manager.ops_permissions_revoke_all(to_valid_account_id(bob));
                        let logs = test_utils::get_logs();
                        println!("{:#?}", logs);
                        assert_eq!(logs.len(), 2);
                        assert_eq!(
                            &logs[0],
                            "[INFO] [ACCOUNT_STORAGE_CHANGED] StorageUsageChange(-8)"
                        );
                        assert_eq!(
                            &logs[1],
                            "[INFO] [PERMISSIONS_REVOKE] all permissions were revoked"
                        );

                        testing_env!(ctx.clone());
                        account_manager.ops_permissions_grant_permissions(
                            to_valid_account_id(bob),
                            vec![0, 1],
                        );
                        let logs = test_utils::get_logs();
                        println!("{:#?}", logs);
                        assert_eq!(logs.len(), 2);
                        assert_eq!(
                            &logs[1],
                            "[INFO] [PERMISSIONS_GRANT] [\"perm_0\", \"perm_1\"]"
                        );

                        account_manager.ops_permissions_revoke_permissions(
                            to_valid_account_id(bob),
                            vec![0, 1],
                        );
                        assert!(account_manager
                            .ops_permissions(to_valid_account_id(bob))
                            .is_none());
                    });
                }
            }
        }
    }

    #[cfg(test)]
    mod not_as_admin {
        use super::*;

        #[test]
        #[should_panic(expected = "[ERR] [NOT_AUTHORIZED]")]
        fn grant_admin() {
            test(false, Default::default(), |_ctx, mut account_manager| {
                account_manager.ops_permissions_grant_admin(to_valid_account_id("bob"));
            });
        }

        #[test]
        #[should_panic(expected = "[ERR] [NOT_AUTHORIZED]")]
        fn grant_operator() {
            test(false, Default::default(), |_ctx, mut account_manager| {
                account_manager.ops_permissions_grant_operator(to_valid_account_id("bob"));
            });
        }

        #[test]
        #[should_panic(expected = "[ERR] [NOT_AUTHORIZED]")]
        fn grant() {
            test(false, permissions(), |_ctx, mut account_manager| {
                account_manager.ops_permissions_grant(to_valid_account_id("bob"), (1 << 1).into());
            });
        }

        #[test]
        #[should_panic(expected = "[ERR] [NOT_AUTHORIZED]")]
        fn revoke_admin() {
            test(false, Default::default(), |_ctx, mut account_manager| {
                account_manager.ops_permissions_revoke_admin(to_valid_account_id("bob"));
            });
        }

        #[test]
        #[should_panic(expected = "[ERR] [NOT_AUTHORIZED]")]
        fn revoke_operator() {
            test(false, Default::default(), |_ctx, mut account_manager| {
                account_manager.ops_permissions_revoke_operator(to_valid_account_id("bob"));
            });
        }

        #[test]
        #[should_panic(expected = "[ERR] [NOT_AUTHORIZED]")]
        fn revoke() {
            test(false, permissions(), |_ctx, mut account_manager| {
                account_manager.ops_permissions_revoke(to_valid_account_id("bob"), (1 << 1).into());
            });
        }

        #[test]
        #[should_panic(expected = "[ERR] [NOT_AUTHORIZED]")]
        fn revoke_all() {
            test(false, permissions(), |_ctx, mut account_manager| {
                account_manager.ops_permissions_revoke_all(to_valid_account_id("bob"));
            });
        }
    }

    #[cfg(test)]
    mod contract_permission_bits {
        use super::*;

        #[test]
        fn no_contract_permissions() {
            test(false, Default::default(), |_, account_manager| {
                assert!(account_manager
                    .ops_permissions_contract_permissions()
                    .is_none());
            });
        }

        #[test]
        fn with_contract_permissions() {
            test(false, permissions(), |_, account_manager| {
                let permissions = account_manager
                    .ops_permissions_contract_permissions()
                    .unwrap();
                assert_eq!(permissions.len(), 2);
                assert_eq!(permissions.get(&0).unwrap(), "perm_0");
                assert_eq!(permissions.get(&1).unwrap(), "perm_1");

                for (k, v) in permissions {
                    assert_eq!(account_manager.permission_by_name(&v).unwrap(), 1 << k);
                }
            });
        }
    }
}
