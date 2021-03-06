use crate::data::numbers::U256;
use crate::domain::YoctoNear;
use near_sdk::{
    borsh::{self, BorshDeserialize, BorshSerialize},
    serde::{Deserialize, Serialize},
};
use std::ops::{Add, AddAssign, Mul};
use std::{
    fmt::{self, Display, Formatter},
    ops::{Deref, DerefMut},
};

/// Basis points (BPS) refers to a common unit of measure for interest rates and other percentages in finance.
/// One basis point is equal to 1/100th of 1%, or 0.01%, or 0.0001, and is used to denote the
/// percentage change in a financial instrument. The relationship between percentage changes and
/// basis points can be summarized as follows: 1% change = 100 basis points and 0.01% = 1 basis point.
/// Basis points are typically expressed in the abbreviations "bp," "bps," or "bips."
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Default,
    Hash,
)]
#[serde(crate = "near_sdk::serde")]
pub struct BasisPoints(pub u16);

impl BasisPoints {
    pub const ZERO: BasisPoints = BasisPoints(0);

    pub fn value(&self) -> u16 {
        self.0
    }

    pub fn of_rounded_down(&self, amount: YoctoNear) -> YoctoNear {
        *self * amount
    }

    pub fn of_rounded_up(&self, amount: YoctoNear) -> YoctoNear {
        ((U256::from(*amount) * U256::from(self.0) + U256::from(9999)) / U256::from(10000))
            .as_u128()
            .into()
    }
}

impl From<u16> for BasisPoints {
    fn from(value: u16) -> Self {
        Self(value)
    }
}

impl Deref for BasisPoints {
    type Target = u16;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for BasisPoints {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Display for BasisPoints {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: Into<YoctoNear>> Mul<T> for BasisPoints {
    type Output = YoctoNear;

    /// result is rounded down
    fn mul(self, rhs: T) -> Self::Output {
        (U256::from(*rhs.into()) * U256::from(*self) / U256::from(10000))
            .as_u128()
            .into()
    }
}

impl Mul<BasisPoints> for YoctoNear {
    type Output = YoctoNear;

    /// result is rounded down
    fn mul(self, rhs: BasisPoints) -> Self::Output {
        (U256::from(*rhs) * U256::from(*self) / U256::from(10000))
            .as_u128()
            .into()
    }
}

impl Add for BasisPoints {
    type Output = Self;

    fn add(self, rhs: BasisPoints) -> Self::Output {
        (*self + *rhs).into()
    }
}

impl AddAssign for BasisPoints {
    fn add_assign(&mut self, rhs: Self) {
        **self += *rhs;
    }
}

impl Add<u16> for BasisPoints {
    type Output = Self;

    fn add(self, rhs: u16) -> Self::Output {
        (*self + rhs).into()
    }
}

impl AddAssign<u16> for BasisPoints {
    fn add_assign(&mut self, rhs: u16) {
        **self += rhs;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yocto_near_bps() {
        let amount = YoctoNear::from(10001);
        let bps = BasisPoints::from(50);
        let value = amount * bps;
        assert_eq!(value, 50.into());
        assert_eq!(value, bps.of_rounded_down(amount));
        assert_eq!(bps.of_rounded_up(amount), 51.into());
    }
}
