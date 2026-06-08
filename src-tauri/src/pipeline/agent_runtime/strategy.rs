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

/// 内置 baseline 策略文本（基础纪律：仓位 / freshness / 止损 / 不确定不交易）。
const BASELINE_STRATEGY_TEXT: &str = "你是 A 股模拟交易投资者，自驱动、保守审慎。基础纪律：\
单票市值不超过总资产 25%、总仓位不超过 95%、单笔不超过 25%；\
行情过期（stale）一律不下单；\
消息驱动交易必须先判断是否已被 price-in，不追高（标的当日大涨或临近涨停时倾向 no_action / 仅加自选）；\
开仓设止损（默认跌破成本约 8% 考虑止损）；\
不确定时不交易，宁可 no_action。";

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
        assert!(active.strategy.contains("不确定时不交易"));
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
