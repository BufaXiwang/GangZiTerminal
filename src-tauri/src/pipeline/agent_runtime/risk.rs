//! 风控编排（Runtime 级，跨 mode）—— 熔断 / 追高保护 / 当日额度 / 防自我打架。
//!
//! Spec: docs/design/agent-runtime-module.md §6 风控编排
//!
//! 两层风控里这是**编排级**：账户级硬约束（单票/总仓/单笔/日新单）由 Account fail-closed 兜底；
//! 本层在 Runtime 下单前拦截，阈值取 Runtime settings。纯策略函数（无 I/O），便于测试。

/// 风控阈值（来自 Runtime settings，spec §8）。
#[derive(Debug, Clone, Copy)]
pub struct RiskConfig {
    /// 熔断：连续亏损笔数阈值。缺省 5。
    pub max_consecutive_losses: u32,
    /// 熔断：单日组合回撤比例阈值（0.05 = 5%）。缺省 0.05。
    pub max_daily_drawdown: f64,
    /// 追高保护：标的当日涨幅超此值（0.05 = 5%）降级。缺省 0.05。
    pub chasing_guard_pct: f64,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_consecutive_losses: 5,
            max_daily_drawdown: 0.05,
            chasing_guard_pct: 0.05,
        }
    }
}

/// 风控闸门判定（spec §6）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RiskGateDecision {
    /// 放行。
    Allow,
    /// 降级为 no_action / 仅加自选（追高保护）。
    Downgrade { reason: String },
    /// 拦截（熔断 / 当日额度耗尽）。自动 mode 下不实际下单。
    Block { reason: String },
}

impl RiskGateDecision {
    pub fn is_allow(&self) -> bool {
        matches!(self, RiskGateDecision::Allow)
    }
}

/// 熔断是否激活：连亏或单日回撤任一超阈值。返回 Some(reason) 表示已熔断。
pub fn circuit_breaker_tripped(
    consecutive_losses: u32,
    daily_drawdown_ratio: f64,
    cfg: &RiskConfig,
) -> Option<String> {
    if consecutive_losses >= cfg.max_consecutive_losses {
        return Some(format!(
            "连续亏损 {} 笔达熔断阈值 {}",
            consecutive_losses, cfg.max_consecutive_losses
        ));
    }
    if daily_drawdown_ratio >= cfg.max_daily_drawdown {
        return Some(format!(
            "单日回撤 {:.2}% 达熔断阈值 {:.2}%",
            daily_drawdown_ratio * 100.0,
            cfg.max_daily_drawdown * 100.0
        ));
    }
    None
}

/// 追高保护：开仓时标的当日涨幅超阈值或临近涨停 → 触发（应降级）。
pub fn chasing_guard_triggered(today_change_pct: f64, near_limit_up: bool, cfg: &RiskConfig) -> bool {
    near_limit_up || today_change_pct >= cfg.chasing_guard_pct
}

/// 当日新开仓额度是否耗尽（账户级 maxDailyNewOrders）。
pub fn daily_quota_exhausted(today_new_orders: u32, max_daily_new_orders: u32) -> bool {
    today_new_orders >= max_daily_new_orders
}

/// 输入：一次"自动 mode 开仓"决策的风控上下文。
#[derive(Debug, Clone, Copy)]
pub struct OpenGateInput {
    pub consecutive_losses: u32,
    pub daily_drawdown_ratio: f64,
    pub today_new_orders: u32,
    pub max_daily_new_orders: u32,
    pub today_change_pct: f64,
    pub near_limit_up: bool,
}

/// 综合闸门（自动 mode 开仓）：熔断/额度 → Block；追高 → Downgrade；否则 Allow。
///
/// 注：平仓 / 调仓 / 调整保护**不**受追高与额度限制（只有"新开仓"受额度 + 追高），
/// 熔断对所有自动下单生效——调用方按动作类型选择是否走本闸门或只走 `circuit_breaker_tripped`。
pub fn evaluate_open_gate(input: &OpenGateInput, cfg: &RiskConfig) -> RiskGateDecision {
    if let Some(reason) = circuit_breaker_tripped(input.consecutive_losses, input.daily_drawdown_ratio, cfg)
    {
        return RiskGateDecision::Block {
            reason: format!("熔断激活：{reason}（需用户在对话中确认解除）"),
        };
    }
    if daily_quota_exhausted(input.today_new_orders, input.max_daily_new_orders) {
        return RiskGateDecision::Block {
            reason: format!(
                "当日新开仓额度耗尽（{}/{}）",
                input.today_new_orders, input.max_daily_new_orders
            ),
        };
    }
    if chasing_guard_triggered(input.today_change_pct, input.near_limit_up, cfg) {
        return RiskGateDecision::Downgrade {
            reason: if input.near_limit_up {
                "临近涨停，追高保护：降级为 no_action / 仅加自选".to_string()
            } else {
                format!(
                    "当日涨幅 {:.2}% 超追高阈值 {:.2}%：降级为 no_action / 仅加自选",
                    input.today_change_pct * 100.0,
                    cfg.chasing_guard_pct * 100.0
                )
            },
        };
    }
    RiskGateDecision::Allow
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> OpenGateInput {
        OpenGateInput {
            consecutive_losses: 0,
            daily_drawdown_ratio: 0.0,
            today_new_orders: 0,
            max_daily_new_orders: 20,
            today_change_pct: 0.0,
            near_limit_up: false,
        }
    }

    #[test]
    fn allow_when_clean() {
        assert_eq!(evaluate_open_gate(&base(), &RiskConfig::default()), RiskGateDecision::Allow);
    }

    #[test]
    fn circuit_breaker_on_consecutive_losses() {
        let cfg = RiskConfig::default();
        assert!(circuit_breaker_tripped(5, 0.0, &cfg).is_some());
        assert!(circuit_breaker_tripped(4, 0.0, &cfg).is_none());
        let mut i = base();
        i.consecutive_losses = 6;
        assert!(matches!(evaluate_open_gate(&i, &cfg), RiskGateDecision::Block { .. }));
    }

    #[test]
    fn circuit_breaker_on_daily_drawdown() {
        let cfg = RiskConfig::default();
        assert!(circuit_breaker_tripped(0, 0.06, &cfg).is_some());
        assert!(circuit_breaker_tripped(0, 0.04, &cfg).is_none());
    }

    #[test]
    fn quota_exhausted_blocks_new_open() {
        let cfg = RiskConfig::default();
        let mut i = base();
        i.today_new_orders = 20;
        assert!(matches!(evaluate_open_gate(&i, &cfg), RiskGateDecision::Block { .. }));
    }

    #[test]
    fn chasing_guard_downgrades() {
        let cfg = RiskConfig::default();
        let mut i = base();
        i.today_change_pct = 0.06; // 超 5%
        assert!(matches!(evaluate_open_gate(&i, &cfg), RiskGateDecision::Downgrade { .. }));
        // 临近涨停也降级。
        let mut j = base();
        j.near_limit_up = true;
        assert!(matches!(evaluate_open_gate(&j, &cfg), RiskGateDecision::Downgrade { .. }));
    }

    #[test]
    fn circuit_breaker_takes_priority_over_chasing() {
        let cfg = RiskConfig::default();
        let mut i = base();
        i.consecutive_losses = 5;
        i.today_change_pct = 0.10;
        // 熔断优先于追高 → Block 而非 Downgrade。
        assert!(matches!(evaluate_open_gate(&i, &cfg), RiskGateDecision::Block { .. }));
    }
}
