//! StockQuote / MarketQuoteSnapshot / DailyBasic / QuoteDepthLevel — 行情和基本面 DTO。
//!
//! Spec: docs/design/quotes-module.md §2

use crate::domain::shared::{
    Amount, Freshness, InstrumentCategory, Money, OccurredAt, Percent, Price, TradeDate, TsCode,
    Volume, WarningCode,
};
use serde::{Deserialize, Serialize};
use specta::Type;

/// Spec: quotes-module.md §2 行情 DTO
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum QuoteSource {
    Tdx,
    Eastmoney,
    Tencent,
    Sina,
    Mixed,
}

impl QuoteSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            QuoteSource::Tdx => "tdx",
            QuoteSource::Eastmoney => "eastmoney",
            QuoteSource::Tencent => "tencent",
            QuoteSource::Sina => "sina",
            QuoteSource::Mixed => "mixed",
        }
    }
    /// 完整 quote 优先级（spec §5 provider 选择策略 tie-breaker）。
    pub fn priority(&self) -> u8 {
        match self {
            QuoteSource::Tdx => 4,
            QuoteSource::Eastmoney => 3,
            QuoteSource::Tencent => 2,
            QuoteSource::Sina => 1,
            QuoteSource::Mixed => 0,
        }
    }
}

/// Spec: quotes-module.md §2 行情 DTO
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct QuoteDepthLevel {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<Volume>,
}

/// Spec: quotes-module.md §2 — query facade 派生 trade status。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum TradeStatus {
    Trading,
    Halted,
    Closed,
    Unknown,
}

/// Spec: quotes-module.md §2 StockQuote
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StockQuote {
    pub ts_code: TsCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub category: InstrumentCategory,
    pub trade_date: TradeDate,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_close: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_percent: Option<Percent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<Volume>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turnover_rate: Option<Percent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_up: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_down: Option<Price>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub bid: Vec<QuoteDepthLevel>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub ask: Vec<QuoteDepthLevel>,
    pub trade_status: TradeStatus,
    pub source: QuoteSource,
    pub captured_at: OccurredAt,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange_time: Option<OccurredAt>,
    pub freshness: Freshness,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}

impl StockQuote {
    /// Spec: quotes-module.md §5 line 742 — 展示最小集：`price / tradeDate / capturedAt`。
    ///
    /// `tradeDate` 和 `captured_at` 在 `StockQuote` 类型上不可为空（非 Option），
    /// 所以展示完整性只取决于 `price` 是否存在。
    pub fn is_display_complete(&self) -> bool {
        self.price.is_some()
    }

    /// Spec: quotes-module.md §5 line 742 — 成交模拟完整集：展示集 + 买一/卖一价格。
    pub fn is_quote_complete(&self) -> bool {
        if !self.is_display_complete() {
            return false;
        }
        let bid_ok = self.bid.first().and_then(|l| l.price).is_some();
        let ask_ok = self.ask.first().and_then(|l| l.price).is_some();
        bid_ok && ask_ok
    }
}

/// Spec: quotes-module.md §2 行情快照
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MarketQuoteSnapshot {
    pub ts_code: TsCode,
    pub category: InstrumentCategory,
    pub quote: StockQuote,
    pub updated_at: OccurredAt,
}

/// Spec: quotes-module.md §2 基本面读模型
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DailyBasic {
    pub ts_code: TsCode,
    pub trade_date: TradeDate,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pe: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pe_ttm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ps_ttm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turnover_rate: Option<Percent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turnover_rate_float: Option<Percent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_mv: Option<Money>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub circ_mv: Option<Money>,
    pub source: String,
    pub fetched_at: OccurredAt,
}

/// Spec: quotes-module.md §2 CompanyEvent
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CompanyEvent {
    pub id: String,
    pub ts_code: TsCode,
    pub event_type: CompanyEventType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announce_date: Option<TradeDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_date: Option<TradeDate>,
    pub payload: serde_json::Value,
    pub source: String,
    pub fetched_at: OccurredAt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CompanyEventType {
    Dividend,
    Suspension,
    Resume,
    St,
    EarningsForecast,
    Unlock,
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shared::{Freshness, FreshnessStatus, InstrumentCategory, Price, TradeDate};
    use chrono::Utc;
    use rust_decimal::Decimal;

    fn make_base(price: Option<Price>) -> StockQuote {
        StockQuote {
            ts_code: crate::domain::shared::TsCode::parse("600519.SH").unwrap(),
            name: None,
            category: InstrumentCategory::Stock,
            trade_date: TradeDate::from_naive(chrono::NaiveDate::from_ymd_opt(2025, 5, 26).unwrap()),
            price,
            previous_close: None,
            open: None,
            high: None,
            low: None,
            change: None,
            change_percent: None,
            volume: None,
            amount: None,
            turnover_rate: None,
            volume_ratio: None,
            limit_up: None,
            limit_down: None,
            bid: Vec::new(),
            ask: Vec::new(),
            trade_status: TradeStatus::Trading,
            source: QuoteSource::Tdx,
            captured_at: Utc::now(),
            exchange_time: None,
            freshness: Freshness {
                status: FreshnessStatus::Fresh,
                captured_at: None,
                exchange_time: None,
                age_ms: None,
                source: None,
                warning: None,
            },
            warnings: Vec::new(),
        }
    }

    #[test]
    fn is_display_complete_requires_price() {
        let q_no_price = make_base(None);
        assert!(!q_no_price.is_display_complete());
        let q_with_price = make_base(Some(Price(Decimal::new(1000, 2))));
        assert!(q_with_price.is_display_complete());
    }

    #[test]
    fn is_quote_complete_requires_depth() {
        let mut q = make_base(Some(Price(Decimal::new(1000, 2))));
        assert!(!q.is_quote_complete(), "missing bid+ask");
        q.bid = vec![QuoteDepthLevel { price: Some(Price(Decimal::new(999, 2))), volume: None }];
        assert!(!q.is_quote_complete(), "still missing ask");
        q.ask = vec![QuoteDepthLevel { price: Some(Price(Decimal::new(1001, 2))), volume: None }];
        assert!(q.is_quote_complete());
    }

    #[test]
    fn is_quote_complete_requires_bid0_price_not_just_volume() {
        let mut q = make_base(Some(Price(Decimal::new(1000, 2))));
        q.bid = vec![QuoteDepthLevel { price: None, volume: Some(crate::domain::shared::Volume(100)) }];
        q.ask = vec![QuoteDepthLevel { price: Some(Price(Decimal::new(1001, 2))), volume: None }];
        assert!(!q.is_quote_complete(), "bid[0].price missing must fail completeness");
    }
}
