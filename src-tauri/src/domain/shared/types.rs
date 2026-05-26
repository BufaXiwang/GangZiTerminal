//! Shared scalar types — Money / Price / Shares / Volume / TsCode / 时间 / 枚举。
//!
//! Spec: docs/design/shared-types.md §1–§3

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::fmt;

// ============================================================================
// §1 标的和市场
// ============================================================================

/// 跨模块主键。格式：6 位数字 + `.` + 市场后缀（`600519.SH` / `000001.SZ` / `430047.BJ`）。
///
/// Spec: shared-types.md §1
/// - 匹配大小写不敏感，但对外返回必须大写。
/// - 非 `TsCode` 标识不能作为跨模块持久化主键。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
pub struct TsCode(String);

impl TsCode {
    /// 构造并校验。输入会被大写化。
    pub fn parse(raw: &str) -> Result<Self, TsCodeParseError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(TsCodeParseError::Empty);
        }
        let upper = trimmed.to_ascii_uppercase();
        let bytes = upper.as_bytes();
        if bytes.len() != 9 || bytes[6] != b'.' {
            return Err(TsCodeParseError::BadShape(upper));
        }
        if !bytes[..6].iter().all(|b| b.is_ascii_digit()) {
            return Err(TsCodeParseError::BadShape(upper));
        }
        let suffix = &upper[7..];
        if !matches!(suffix, "SH" | "SZ" | "BJ") {
            return Err(TsCodeParseError::BadMarket(upper));
        }
        Ok(Self(upper))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn market(&self) -> Market {
        match &self.0[7..] {
            "SH" => Market::SH,
            "SZ" => Market::SZ,
            "BJ" => Market::BJ,
            _ => unreachable!("TsCode invariant: market suffix validated in parse"),
        }
    }
}

impl fmt::Display for TsCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TsCodeParseError {
    #[error("ts_code is empty")]
    Empty,
    #[error("ts_code shape invalid: {0}")]
    BadShape(String),
    #[error("ts_code market suffix not SH/SZ/BJ: {0}")]
    BadMarket(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "UPPERCASE")]
pub enum Market {
    SH,
    SZ,
    BJ,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum InstrumentCategory {
    Stock,
    Index,
    Fund,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum InstrumentStatus {
    Listed,
    Suspended,
    Delisted,
    Unknown,
}

// ============================================================================
// §2 金额、数量和比例
// ============================================================================
//
// 设计决策（AGENTS.md：shared-types Rust 映射）：
//   - Money / Price / Amount → rust_decimal::Decimal 包 newtype。
//     避免 f64 精度坑、避免 fen / 厘 多单位混淆；
//     serde 默认序列化为 string，跨 IPC / DB / provider 边界精度无损。
//   - Shares / Volume → i64 newtype；A 股股票 / 基金均为整数股 / 整数份。
//   - Ratio / Percent → f64；展示和阈值比较，不参与金钱算术。

/// CNY，保留到分；内部计算可使用更高精度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Type)]
#[serde(transparent)]
pub struct Money(pub Decimal);

impl Money {
    pub const ZERO: Money = Money(Decimal::ZERO);

    pub fn from_decimal(d: Decimal) -> Self {
        Self(d)
    }

    pub fn into_decimal(self) -> Decimal {
        self.0
    }
}

/// CNY / 股或份。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Type)]
#[serde(transparent)]
pub struct Price(pub Decimal);

impl Price {
    pub fn from_decimal(d: Decimal) -> Self {
        Self(d)
    }

    pub fn into_decimal(self) -> Decimal {
        self.0
    }
}

/// 成交额，CNY。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Type)]
#[serde(transparent)]
pub struct Amount(pub Decimal);

impl Amount {
    pub const ZERO: Amount = Amount(Decimal::ZERO);

    pub fn from_decimal(d: Decimal) -> Self {
        Self(d)
    }

    pub fn into_decimal(self) -> Decimal {
        self.0
    }
}

/// 股 / 份数量。
///
/// Spec: shared-types.md §2
/// - Account 写接口必须校验 `Shares` 为正数；股票和场内基金买卖数量必须是 100 股 / 份整数倍。
///   （正数 / 整手校验由 Account domain 实现，本类型仅承载数值。）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Type)]
#[serde(transparent)]
pub struct Shares(pub i64);

/// 成交量，股或份。Provider 单位必须 normalize 到最小交易数量单位，不使用"手"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Type)]
#[serde(transparent)]
pub struct Volume(pub i64);

/// 0..1 比例。
pub type Ratio = f64;

/// 百分数点（3.25 表示 3.25%）。
pub type Percent = f64;

// ============================================================================
// §3 时间和交易日历
// ============================================================================

/// ISO-8601 with timezone。内部存 UTC，UI 渲染时转 Asia/Shanghai。
pub type OccurredAt = DateTime<Utc>;

/// 毫秒时间戳。
pub type TimestampMs = i64;

/// YYYYMMDD 交易日。
///
/// Spec: shared-types.md §3
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TradeDate(NaiveDate);

impl TradeDate {
    pub fn from_naive(d: NaiveDate) -> Self {
        Self(d)
    }

    pub fn into_naive(self) -> NaiveDate {
        self.0
    }

    pub fn as_naive(&self) -> NaiveDate {
        self.0
    }

    /// 解析 `YYYYMMDD` 字符串。
    pub fn parse(raw: &str) -> Result<Self, TradeDateParseError> {
        NaiveDate::parse_from_str(raw, "%Y%m%d")
            .map(Self)
            .map_err(|_| TradeDateParseError(raw.to_string()))
    }

    /// 序列化字符串 `YYYYMMDD`。
    pub fn format(&self) -> String {
        self.0.format("%Y%m%d").to_string()
    }
}

impl fmt::Display for TradeDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("trade_date not YYYYMMDD: {0}")]
pub struct TradeDateParseError(String);

impl Serialize for TradeDate {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.format())
    }
}

impl<'de> Deserialize<'de> for TradeDate {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(de)?;
        TradeDate::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl Type for TradeDate {
    fn inline(
        type_map: &mut specta::TypeMap,
        generics: specta::Generics,
    ) -> specta::DataType {
        // 对外类型契约：YYYYMMDD 字符串（shared-types.md §3）
        String::inline(type_map, generics)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_code_parses_and_uppercases() {
        let code = TsCode::parse("600519.sh").unwrap();
        assert_eq!(code.as_str(), "600519.SH");
        assert_eq!(code.market(), Market::SH);
    }

    #[test]
    fn ts_code_rejects_bad_shape() {
        assert!(TsCode::parse("60051.SH").is_err());
        assert!(TsCode::parse("600519SH").is_err());
        assert!(TsCode::parse("60051a.SH").is_err());
    }

    #[test]
    fn ts_code_rejects_unknown_market() {
        assert!(TsCode::parse("123456.XX").is_err());
    }

    #[test]
    fn ts_code_supports_bj() {
        assert_eq!(TsCode::parse("430047.BJ").unwrap().market(), Market::BJ);
    }

    #[test]
    fn trade_date_roundtrip() {
        let td = TradeDate::parse("20260526").unwrap();
        assert_eq!(td.format(), "20260526");
        let json = serde_json::to_string(&td).unwrap();
        assert_eq!(json, "\"20260526\"");
        let back: TradeDate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, td);
    }

    #[test]
    fn money_serializes_as_string() {
        let m = Money(Decimal::new(12345, 2)); // 123.45
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, "\"123.45\"");
    }
}
