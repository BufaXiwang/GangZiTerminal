//! 金额单位 newtype——`Yuan`（元）/ `KYuan`（千元）。
//!
//! TuShare amount 字段是千元单位；前端 / agent 展示用元。互转必须显式，编译期防混。

use serde::{Deserialize, Serialize};

/// 元——内部 / 前端 / agent 展示标准单位。
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Yuan(f64);

impl Yuan {
    pub fn new(v: f64) -> Result<Self, MoneyError> {
        if v.is_finite() {
            Ok(Self(v))
        } else {
            Err(MoneyError::NonFinite(v))
        }
    }

    /// 不校验——仅在数据源已保证 finite 的场景用（DB row / API 响应内部转换）。
    pub fn from_unchecked(v: f64) -> Self {
        Self(v)
    }

    pub fn value(&self) -> f64 {
        self.0
    }

    /// 千元 → 元
    pub fn from_kyuan(v: KYuan) -> Self {
        Self(v.value() * 1000.0)
    }

    /// 0 元
    pub const ZERO: Self = Self(0.0);
}

impl std::fmt::Display for Yuan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "¥{:.2}", self.0)
    }
}

// 算术——同单位可加减，不同单位需显式 from_*
impl std::ops::Add for Yuan {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}
impl std::ops::Sub for Yuan {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}
impl std::ops::AddAssign for Yuan {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

/// 千元——TuShare amount 默认单位。需显式转 Yuan 才能用。
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KYuan(f64);

impl KYuan {
    pub fn new(v: f64) -> Result<Self, MoneyError> {
        if v.is_finite() {
            Ok(Self(v))
        } else {
            Err(MoneyError::NonFinite(v))
        }
    }

    pub fn from_unchecked(v: f64) -> Self {
        Self(v)
    }

    pub fn value(&self) -> f64 {
        self.0
    }

    pub fn to_yuan(self) -> Yuan {
        Yuan::from_kyuan(self)
    }
}

// ===== 错误 ===============================================================

#[derive(Debug, Clone, thiserror::Error)]
pub enum MoneyError {
    #[error("非有限金额：{0}（NaN / Inf）")]
    NonFinite(f64),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuan_validation() {
        assert!(Yuan::new(10.5).is_ok());
        assert!(Yuan::new(f64::NAN).is_err());
        assert!(Yuan::new(f64::INFINITY).is_err());
    }

    #[test]
    fn kyuan_to_yuan() {
        let ky = KYuan::new(10.5).unwrap();
        assert_eq!(ky.to_yuan().value(), 10500.0);
    }

    #[test]
    fn yuan_arithmetic() {
        let a = Yuan::new(100.0).unwrap();
        let b = Yuan::new(50.0).unwrap();
        assert_eq!((a + b).value(), 150.0);
        assert_eq!((a - b).value(), 50.0);
    }
}
