use near_sdk::serde::{Deserialize, Serialize};
use oysterpack_smart_near::data::numbers::U128;
use std::fmt::{self, Display, Formatter};
use std::ops::{Deref, DerefMut};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(crate = "near_sdk::serde")]
pub struct TokenAmount(pub U128);

impl Deref for TokenAmount {
    type Target = u128;

    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}

impl DerefMut for TokenAmount {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.deref_mut()
    }
}

impl From<u128> for TokenAmount {
    fn from(amount: u128) -> Self {
        TokenAmount(amount.into())
    }
}

impl Display for TokenAmount {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}