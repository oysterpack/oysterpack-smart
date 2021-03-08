use crate::AccountStorageEvent;
use near_sdk::{
    borsh::{self, BorshDeserialize, BorshSerialize},
    env,
    serde::{Deserialize, Serialize},
};
use oysterpack_smart_near::data::numbers::U128;
use oysterpack_smart_near::domain::StorageUsage;
use oysterpack_smart_near::{data::Object, domain::YoctoNear};

const ACCOUNT_STATS_KEY: u128 = 1952364736129901845182088441739779955;

type AccountStatsObject = Object<u128, AccountStats>;

/// Account statistics
#[derive(
    BorshSerialize, BorshDeserialize, Deserialize, Serialize, Copy, Clone, Debug, PartialEq, Default,
)]
#[serde(crate = "near_sdk::serde")]
pub struct AccountStats {
    pub total_registered_accounts: U128,
    pub total_near_balance: YoctoNear,
    pub total_storage_usage: StorageUsage,
}

impl AccountStats {
    pub fn load() -> AccountStats {
        let stats = AccountStatsObject::load(&ACCOUNT_STATS_KEY)
            .unwrap_or_else(|| AccountStatsObject::new(ACCOUNT_STATS_KEY, AccountStats::default()));
        *stats
    }

    pub fn save(&self) {
        AccountStatsObject::new(ACCOUNT_STATS_KEY, *self).save();
    }

    pub fn on_account_storage_event(event: &AccountStorageEvent) {
        env::log(format!("{:?}", event).as_bytes());

        let mut stats = AccountStats::load();

        match event {
            AccountStorageEvent::Registered(storage_balance, storage_usage) => {
                stats.total_registered_accounts = stats
                    .total_registered_accounts
                    .checked_add(1)
                    .expect("total_registered_accounts overflow")
                    .into();

                stats.total_near_balance = stats
                    .total_near_balance
                    .checked_add(storage_balance.total.value())
                    .expect("total_near_balance overflow")
                    .into();

                stats.total_storage_usage = stats
                    .total_storage_usage
                    .checked_add(storage_usage.value())
                    .expect("total_storage_usage overflow")
                    .into();
            }

            AccountStorageEvent::Deposit(amount) => {
                stats.total_near_balance = stats
                    .total_near_balance
                    .checked_add(amount.value())
                    .expect("total_near_balance overflow")
                    .into();
            }
            AccountStorageEvent::Withdrawal(amount) => {
                stats.total_near_balance = stats
                    .total_near_balance
                    .checked_sub(amount.value())
                    .expect("total_near_balance overflow")
                    .into();
            }
            AccountStorageEvent::StorageUsageChanged(change) => {
                if change.is_positive() {
                    stats.total_storage_usage = stats
                        .total_storage_usage
                        .checked_add(change.value() as u64)
                        .expect("total_storage_usage overflow")
                        .into();
                } else {
                    stats.total_storage_usage = stats
                        .total_storage_usage
                        .checked_sub(change.value().abs() as u64)
                        .expect("total_storage_usage overflow")
                        .into();
                }
            }

            AccountStorageEvent::Unregistered(account_near_balance, account_storage_usage) => {
                stats.total_registered_accounts = stats
                    .total_registered_accounts
                    .checked_sub(1)
                    .expect("total_registered_accounts overflow")
                    .into();

                stats.total_near_balance = stats
                    .total_near_balance
                    .checked_sub(account_near_balance.value())
                    .expect("total_near_balance overflow")
                    .into();

                stats.total_storage_usage = stats
                    .total_storage_usage
                    .checked_sub(account_storage_usage.value())
                    .expect("total_storage_usage overflow")
                    .into();
            }
        }

        stats.save();
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::StorageBalance;
    use oysterpack_smart_near::domain::StorageUsageChange;
    use oysterpack_smart_near::*;
    use oysterpack_smart_near_test::*;

    #[test]
    fn on_account_storage_event() {
        // Arrange
        let account_id = "bob.near";
        let context = new_context(account_id);
        testing_env!(context);

        EVENT_BUS.register(AccountStats::on_account_storage_event);

        let stats = AccountStats::load();
        assert_eq!(stats.total_registered_accounts, 0.into());
        assert_eq!(stats.total_near_balance, 0.into());
        assert_eq!(stats.total_storage_usage, 0.into());

        // Act - account registered
        let storage_balance = StorageBalance {
            total: YOCTO.into(),
            available: 0.into(),
        };
        EVENT_BUS.post(&AccountStorageEvent::Registered(
            storage_balance,
            1000.into(),
        ));

        // Assert
        let stats = AccountStats::load();
        assert_eq!(stats.total_registered_accounts, 1.into());
        assert_eq!(stats.total_near_balance, YOCTO.into());
        assert_eq!(stats.total_storage_usage, 1000.into());

        // Act - deposit
        EVENT_BUS.post(&AccountStorageEvent::Deposit(YOCTO.into()));

        // Assert
        let stats = AccountStats::load();
        assert_eq!(stats.total_registered_accounts, 1.into());
        assert_eq!(stats.total_near_balance, (2 * YOCTO).into());
        assert_eq!(stats.total_storage_usage, 1000.into());

        // Act - withdraw
        EVENT_BUS.post(&AccountStorageEvent::Withdrawal(YOCTO.into()));

        // Assert
        let stats = AccountStats::load();
        assert_eq!(stats.total_registered_accounts, 1.into());
        assert_eq!(stats.total_near_balance, YOCTO.into());
        assert_eq!(stats.total_storage_usage, 1000.into());

        // Act - storage usage increase
        EVENT_BUS.post(&AccountStorageEvent::StorageUsageChanged(1000.into()));

        // Assert
        let stats = AccountStats::load();
        assert_eq!(stats.total_registered_accounts, 1.into());
        assert_eq!(stats.total_near_balance, YOCTO.into());
        assert_eq!(stats.total_storage_usage, 2000.into());

        // Act - storage usage decrease
        EVENT_BUS.post(&AccountStorageEvent::StorageUsageChanged(
            StorageUsageChange(-1000),
        ));

        // Assert
        let stats = AccountStats::load();
        assert_eq!(stats.total_registered_accounts, 1.into());
        assert_eq!(stats.total_near_balance, YOCTO.into());
        assert_eq!(stats.total_storage_usage, 1000.into());

        // Act - account unregistered
        EVENT_BUS.post(&AccountStorageEvent::Unregistered(
            YOCTO.into(),
            StorageUsage(1000),
        ));

        // Assert
        let stats = AccountStats::load();
        assert_eq!(stats.total_registered_accounts, 0.into());
        assert_eq!(stats.total_near_balance, 0.into());
        assert_eq!(stats.total_storage_usage, 0.into());
    }
}
