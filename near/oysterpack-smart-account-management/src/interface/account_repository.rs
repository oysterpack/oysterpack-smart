use crate::AccountNearDataObject;
use crate::{AccountDataObject, ERR_NOT_AUTHORIZED};
use oysterpack_smart_near::near_sdk::{
    borsh::{BorshDeserialize, BorshSerialize},
    env,
};
use oysterpack_smart_near::{domain::YoctoNear, ErrCode, ErrorConst};
use std::fmt::Debug;

pub type Account<T> = (AccountNearDataObject, Option<AccountDataObject<T>>);

/// Provides account data access, i.e., CRUD
pub trait AccountRepository<T>
where
    T: BorshSerialize + BorshDeserialize + Clone + Debug + PartialEq + Default,
{
    /// Creates a new account.
    ///
    /// - tracks storage usage - emits [`crate::AccountStorageEvent::StorageUsageChanged`]
    ///
    /// # Panics
    /// if the account already is registered
    fn create_account(
        &mut self,
        account_id: &str,
        near_balance: YoctoNear,
        data: Option<T>,
    ) -> Account<T>;

    /// tries to load the account from storage
    fn load_account(&self, account_id: &str) -> Option<Account<T>>;

    /// tries to load the account data from storage
    fn load_account_data(&self, account_id: &str) -> Option<AccountDataObject<T>>;

    /// tries to load the account NEAR data from storage
    fn load_account_near_data(&self, account_id: &str) -> Option<AccountNearDataObject>;

    /// Looks up the account for the specified registered account ID.
    ///
    /// ## Panics
    /// if the account is not registered
    fn registered_account(&self, account_id: &str) -> Account<T>;

    /// Looks up the account NEAR data for the specified registered account ID.
    ///
    /// ## Panics
    /// if the account is not registered
    fn registered_account_near_data(&self, account_id: &str) -> AccountNearDataObject;

    /// If the account is registered but has no data, then a default instance will be created and
    /// returned.
    ///
    /// ## Panics
    /// if the account is not registered
    fn registered_account_data(&self, account_id: &str) -> AccountDataObject<T>;

    fn account_exists(&self, account_id: &str) -> bool;

    /// Deletes [AccountNearDataObject] and [AccountDataObject] for the specified  account ID
    /// - tracks storage usage - emits [`crate::AccountStorageEvent::StorageUsageChanged`]
    fn delete_account(&mut self, account_id: &str);

    /// asserts that the predecessor account ID is registered and has operator permission
    fn assert_operator(&self) -> AccountNearDataObject {
        let account = self.registered_account_near_data(env::predecessor_account_id().as_str());
        ERR_NOT_AUTHORIZED.assert(|| account.is_operator());
        account
    }

    /// asserts that the predecessor account ID is registered and has admin permission
    fn assert_admin(&self) -> AccountNearDataObject {
        let account = self.registered_account_near_data(env::predecessor_account_id().as_str());
        ERR_NOT_AUTHORIZED.assert(|| account.is_admin());
        account
    }
}

pub const ERR_ACCOUNT_NOT_REGISTERED: ErrorConst = ErrorConst(
    ErrCode("ACCOUNT_NOT_REGISTERED"),
    "account is not registered",
);

pub const ERR_ACCOUNT_ALREADY_REGISTERED: ErrorConst = ErrorConst(
    ErrCode("ACCOUNT_ALREADY_REGISTERED"),
    "account is already registered",
);
