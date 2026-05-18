//! 系统启动时 seed 3 条默认 Strategy。
//!
//! 触发：strategies 表为空。覆盖 3 类常见入场模式：
//! - 动量突破型：放量突破 20MA + 板块强势
//! - 超跌反弹型：RSI 超卖 + 布林下轨 + 板块未恶化
//! - 资金驱动型：龙虎榜 + 北向连续净流入 + 当日强势

use crate::domain::account::expectation::Direction;
use crate::domain::shared::signal::SignalKind;
use crate::domain::agent::strategy::{
    ConvictionRule, SignalCondition, Strategy, TargetRule, TriggerLogic,
};
use crate::domain::shared::OccurredAt;
use crate::infrastructure::agent::strategy_repo::{create, list_all};
use tauri::AppHandle;

pub fn seed_if_empty(app: &AppHandle) -> Result<(), String> {
    if !list_all(app)?.is_empty() {
        return Ok(());
    }
    let now = OccurredAt::now();

    // 1. 动量突破型
    let momentum = Strategy {
        name: "动量突破".into(),
        description: "放量突破 20MA + 板块共振 → 5-8 天目标位".into(),
        trigger_when: vec![
            SignalCondition {
                signal: SignalKind::BreakoutAbove20MA,
            },
            SignalCondition {
                signal: SignalKind::VolumeSpike { ratio: 1.5 },
            },
            SignalCondition {
                signal: SignalKind::SectorStrengthAbove { pct: 3.0 },
            },
        ],
        trigger_logic: TriggerLogic::And,
        target: TargetRule {
            direction: Direction::Up,
            pct_relative_to_current: 7.0,
            horizon_days: 8,
        },
        conviction_rule: ConvictionRule {
            high_if: vec![SignalKind::NorthInflowStreak { days: 5 }],
            medium_default: true,
        },
        enabled: true,
        applied_count: 0,
        hit_count: 0,
        miss_count: 0,
        created_at: now,
        updated_at: now,
        ..Strategy::new("placeholder".into(), "".into(), vec![], TargetRule {
            direction: Direction::Up,
            pct_relative_to_current: 0.0,
            horizon_days: 0,
        }, now)
    };
    create(app, &momentum)?;

    // 2. 超跌反弹型
    let mean_reversion = Strategy {
        name: "超跌反弹".into(),
        description: "RSI 超卖 + 布林下轨 + 板块未恶化 → 反弹 +4% 5 天".into(),
        trigger_when: vec![
            SignalCondition {
                signal: SignalKind::RSIOversold { period: 14 },
            },
            SignalCondition {
                signal: SignalKind::BollingerBreakLower,
            },
        ],
        trigger_logic: TriggerLogic::And,
        target: TargetRule {
            direction: Direction::Up,
            pct_relative_to_current: 4.0,
            horizon_days: 5,
        },
        conviction_rule: ConvictionRule {
            high_if: vec![SignalKind::SectorStrengthAbove { pct: 0.5 }],
            medium_default: true,
        },
        enabled: true,
        applied_count: 0,
        hit_count: 0,
        miss_count: 0,
        created_at: now,
        updated_at: now,
        ..Strategy::new("placeholder".into(), "".into(), vec![], TargetRule {
            direction: Direction::Up,
            pct_relative_to_current: 0.0,
            horizon_days: 0,
        }, now)
    };
    create(app, &mean_reversion)?;

    // 3. 资金驱动型
    let capital_flow = Strategy {
        name: "资金驱动".into(),
        description: "龙虎榜上榜 + 北向连续净流入 + 当日强势 → +6% 10 天".into(),
        trigger_when: vec![
            SignalCondition {
                signal: SignalKind::OnDragonTigerList,
            },
            SignalCondition {
                signal: SignalKind::NorthInflowStreak { days: 3 },
            },
        ],
        trigger_logic: TriggerLogic::And,
        target: TargetRule {
            direction: Direction::Up,
            pct_relative_to_current: 6.0,
            horizon_days: 10,
        },
        conviction_rule: ConvictionRule::default(),
        enabled: true,
        applied_count: 0,
        hit_count: 0,
        miss_count: 0,
        created_at: now,
        updated_at: now,
        ..Strategy::new("placeholder".into(), "".into(), vec![], TargetRule {
            direction: Direction::Up,
            pct_relative_to_current: 0.0,
            horizon_days: 0,
        }, now)
    };
    create(app, &capital_flow)?;

    tracing::info!(count = 3, "Seed strategies 已注入");
    Ok(())
}
