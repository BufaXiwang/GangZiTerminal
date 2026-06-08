//! InvestmentStrategy 服务 —— 读 active / 版本化 upsert / seed baseline。
//!
//! Spec: docs/design/agent-runtime-module.md §3 `InvestmentStrategy`
//!
//! 纪律：**单点写、用户确认**——策略只在对话 mode、用户明确确认后经 `upsert` 写新版本
//! （调用方保证"用户已确认"）；news / account_trigger / review run 不写策略。
//! 每次更新 version+1，旧版本保留可追溯；纯自然语言（本阶段不结构化硬约束）。

use std::sync::Arc;

use chrono::Utc;

use crate::domain::agent::runtime::{InvestmentStrategy, StrategyStatus};
use crate::domain::shared::OccurredAt;
use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;

/// 内置 baseline 策略 id（首启无 active 策略时 seed）。
pub const BASELINE_STRATEGY_ID: &str = "baseline_a_share";

/// 内置 baseline 策略文本（龙头战法：情绪周期 / 龙头选股 / 仓位管理 / 止损止盈纪律）。
const BASELINE_STRATEGY_TEXT: &str = r#"# 龙头战法 — A 股模拟交易默认策略

> 100 万 CNY 模拟账户，超短线风格，以情绪周期为框架、以辨识度龙头为唯一标的。

## 一、核心理念

**只做最强，不做其余。** 在市场情绪周期的合适阶段，集中资金买入辨识度最高、资金最认可的板块领涨股（龙头），赚取主升段利润，在情绪退潮前离场。

- **龙头唯一性**：每轮主线题材只做一个真龙头，不做跟风、补涨、卡位
- **买在分歧，卖在一致**：市场对龙头产生分歧时是最佳买点，一致看多加速缩量涨停时兑现利润
- **情绪周期驱动**：择时重于择股，冰点/退潮期空仓等待，主升期重仓核心龙头
- **空仓也是操作**：龙空龙循环中，空仓等待时间往往占 60% 以上

## 二、情绪周期与仓位

| 阶段 | 特征 | 仓位上限 |
|------|------|----------|
| **冰点期** | 涨停 <15 家，连板消失，炸板率 >50% | ≤10%（试错仓） |
| **启动期** | 新题材首板集群，方向不明 | ≤30% |
| **主升期** | 涨停 >20 家，连板创新高，炸板率低 | 50%-80% |
| **高潮期** | 龙头缩量加速，补涨股批量涨停 | ≤30%（兑现中） |
| **退潮期** | 龙头断板/跌停，中位股 A 杀 | 0%（空仓） |

## 三、龙头选股标准

- 板块内**最早涨停**、连板高度最高、封单最厚、分歧日抗跌最强
- **二板定龙头**（Day 2 确认），三板确认地位
- 流通市值 20-200 亿，换手率 15%-35%，涨停时间早盘 10:30 前
- 题材最正宗、辨识度最高、有龙头历史记忆

## 四、买入条件

- **模式 A 打板**：二板确认日/分歧转一致日，10:30 前封板，封单 ≥1 万手，板块 ≥3 只跟风涨停。首次 30%，确认后加至 50%
- **模式 B 低吸（龙回头）**：龙头回调至 5 日线或前一涨停开盘价，缩量（<前日 70%），不破前低。30%-50%
- **模式 C 弱转强**：分歧日后次日高开 ≥2% + 竞价放量 + 5 分钟内封板。竞价 20%，封板加至 50%
- **通用前提**：情绪周期在启动/主升期，已确认主线题材，大盘非极端恐慌

## 五、止损规则（硬性，无条件执行）

- 龙头**跌停**：次日集合竞价无条件清仓
- 买入后当日亏损 ≥5%：尾盘清仓
- 买入后次日低开 ≥3% 且无回封：开盘 30 分钟内清仓
- 持仓从最高浮盈回撤 ≥8%：减半仓，跌破 5 日线全清
- 总账户单日亏损 ≥3%：当日不再操作，次日减仓至 ≤30%
- 总账户连续亏损 3 天：强制空仓 2 个交易日

## 六、止盈规则

- 龙头**断板**（未封住涨停）：全部卖出
- 龙头开板后**回封失败**：全部卖出
- 板块跟风股大面积炸板：减仓 50%
- 持股 ≥5 个交易日未创新高：清仓
- 龙头缩量加速板后次日高开低走：立即卖出

## 七、仓位管理

- 单票最大 **50%**（极端强势可临时 60%），同时持股 ≤2 只
- 最低现金保留 **10%**，单笔最大亏损 ≤ 总资产 **3%**
- **金字塔加仓**：首次最重(30%) → 确认加(20%) → 再确认(10%)，只在浮盈时加仓
- 月度最大回撤 **10%**

## 八、纪律铁律

1. **不做非龙头**：宁可错过，不可做错
2. **不在退潮期操作**
3. **不死扛亏损**：触发止损必须执行
4. **不追高一致性涨停**：全市场一致看好时往往是最后一棒
5. 每周交易 ≤5 次，没有明确信号时空仓等待
6. **不同时做多个题材**：聚焦一条主线
7. 单日亏损后**不报复性交易**

## 九、不做什么

- 不做基本面投资（龙头战法是短线情绪博弈）
- 不做没涨停板的票
- 不做 ST/*ST
- 不做老题材回炒
- 不做消息不明的异动股
- 大盘跌幅 >2% 或千股跌停时空仓"#;

#[derive(Debug)]
pub enum StrategyError {
    /// 乐观并发：`base_version` 与当前最新版本不一致。
    VersionConflict { expected: u32, actual: Option<u32> },
    Db(rusqlite::Error),
}

impl From<rusqlite::Error> for StrategyError {
    fn from(e: rusqlite::Error) -> Self {
        StrategyError::Db(e)
    }
}

pub struct StrategyService {
    repo: Arc<AgentRuntimeRepo>,
}

impl StrategyService {
    pub fn new(repo: Arc<AgentRuntimeRepo>) -> Self {
        Self { repo }
    }

    /// 当前 active 策略（最高版本的 active 行）。
    pub fn active(&self) -> rusqlite::Result<Option<InvestmentStrategy>> {
        self.repo.active_strategy()
    }

    /// 首启 seed：若无 active 策略，写入内置 baseline v1。返回是否新 seed。
    pub fn seed_baseline_if_empty(&self) -> rusqlite::Result<bool> {
        if self.repo.active_strategy()?.is_some() {
            return Ok(false);
        }
        let now: OccurredAt = Utc::now();
        let baseline = InvestmentStrategy {
            strategy_id: BASELINE_STRATEGY_ID.into(),
            version: 1,
            strategy: BASELINE_STRATEGY_TEXT.into(),
            status: StrategyStatus::Active,
            created_at: now,
            updated_at: now,
        };
        self.repo.insert_strategy_version(&baseline, Some("seed builtin baseline"))?;
        Ok(true)
    }

    /// 写策略新版本（单点写，调用方保证用户已确认）。
    ///
    /// - `strategy_id=None` → 更新当前 active 策略的 id（无 active 时用 baseline id）。
    /// - `base_version=Some(v)` → 乐观并发：v 必须等于当前最新版本，否则 `VersionConflict`。
    /// - 新版本号 = 当前最新版本 + 1。
    pub fn upsert(
        &self,
        strategy_id: Option<&str>,
        base_version: Option<u32>,
        strategy_text: String,
        status: StrategyStatus,
        reason: &str,
    ) -> Result<(String, u32), StrategyError> {
        let sid = match strategy_id {
            Some(s) => s.to_string(),
            None => self
                .repo
                .active_strategy()?
                .map(|s| s.strategy_id)
                .unwrap_or_else(|| BASELINE_STRATEGY_ID.to_string()),
        };
        let latest = self.repo.latest_version(&sid)?;
        if let Some(expected) = base_version {
            if Some(expected) != latest {
                return Err(StrategyError::VersionConflict { expected, actual: latest });
            }
        }
        let version = latest.unwrap_or(0) + 1;
        let now: OccurredAt = Utc::now();
        // 每个版本是独立行（version 为主键一部分），created_at = updated_at = 写入时刻。
        let s = InvestmentStrategy {
            strategy_id: sid.clone(),
            version,
            strategy: strategy_text,
            status,
            created_at: now,
            updated_at: now,
        };
        self.repo.insert_strategy_version(&s, Some(reason))?;
        Ok((sid, version))
    }

    /// 历史版本（前端展示）：`(version, updated_at, reason)`，倒序。
    pub fn list_versions(
        &self,
        strategy_id: &str,
    ) -> rusqlite::Result<Vec<(u32, OccurredAt, Option<String>)>> {
        self.repo.list_strategy_versions(strategy_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::db::{run_migrations, AppDb};

    fn svc() -> StrategyService {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        StrategyService::new(Arc::new(AgentRuntimeRepo::new(db)))
    }

    #[test]
    fn seed_baseline_once() {
        let s = svc();
        assert!(s.active().unwrap().is_none());
        assert!(s.seed_baseline_if_empty().unwrap());
        let active = s.active().unwrap().unwrap();
        assert_eq!(active.strategy_id, BASELINE_STRATEGY_ID);
        assert_eq!(active.version, 1);
        assert!(active.strategy.contains("龙头战法"));
        // 再 seed 不重复。
        assert!(!s.seed_baseline_if_empty().unwrap());
    }

    #[test]
    fn upsert_bumps_version() {
        let s = svc();
        s.seed_baseline_if_empty().unwrap();
        let (id, v) = s
            .upsert(None, Some(1), "更保守：单票不超 20%。".into(), StrategyStatus::Active, "用户确认收紧")
            .unwrap();
        assert_eq!(id, BASELINE_STRATEGY_ID);
        assert_eq!(v, 2);
        assert_eq!(s.active().unwrap().unwrap().version, 2);
        assert_eq!(s.active().unwrap().unwrap().strategy, "更保守：单票不超 20%。");
    }

    #[test]
    fn upsert_version_conflict() {
        let s = svc();
        s.seed_baseline_if_empty().unwrap();
        // base_version=99 != 当前最新 1 → 冲突。
        let err = s
            .upsert(None, Some(99), "x".into(), StrategyStatus::Active, "r")
            .unwrap_err();
        assert!(matches!(
            err,
            StrategyError::VersionConflict { expected: 99, actual: Some(1) }
        ));
    }

    #[test]
    fn upsert_new_strategy_id_starts_at_v1() {
        let s = svc();
        let (id, v) = s
            .upsert(Some("custom"), None, "全新策略".into(), StrategyStatus::Active, "新建")
            .unwrap();
        assert_eq!(id, "custom");
        assert_eq!(v, 1);
    }
}
