//! K 线 / 分钟 K / 分时读模型。
//!
//! Spec: docs/design/quotes-module.md §2 (K 线和分时读模型)

use crate::domain::shared::{Amount, Freshness, Price, TimestampMs, TradeDate, Volume, WarningCode};
use serde::{Deserialize, Serialize};
use specta::Type;

/// Spec: quotes-module.md §2
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum KlinePeriod {
    Day,
    Week,
    Month,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum Adjust {
    None,
    Qfq,
    Hfq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum MinuteKlinePeriod {
    #[serde(rename = "1m")]
    M1,
    #[serde(rename = "5m")]
    M5,
    #[serde(rename = "15m")]
    M15,
    #[serde(rename = "30m")]
    M30,
    #[serde(rename = "60m")]
    M60,
}

impl MinuteKlinePeriod {
    pub fn as_str(&self) -> &'static str {
        match self {
            MinuteKlinePeriod::M1 => "1m",
            MinuteKlinePeriod::M5 => "5m",
            MinuteKlinePeriod::M15 => "15m",
            MinuteKlinePeriod::M30 => "30m",
            MinuteKlinePeriod::M60 => "60m",
        }
    }
}

impl KlinePeriod {
    pub fn as_str(&self) -> &'static str {
        match self {
            KlinePeriod::Day => "day",
            KlinePeriod::Week => "week",
            KlinePeriod::Month => "month",
        }
    }
}

impl Adjust {
    pub fn as_str(&self) -> &'static str {
        match self {
            Adjust::None => "none",
            Adjust::Qfq => "qfq",
            Adjust::Hfq => "hfq",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KlinePoint {
    pub date: TradeDate,
    pub open: Price,
    pub close: Price,
    pub high: Price,
    pub low: Price,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<Volume>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<Amount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KlineSeries {
    pub period: KlinePeriod,
    pub adjust: Adjust,
    pub points: Vec<KlinePoint>,
    pub freshness: Freshness,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MinuteKlinePoint {
    pub timestamp_ms: TimestampMs,
    pub open: Price,
    pub close: Price,
    pub high: Price,
    pub low: Price,
    pub volume: Volume,
    pub amount: Amount,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MinuteKlineSeries {
    pub period: MinuteKlinePeriod,
    pub points: Vec<MinuteKlinePoint>,
    pub freshness: Freshness,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MinutePoint {
    pub trade_date: TradeDate,
    pub time: String,
    pub price: Price,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<Volume>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<Amount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct IntradaySeries {
    pub trade_date: TradeDate,
    pub points: Vec<MinutePoint>,
    pub freshness: Freshness,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}
