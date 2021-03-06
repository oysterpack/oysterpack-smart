use oysterpack_smart_near::domain::StorageUsage;
use oysterpack_smart_near::near_sdk::json_types::ValidAccountId;

use crate::{AccountNearDataObject, AccountStorageUsage, StorageUsageBounds};
use oysterpack_smart_near::component::{Component, Deploy};

#[derive(Default)]
pub(crate) struct AccountStorageUsageComponent;

impl AccountStorageUsage for AccountStorageUsageComponent {
    fn ops_storage_usage_bounds(&self) -> StorageUsageBounds {
        *Self::load_state().expect("requires deployment")
    }

    fn ops_storage_usage(&self, account_id: ValidAccountId) -> Option<StorageUsage> {
        AccountNearDataObject::load(account_id.as_ref().as_str())
            .map(|account| account.storage_usage())
    }
}

impl Component for AccountStorageUsageComponent {
    type State = StorageUsageBounds;

    const STATE_KEY: u128 = 1952475351321611295376996018476025471;
}

impl Deploy for AccountStorageUsageComponent {
    type Config = StorageUsageBounds;

    fn deploy(config: Self::Config) {
        let state = Self::new_state(config);
        state.save();
    }
}
