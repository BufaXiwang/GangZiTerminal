//! 股数单位 newtype——`Shares`（股，A 股最小单位 100）/ `Lots`（手 = 100 股，交易单位）。
//!
//! Shares 强制整百校验；Lots 不校验（合法手数任意正整数）。

use serde::{Deserialize, Serialize};

/// 股——A 股最小数量单位，必须 ≥ 100 且 100 的倍数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Shares(i64);

impl Shares {
    /// 构造——整百校验。
    pub fn new(n: i64) -> Result<Self, SharesError> {
        if n < 100 || n % 100 != 0 {
            Err(SharesError::NotIntegerLot(n))
        } else {
            Ok(Self(n))
        }
    }

    /// 不校验直接构造——仅供已验证场景（DB / API 内部）。
    pub fn from_unchecked(n: i64) -> Self {
        Self(n)
    }

    pub fn value(&self) -> i64 {
        self.0
    }

    pub fn to_lots(self) -> Lots {
        Lots(self.0 / 100)
    }

    /// 0 股
    pub const ZERO: Self = Self(0);
}

impl std::ops::Add for Shares {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}
impl std::ops::Sub for Shares {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}

impl std::fmt::Display for Shares {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}股", self.0)
    }
}

/// 手——A 股交易单位，1 手 = 100 股。
///
/// TuShare 接口的 `vol` 字段默认是手；EM 接口的成交量字段也是手。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Lots(i64);

impl Lots {
    /// 构造——非负校验。
    pub fn new(n: i64) -> Self {
        Self(n.max(0))
    }

    pub fn from_unchecked(n: i64) -> Self {
        Self(n)
    }

    pub fn value(&self) -> i64 {
        self.0
    }

    pub fn to_shares(self) -> Shares {
        Shares(self.0 * 100)
    }

    pub const ZERO: Self = Self(0);
}

impl std::fmt::Display for Lots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}手", self.0)
    }
}

// ===== 错误 ===============================================================

#[derive(Debug, Clone, thiserror::Error)]
pub enum SharesError {
    #[error("股数 {0} 不合 A 股整手规则（≥ 100 且 100 倍数）")]
    NotIntegerLot(i64),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_integer_lot_rule() {
        assert!(Shares::new(100).is_ok());
        assert!(Shares::new(200).is_ok());
        assert!(Shares::new(1500).is_ok());
        assert!(Shares::new(50).is_err()); // < 100
        assert!(Shares::new(150).is_err()); // 不是 100 倍数
        assert!(Shares::new(0).is_err());
    }

    #[test]
    fn lots_to_shares() {
        let lots = Lots::new(5);
        let shares = lots.to_shares();
        assert_eq!(shares.value(), 500);
    }

    #[test]
    fn shares_to_lots() {
        let shares = Shares::new(300).unwrap();
        let lots = shares.to_lots();
        assert_eq!(lots.value(), 3);
    }
}
