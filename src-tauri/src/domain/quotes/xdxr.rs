//! XDXR (除权除息) 事件 — 本地复权计算的真源输入。
//!
//! Spec: docs/design/quotes-module.md §2 "本地复权计算（基于 TDX xdxr）"
//!
//! 纯 domain 类型；不依赖 tauri / rusqlite / reqwest / infrastructure。
//!
//! 14 种 category 含义见 `XdxrCategory` 文档；category=1 是 qfq/hfq 复权计算最核心
//! 的输入（每个除权日的 `fenhong / songzhuangu / peigu / peigujia`）。

use crate::domain::shared::{TimestampMs, TradeDate, TsCode};

/// XDXR 事件分类。值与 TDX 协议 / pytdx `XDXR_CATEGORY_MAPPING` 一致（1..=14）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum XdxrCategory {
    /// 除权除息（cash dividend + bonus + rights）—— 复权核心输入。
    DividendAndSplit = 1,
    /// 送配股上市（bonus/rights listing）。
    RightsListing = 2,
    /// 非流通股上市。
    NonTradableListing = 3,
    /// 未知股本变动。
    UnknownEquityChange = 4,
    /// 股本变化。
    EquityChange = 5,
    /// 增发新股。
    NewIssue = 6,
    /// 股份回购。
    Buyback = 7,
    /// 增发新股上市。
    NewIssueListing = 8,
    /// 转配股上市。
    ConvertibleListing = 9,
    /// 可转债上市。
    ConvertibleBondListing = 10,
    /// 扩缩股（split/consolidation）。
    ShareConsolidation = 11,
    /// 非流通股缩股。
    NonTradableConsolidation = 12,
    /// 送认购权证。
    CallWarrant = 13,
    /// 送认沽权证。
    PutWarrant = 14,
}

impl XdxrCategory {
    pub fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::DividendAndSplit),
            2 => Some(Self::RightsListing),
            3 => Some(Self::NonTradableListing),
            4 => Some(Self::UnknownEquityChange),
            5 => Some(Self::EquityChange),
            6 => Some(Self::NewIssue),
            7 => Some(Self::Buyback),
            8 => Some(Self::NewIssueListing),
            9 => Some(Self::ConvertibleListing),
            10 => Some(Self::ConvertibleBondListing),
            11 => Some(Self::ShareConsolidation),
            12 => Some(Self::NonTradableConsolidation),
            13 => Some(Self::CallWarrant),
            14 => Some(Self::PutWarrant),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// XDXR 事件。所有数值字段都是 Option，因为不同 category 填不同子集。
///
/// 字段语义对齐 TDX 协议层 `XdxrRecord`，但日期改为 `TradeDate`，category 改 enum。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct XdxrEvent {
    pub ts_code: TsCode,
    pub occur_date: TradeDate,
    pub category: XdxrCategory,

    // Category == DividendAndSplit (1)：以下 4 个字段才有值
    /// 每 10 股派息（人民币元）
    pub fenhong: Option<f64>,
    /// 配股价（人民币元）
    pub peigujia: Option<f64>,
    /// 每 10 股送转股本（股）
    pub songzhuangu: Option<f64>,
    /// 每 10 股配股股数（股）
    pub peigu: Option<f64>,

    // Category in [ShareConsolidation, NonTradableConsolidation]：缩股比例
    pub suogu: Option<f64>,

    // Category in [CallWarrant, PutWarrant]：权证
    pub xingquanjia: Option<f64>,
    pub fenshu: Option<f64>,

    // 其他 category：股本结构变动 4 字段
    pub panqianliutong: Option<f64>,
    pub qianzongguben: Option<f64>,
    pub panhouliutong: Option<f64>,
    pub houzongguben: Option<f64>,

    pub fetched_at: TimestampMs,
}

impl XdxrEvent {
    /// 构造一个除权除息事件（category=1）的便利方法。
    pub fn dividend_and_split(
        ts_code: TsCode,
        occur_date: TradeDate,
        fenhong: Option<f64>,
        peigujia: Option<f64>,
        songzhuangu: Option<f64>,
        peigu: Option<f64>,
        fetched_at: TimestampMs,
    ) -> Self {
        Self {
            ts_code,
            occur_date,
            category: XdxrCategory::DividendAndSplit,
            fenhong,
            peigujia,
            songzhuangu,
            peigu,
            suogu: None,
            xingquanjia: None,
            fenshu: None,
            panqianliutong: None,
            qianzongguben: None,
            panhouliutong: None,
            houzongguben: None,
            fetched_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_round_trip_all_14() {
        for n in 1u8..=14u8 {
            let cat = XdxrCategory::from_u8(n).expect("known category");
            assert_eq!(cat.as_u8(), n);
        }
        assert!(XdxrCategory::from_u8(0).is_none());
        assert!(XdxrCategory::from_u8(15).is_none());
    }

    #[test]
    fn dividend_and_split_builder_leaves_other_fields_none() {
        let ts = TsCode::parse("600519.SH").unwrap();
        let d = TradeDate::parse("20240620").unwrap();
        let e = XdxrEvent::dividend_and_split(
            ts.clone(),
            d,
            Some(30.872),
            None,
            Some(0.0),
            Some(0.0),
            1_700_000_000_000,
        );
        assert_eq!(e.category, XdxrCategory::DividendAndSplit);
        assert_eq!(e.fenhong, Some(30.872));
        assert!(e.suogu.is_none());
        assert!(e.xingquanjia.is_none());
        assert!(e.qianzongguben.is_none());
    }
}
