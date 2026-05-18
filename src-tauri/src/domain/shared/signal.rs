//! Signal 枚举——驱动 Expectation 触发的原子条件。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 3。
//!
//! 24 个标准枚举 + Custom 兜底。分 7 类：
//! - 趋势 / 动量
//! - 摆动 / 均值回归
//! - 量能
//! - 资金 / 主力
//! - A 股特殊
//! - 板块 / 事件
//! - 基本面因子
//! - 消息
//! - 视觉（由 LLM 看图后回报，不在算法 detector 范畴）
//!
//! 每个 SignalKind 在 `infrastructure/quotes/signal_detector.rs` 有对应纯代码检测器
//! （视觉信号除外——它走 `analyze_chart` + `propose_visual_pattern` 工具链）。

use serde::{Deserialize, Serialize};

// ====== Signal 枚举 ======================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SignalKind {
    // ===== 趋势 / 动量（8） =====
    BreakoutAbove20MA,
    BreakoutBelow20MA,
    MA5CrossAbove20,
    MA5CrossBelow20,
    MACDGoldenCross,
    MACDDeathCross,
    New20DayHigh,
    New20DayLow,

    // ===== 摆动 / 均值回归（4） =====
    RSIOversold { period: u32 },     // 默认 period=14, threshold=30
    RSIOverbought { period: u32 },   // 默认 period=14, threshold=70
    BollingerBreakUpper,
    BollingerBreakLower,

    // ===== 量能（3） =====
    VolumeSpike { ratio: f32 },      // 量比 > ratio（默认 1.5）
    VolumeShrink { ratio: f32 },     // 量比 < ratio（默认 0.5）
    VolumePriceDivergence,           // OBV 与价格背离

    // ===== 资金 / 主力（3） =====
    NorthInflowStreak { days: u32 },
    NorthOutflowStreak { days: u32 },
    OnDragonTigerList,

    // ===== A 股特殊（3） =====
    LimitUp,
    LimitDown,
    LimitUpFlooded,                  // 一字板——流动性断点警告

    // ===== 板块 / 事件（2） =====
    SectorStrengthAbove { pct: f32 },
    UpcomingEvent { event_kind: EventKind, days_ahead: u32 },

    // ===== 基本面因子（4） =====
    PEBelowSectorPct { pct: f32 },   // PE 低于行业 N%
    PBBelowThreshold { value: f32 },
    ROEAboveThreshold { pct: f32 },
    EarningsGrowthAbove { pct: f32 },

    // ===== 消息（1） =====
    NewsCatalystMatched {
        news_kind: NewsKind,
        importance: NewsImportance,
    },

    // ===== 视觉（1，由 LLM 通过 analyze_chart + propose_visual_pattern 调用） =====
    VisualPatternRead {
        pattern: String,             // "double_bottom" / "head_and_shoulders_top" / "exhaustion_top" / ...
        confidence: f32,             // 0.0 - 1.0
        timeframe: String,           // "day" / "week" / "60m"
    },

    // ===== 兜底 =====
    Custom { tag: String },
}

impl SignalKind {
    /// 用于 schema CHECK + DB 序列化 + 命中率聚合的稳定 key。
    /// 同类枚举的不同参数算同一类（PE 阈值不同算同一 PE signal）。
    pub fn family_str(&self) -> &'static str {
        match self {
            Self::BreakoutAbove20MA => "breakout_above_20ma",
            Self::BreakoutBelow20MA => "breakout_below_20ma",
            Self::MA5CrossAbove20 => "ma5_cross_above_20",
            Self::MA5CrossBelow20 => "ma5_cross_below_20",
            Self::MACDGoldenCross => "macd_golden_cross",
            Self::MACDDeathCross => "macd_death_cross",
            Self::New20DayHigh => "new_20day_high",
            Self::New20DayLow => "new_20day_low",
            Self::RSIOversold { .. } => "rsi_oversold",
            Self::RSIOverbought { .. } => "rsi_overbought",
            Self::BollingerBreakUpper => "bollinger_break_upper",
            Self::BollingerBreakLower => "bollinger_break_lower",
            Self::VolumeSpike { .. } => "volume_spike",
            Self::VolumeShrink { .. } => "volume_shrink",
            Self::VolumePriceDivergence => "volume_price_divergence",
            Self::NorthInflowStreak { .. } => "north_inflow_streak",
            Self::NorthOutflowStreak { .. } => "north_outflow_streak",
            Self::OnDragonTigerList => "on_dragon_tiger_list",
            Self::LimitUp => "limit_up",
            Self::LimitDown => "limit_down",
            Self::LimitUpFlooded => "limit_up_flooded",
            Self::SectorStrengthAbove { .. } => "sector_strength_above",
            Self::UpcomingEvent { .. } => "upcoming_event",
            Self::PEBelowSectorPct { .. } => "pe_below_sector_pct",
            Self::PBBelowThreshold { .. } => "pb_below_threshold",
            Self::ROEAboveThreshold { .. } => "roe_above_threshold",
            Self::EarningsGrowthAbove { .. } => "earnings_growth_above",
            Self::NewsCatalystMatched { .. } => "news_catalyst_matched",
            Self::VisualPatternRead { .. } => "visual_pattern_read",
            Self::Custom { .. } => "custom",
        }
    }

    /// 视觉信号——由 LLM 走 `propose_visual_pattern` 工具落到事件链，
    /// 不由 `signal_detector::scan_all` 触发。
    pub fn is_visual(&self) -> bool {
        matches!(self, Self::VisualPatternRead { .. })
    }

    /// 消息类信号——由 news tagger 间接触发，不由技术指标 scanner 触发。
    pub fn is_news_based(&self) -> bool {
        matches!(self, Self::NewsCatalystMatched { .. })
    }
}

// ====== EventKind（用于 UpcomingEvent signal） ===========================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Earnings,
    Dividend,
    ShareUnlock,
    ShareholderMeeting,
    Other,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Earnings => "earnings",
            Self::Dividend => "dividend",
            Self::ShareUnlock => "share_unlock",
            Self::ShareholderMeeting => "shareholder_meeting",
            Self::Other => "other",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "earnings" => Some(Self::Earnings),
            "dividend" => Some(Self::Dividend),
            "share_unlock" => Some(Self::ShareUnlock),
            "shareholder_meeting" => Some(Self::ShareholderMeeting),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

// ====== NewsKind / NewsImportance（用于 NewsCatalystMatched signal） =====

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewsKind {
    Earnings,
    Halt,
    Restructure,
    Regulatory,
    Ownership,
    Operating,
    Policy,
    SectorTrend,
    Market,
    Other,
}

impl NewsKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Earnings => "earnings",
            Self::Halt => "halt",
            Self::Restructure => "restructure",
            Self::Regulatory => "regulatory",
            Self::Ownership => "ownership",
            Self::Operating => "operating",
            Self::Policy => "policy",
            Self::SectorTrend => "sector_trend",
            Self::Market => "market",
            Self::Other => "other",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "earnings" => Some(Self::Earnings),
            "halt" => Some(Self::Halt),
            "restructure" => Some(Self::Restructure),
            "regulatory" => Some(Self::Regulatory),
            "ownership" => Some(Self::Ownership),
            "operating" => Some(Self::Operating),
            "policy" => Some(Self::Policy),
            "sector_trend" => Some(Self::SectorTrend),
            "market" => Some(Self::Market),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NewsImportance {
    /// 停牌 / 立案 / 退市风险——盘中 5 分钟内必须 mini-scan
    High,
    /// 财报 / 解禁——next tick 处理
    Medium,
    /// 一般评论——盘后消化
    Low,
}

impl NewsImportance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            _ => None,
        }
    }
    /// High 立即触发 mini-scan，绕过 budget。
    pub fn triggers_immediate(self) -> bool {
        matches!(self, Self::High)
    }
}

// ====== SignalDetection 持久化形态 ======================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalDetection {
    /// 关联到 tick run_id（agent_episodes.run_id 或独立 scan tick id）
    pub tick_id: String,
    pub code: String, // StockCode 的 string 形态（落库后读）
    pub signal: SignalKind,
    pub detected_at: crate::domain::shared::OccurredAt,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_str_stable_across_param_variants() {
        let a = SignalKind::VolumeSpike { ratio: 1.5 };
        let b = SignalKind::VolumeSpike { ratio: 2.0 };
        assert_eq!(a.family_str(), b.family_str());
        assert_eq!(a.family_str(), "volume_spike");
    }

    #[test]
    fn visual_signal_detected() {
        let s = SignalKind::VisualPatternRead {
            pattern: "double_bottom".into(),
            confidence: 0.7,
            timeframe: "day".into(),
        };
        assert!(s.is_visual());
        assert!(!s.is_news_based());
    }

    #[test]
    fn news_high_triggers_immediate() {
        assert!(NewsImportance::High.triggers_immediate());
        assert!(!NewsImportance::Medium.triggers_immediate());
        assert!(!NewsImportance::Low.triggers_immediate());
    }

    #[test]
    fn news_signal_kind_introspection() {
        let s = SignalKind::NewsCatalystMatched {
            news_kind: NewsKind::Halt,
            importance: NewsImportance::High,
        };
        assert!(s.is_news_based());
        assert!(!s.is_visual());
        assert_eq!(s.family_str(), "news_catalyst_matched");
    }

    #[test]
    fn serde_round_trip() {
        let s = SignalKind::VolumeSpike { ratio: 1.5 };
        let json = serde_json::to_string(&s).unwrap();
        let back: SignalKind = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
