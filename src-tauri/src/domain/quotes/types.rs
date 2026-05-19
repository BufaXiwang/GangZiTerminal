//! Quotes 模块的核心数据类型。
//!
//! 全部按 architecture.md § 3.3 数据契约定义，使用 newtype（Yuan / Lots / TradeDate ...）
//! 编译期防单位混淆。Five 个分组：
//!
//! 1. 实时报价（StockQuote + 五档盘口）
//! 2. K 线（KlinePoint / KlineSeries / KlinePeriod / AdjMode / MinuteKlinePoint / MinutePeriod）
//! 3. 大盘（MarketIndex / MarketBreadth / MarketOverview）
//! 4. 个股档案（StockRef）
//! 5. 元信息（HistorySource）

use crate::domain::shared::{Lots, OccurredAt, StockCode, TradeDate, Yuan};
use serde::{Deserialize, Serialize};

// ============================================================================
// 1. 实时报价
// ============================================================================

/// 实时报价 + 五档盘口 + 内外盘——StockQuote 是 Quotes 模块的"当前快照"主结构。
///
/// 字段命名：
/// - `day_volume` / `day_amount`：当日累计（避免和 K 线的 period 量额混）
/// - `captured_at`：本地拉取时间——agent 判断 snapshot 多旧用
/// - `quote_time`：交易所给的报价时间——和 captured_at 可能差 0-3 秒
/// - 五档：`bid_*` / `ask_*` 各 5 档，buy 一档是 `bid_prices[0]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StockQuote {
    pub code: StockCode,
    pub name: String,

    // 价
    pub price: Option<Yuan>,
    pub change_percent: Option<f64>, // 百分数（涨 1% → 1.0）
    pub change: Option<Yuan>,
    pub open: Option<Yuan>,
    pub high: Option<Yuan>,
    pub low: Option<Yuan>,
    pub previous_close: Option<Yuan>,

    // 量额（当日累计）
    pub day_volume: Option<Lots>,
    pub day_amount: Option<Yuan>,

    /// 本地拉取时间（unix ms）——判断 snapshot 多旧
    pub captured_at: OccurredAt,

    /// 买盘五档（bid 1..5）。数据源不支持时为空。
    #[serde(default)]
    pub bid_levels: Vec<OrderBookLevel>,
    /// 卖盘五档（ask 1..5）。数据源不支持时为空。
    #[serde(default)]
    pub ask_levels: Vec<OrderBookLevel>,
    /// 外盘 / 主动买入量。数据源不支持时为空。
    pub buy_volume: Option<Lots>,
    /// 内盘 / 主动卖出量。数据源不支持时为空。
    pub sell_volume: Option<Lots>,
    /// 委比（%）：(买量 - 卖量) / (买量 + 卖量) * 100。
    pub order_imbalance: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderBookLevel {
    pub price: Option<Yuan>,
    pub volume: Option<Lots>,
}

// ============================================================================
// 2. K 线
// ============================================================================

/// 单根 K 线——日 / 周 / 月线共用一种。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KlinePoint {
    pub date: TradeDate,
    pub open: Yuan,
    pub close: Yuan,
    pub high: Yuan,
    pub low: Yuan,
    pub volume: Lots,
    pub amount: Yuan,
}

/// K 线序列——含元数据（来源 / 是否 stale / warning）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KlineSeries {
    pub code: StockCode,
    pub period: KlinePeriod,
    pub adj: AdjMode,
    pub points: Vec<KlinePoint>,
    pub source: HistorySource,
    pub stale: bool,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KlinePeriod {
    Day,
    Week,
    Month,
}

impl KlinePeriod {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }
}

/// 复权模式。
/// - `None`：原始价（含分红 / 送转跳水）
/// - `Qfq`：前复权（按最新除权基准归一化历史价）
/// - `Hfq`：后复权（按最初基准归一化）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjMode {
    None,
    Qfq,
    Hfq,
}

impl AdjMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Qfq => "qfq",
            Self::Hfq => "hfq",
        }
    }
}

// ============================================================================
// 分钟 K
// ============================================================================

/// 分钟 K 周期——1m / 5m / 15m / 30m / 60m。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MinutePeriod {
    M1,
    M5,
    M15,
    M30,
    M60,
}

impl MinutePeriod {
    pub fn minutes(&self) -> u32 {
        match self {
            Self::M1 => 1,
            Self::M5 => 5,
            Self::M15 => 15,
            Self::M30 => 30,
            Self::M60 => 60,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::M1 => "1m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::M30 => "30m",
            Self::M60 => "60m",
        }
    }
}

/// 分钟 K 单根。`timestamp` 是分钟边界的 unix ms。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MinuteKlinePoint {
    pub timestamp: OccurredAt,
    pub open: Yuan,
    pub close: Yuan,
    pub high: Yuan,
    pub low: Yuan,
    pub volume: Lots,
    pub amount: Yuan,
    /// 累计 VWAP（成交量加权均价）——日内 trader 必看
    pub vwap: Option<Yuan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MinuteKlineSeries {
    pub code: StockCode,
    pub period: MinutePeriod,
    pub date: TradeDate,
    pub points: Vec<MinuteKlinePoint>,
    pub source: HistorySource,
    pub stale: bool,
}

/// 当日分时图（每分钟价 + 累计均价）——不同于分钟 K（OHLC）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MinutePoint {
    /// "HH:MM" 或完整 "YYYY-MM-DD HH:MM"——前端展示直接用
    pub time: String,
    pub price: Yuan,
    pub average: Option<Yuan>,
    pub volume: Option<Lots>,
    pub amount: Option<Yuan>,
}

// ============================================================================
// 3. 大盘
// ============================================================================

/// 大盘指数当前快照（不带历史 K）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketIndex {
    pub code: StockCode,
    pub name: String,
    pub price: Option<Yuan>,
    pub change: Option<Yuan>,
    pub change_percent: Option<f64>,
    pub timestamp: OccurredAt,
}

/// 涨跌广度——多少只票涨 / 跌 / 平。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketBreadth {
    pub rise: u32,
    pub fall: u32,
    pub flat: u32,
}

impl MarketBreadth {
    pub fn empty() -> Self {
        Self {
            rise: 0,
            fall: 0,
            flat: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketOverview {
    pub indices: Vec<MarketIndex>,
    pub breadth: MarketBreadth,
    pub timestamp: OccurredAt,
}

// ============================================================================
// 4. 个股档案 + 基本面
// ============================================================================

/// 股票档案 lookup 结果——`resolve_stock(code_or_name)` 返。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StockRef {
    pub code: StockCode,
    pub name: String,
    /// 行业（TuShare industry 字段，"白酒" / "半导体"）
    pub sector: Option<String>,
    /// "sh" / "sz" / "bj"
    pub market: String,
}

/// 全市场标的——股票 / 指数 / 基金的合集，给"今日市场"页面列表用。
///
/// `ts_code` 是后端唯一键（"000001.SH" / "510300.SH" / "159915.SZ"）；
/// `code` 是 6 位显示码。`category` 区分类别，前端列表 tab 分流靠它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketInstrument {
    /// 唯一键，带后缀。stock 表里是 "{code}.{SH|SZ|BJ}"
    pub ts_code: String,
    /// 6 位 code，给用户看
    pub code: String,
    pub name: String,
    /// "stock" / "index" / "fund"
    pub category: InstrumentCategory,
    /// 个股 = 行业；指数 = 发布机构（CSI / SSE 等）；基金 = 类别
    pub sector: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstrumentCategory {
    Stock,
    Index,
    Fund,
}

impl InstrumentCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stock => "stock",
            Self::Index => "index",
            Self::Fund => "fund",
        }
    }
}

/// 每日基本面指标——PE / PB / 市值 / 换手率 / 量比（TuShare daily_basic）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyBasic {
    pub code: StockCode,
    pub trade_date: TradeDate,
    pub pe: Option<f64>,
    pub pe_ttm: Option<f64>,
    pub pb: Option<f64>,
    pub ps: Option<f64>,
    pub ps_ttm: Option<f64>,
    /// 股息率（%）
    pub dv_ratio: Option<f64>,
    /// 股息率 TTM（%）
    pub dv_ttm: Option<f64>,
    /// 换手率（%）
    pub turnover_rate: f64,
    /// 自由流通换手率（%）
    pub turnover_rate_float: Option<f64>,
    /// 量比
    pub volume_ratio: f64,
    /// 总市值（万元）
    pub total_mv: Yuan,
    /// 流通市值（万元）
    pub circ_mv: Yuan,
}

/// 个股全档案 = 基础信息 + 当前基本面 + 指标快照。
///
/// `get_stock_profile(code)` 返这个——agent 看一只票时一次拿到全档案。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StockProfile {
    pub stock_ref: StockRef,
    pub fundamentals: Option<DailyBasic>,
    pub list_date: Option<TradeDate>,
    pub list_status: ListStatus,
    pub is_st: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListStatus {
    Listed,
    Suspended,
    Delisted,
}

// ============================================================================
// 5. 资金面
// ============================================================================

/// 龙虎榜单条目（TuShare top_list）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopListItem {
    pub trade_date: TradeDate,
    pub code: StockCode,
    pub name: String,
    pub close: Option<Yuan>,
    pub pct_change: Option<f64>,
    pub turnover_rate: Option<f64>,
    pub amount: Option<Yuan>,
    /// 龙虎榜净买入额
    pub net_amount: Option<Yuan>,
    /// 净买额占总成交比（%）
    pub net_rate: Option<f64>,
    pub reason: String,
}

/// 资金流向（按单笔规模分级——小/中/大/特大单）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoneyFlowItem {
    pub trade_date: TradeDate,
    pub code: StockCode,
    pub net_small: Option<Yuan>,
    pub net_mid: Option<Yuan>,
    pub net_large: Option<Yuan>,
    /// 特大单——通常代表主力 / 机构
    pub net_extra_large: Option<Yuan>,
    pub net_total: Option<Yuan>,
}

/// 沪深港通整体资金流向（北向 / 南向）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NorthMoneyFlow {
    pub trade_date: TradeDate,
    /// 沪股通净买入额（元）
    pub sh_north: Yuan,
    /// 深股通净买入额（元）
    pub sz_north: Yuan,
    /// 总净买入
    pub total: Yuan,
}

/// 北向资金 top10 个股持仓。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NorthHolding {
    pub trade_date: TradeDate,
    pub code: StockCode,
    pub name: String,
    /// 持有市值
    pub hold_amount: Option<Yuan>,
    /// 持股占流通股比（%）
    pub hold_ratio: Option<f64>,
}

/// 融资融券每日汇总。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginSummary {
    pub trade_date: TradeDate,
    /// 融资余额
    pub financing_balance: Yuan,
    /// 融券余额
    pub margin_balance: Yuan,
    /// 当日融资买入额
    pub financing_buy: Yuan,
    /// 当日融券卖出额
    pub margin_sell: Yuan,
}

// ============================================================================
// 6. 公司动作
// ============================================================================

/// 公司动作事件——影响交易策略 / 涨跌幅 / 流通盘的关键节点。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompanyEvent {
    /// 分红 / 送转
    Dividend {
        announce_date: TradeDate,
        ex_date: Option<TradeDate>,
        /// 每 10 股派现金（元）
        cash_per_10: f64,
        /// 每 10 股送股
        share_per_10: f64,
        /// 每 10 股转股
        transfer_per_10: f64,
    },
    /// 停复牌
    Suspension {
        begin_date: TradeDate,
        end_date: Option<TradeDate>,
        reason: String,
    },
    /// ST 状态变更
    StChange {
        effective_date: TradeDate,
        new_status: StStatus,
        previous_name: String,
        new_name: String,
    },
    /// 业绩预告
    EarningsForecast {
        period: String,
        forecast_type: ForecastType,
        /// 预告净利润下限（元）
        min_profit: Option<Yuan>,
        max_profit: Option<Yuan>,
        change_min_pct: Option<f64>,
        change_max_pct: Option<f64>,
        summary: String,
    },
    /// 限售股解禁
    ShareUnlock {
        unlock_date: TradeDate,
        /// 解禁股数
        unlock_shares: Lots,
        /// 占总股本比（%）
        unlock_ratio: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StStatus {
    Normal,
    St,
    StarSt,
    Delisted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForecastType {
    Increase,
    Decrease,
    TurnProfit,
    TurnLoss,
    Continued,
    Unknown,
}

// ============================================================================
// 7. 板块（概念）
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConceptSector {
    pub code: String,
    pub name: String,
    pub member_count: Option<usize>,
}

/// 板块当日涨跌表现。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConceptPerformance {
    pub code: String,
    pub name: String,
    pub trade_date: TradeDate,
    /// 板块平均涨跌幅（%）
    pub avg_change_pct: f64,
    /// 板块成交额合计（元）
    pub total_amount: Yuan,
    /// 涨跌幅最大的成员（领涨）
    pub leader: Option<StockCode>,
    pub leader_change_pct: Option<f64>,
}

// ============================================================================
// 8. 交易日历
// ============================================================================

/// 交易日历——TuShare trade_cal 同步后的本地索引。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeCalendar {
    pub trading_days: Vec<TradeDate>,
    pub last_synced_at: OccurredAt,
}

// ============================================================================
// 9. Scanner（复合 query）
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanFilter {
    LimitUp,
    LimitDown,
    TopGain,
    TopLoss,
    TopAmount,
    TopVolume,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "field", content = "op")]
pub enum ScanCondition {
    ChangePct(ScanOp),
    Amount(ScanOp),
    Volume(ScanOp),
    TurnoverRate(ScanOp),
    VolumeRatio(ScanOp),
    Pe(ScanOp),
    Pb(ScanOp),
    TotalMv(ScanOp),
    CircMv(ScanOp),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op", content = "value")]
pub enum ScanOp {
    Gt(f64),
    Lt(f64),
    Between(f64, f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanSort {
    ChangePctDesc,
    ChangePctAsc,
    AmountDesc,
    VolumeDesc,
    TurnoverRateDesc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanItem {
    pub rank: u32,
    pub code: StockCode,
    pub name: String,
    pub price: Option<Yuan>,
    pub change_pct: Option<f64>,
    pub volume: Option<Lots>,
    pub amount: Option<Yuan>,
    pub turnover_rate: Option<f64>,
    pub volume_ratio: Option<f64>,
    pub pe: Option<f64>,
    pub pb: Option<f64>,
    pub total_mv: Option<Yuan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanResult {
    pub items: Vec<ScanItem>,
    pub trade_date: TradeDate,
    pub captured_at: OccurredAt,
    pub from_cache: bool,
}

// ============================================================================
// 10. 元信息
// ============================================================================

/// 历史数据的来源标签——agent / UI 用来知道这是哪条 path 拿到的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistorySource {
    /// TuShare Pro（历史 K / 财务 / 大盘日级）
    Tushare,
    /// Eastmoney ulist.np（实时报价）/ push2his trends2（分时）
    Eastmoney,
    /// 通达信（私有 TCP，K 线 + 分时主路径；不复权）
    Tdx,
    /// 远端全部失败，返回的是过期缓存
    StaleCache,
}

impl HistorySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tushare => "tushare",
            Self::Eastmoney => "eastmoney",
            Self::Tdx => "tdx",
            Self::StaleCache => "cache:stale",
        }
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minute_period_enum() {
        assert_eq!(MinutePeriod::M5.minutes(), 5);
        assert_eq!(MinutePeriod::M60.as_str(), "60m");
    }

    #[test]
    fn kline_period_str() {
        assert_eq!(KlinePeriod::Day.as_str(), "day");
        assert_eq!(KlinePeriod::Week.as_str(), "week");
    }

    #[test]
    fn adj_mode_str() {
        assert_eq!(AdjMode::Qfq.as_str(), "qfq");
    }

    #[test]
    fn history_source_str() {
        assert_eq!(HistorySource::Tushare.as_str(), "tushare");
        assert_eq!(HistorySource::StaleCache.as_str(), "cache:stale");
    }
}
