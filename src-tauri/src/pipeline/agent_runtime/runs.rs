//! AgentRun 生命周期 —— 创建（冻结策略版本）/ 收尾 / 起止事件。
//!
//! Spec: docs/design/agent-runtime-module.md §3 `AgentRun` / §7 事件
//!
//! 策略版本冻结：run 创建时把 active `version` 写入 `AgentRun.strategy_version`，全程不变
//! （注入 L2、AgentTrade 盖戳都用它），不因并发 `upsert` 中途改变 → 确定性 + 归因正确。

use std::sync::{Arc, RwLock};

use chrono::Utc;
use uuid::Uuid;

use crate::domain::agent::channel::WireFormat;
use crate::domain::agent::runtime::{AgentRun, AgentRunStatus, AgentRunTrigger};
use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;

use super::strategy::StrategyService;

/// Runtime 自有事件（adapter 翻译成 kebab-case app event；spec §7）。
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// `agent-run-started` payload `{runId, mode, trigger}`。
    RunStarted { run_id: String, run: AgentRun },
    /// `agent-run-finished` payload `{runId, status, error?}`。
    RunFinished {
        run_id: String,
        status: AgentRunStatus,
        error: Option<String>,
    },
    /// `agent-analysis-result` payload `{resultId, runId, kind}`（news 分析产出，前端右侧列表）。
    AnalysisResultEmitted {
        result_id: String,
        run_id: String,
        kind: crate::domain::agent::runtime::AnalysisResultKind,
    },
}

pub type RuntimeEventSink = Arc<dyn Fn(RuntimeEvent) + Send + Sync>;

pub struct RunService {
    repo: Arc<AgentRuntimeRepo>,
    event_sink: RwLock<Option<RuntimeEventSink>>,
}

impl RunService {
    pub fn new(repo: Arc<AgentRuntimeRepo>) -> Self {
        Self {
            repo,
            event_sink: RwLock::new(None),
        }
    }

    pub fn set_event_sink(&self, sink: RuntimeEventSink) {
        if let Ok(mut g) = self.event_sink.write() {
            *g = Some(sink);
        }
    }

    fn emit(&self, ev: RuntimeEvent) {
        if let Ok(g) = self.event_sink.read() {
            if let Some(sink) = g.as_ref() {
                sink(ev);
            }
        }
    }

    /// 创建一个 run：冻结 active 策略版本、落库（status=running, started_at=now）、emit RunStarted。
    ///
    /// `parent_run_id` 仅 review 被 fork 时给。
    #[allow(clippy::too_many_arguments)]
    pub fn create_run(
        &self,
        trigger: AgentRunTrigger,
        provider: impl Into<String>,
        wire_format: WireFormat,
        model: impl Into<String>,
        strategy: &StrategyService,
        parent_run_id: Option<String>,
    ) -> rusqlite::Result<AgentRun> {
        let now = Utc::now();
        let strategy_version = strategy.active()?.map(|s| s.version);
        let run = AgentRun {
            run_id: format!("run_{}", Uuid::new_v4()),
            mode: trigger.mode(),
            trigger,
            parent_run_id,
            provider: provider.into(),
            wire_format,
            model: model.into(),
            strategy_version,
            causation_run_id: None,
            status: AgentRunStatus::Running,
            started_at: Some(now),
            ended_at: None,
            error: None,
        };
        self.repo.insert_run(&run, now)?;
        self.emit(RuntimeEvent::RunStarted {
            run_id: run.run_id.clone(),
            run: run.clone(),
        });
        Ok(run)
    }

    /// 收尾：set status + ended_at，emit RunFinished。失败也落 error。
    pub fn finish_run(
        &self,
        run_id: &str,
        status: AgentRunStatus,
        error: Option<String>,
    ) -> rusqlite::Result<()> {
        let now = Utc::now();
        self.repo
            .set_run_status(run_id, status, None, Some(now), error.as_deref())?;
        self.emit(RuntimeEvent::RunFinished {
            run_id: run_id.to_string(),
            status,
            error,
        });
        Ok(())
    }

    /// account_trigger run 归因到原始建仓 run（spec §6）。
    pub fn set_causation(&self, run_id: &str, causation_run_id: &str) -> rusqlite::Result<()> {
        self.repo.set_causation_run(run_id, causation_run_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::runtime::{AgentRunMode, AgentRunStatus};
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::db::{run_migrations, AppDb};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn setup() -> (RunService, StrategyService) {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = Arc::new(AgentRuntimeRepo::new(db));
        (RunService::new(repo.clone()), StrategyService::new(repo))
    }

    #[test]
    fn create_run_freezes_strategy_version_and_persists_running() {
        let (runs, strat) = setup();
        strat.seed_baseline_if_empty().unwrap(); // active v1
        let run = runs
            .create_run(
                AgentRunTrigger::NewsBatch { news_ids: vec!["n1".into()] },
                "anthropic",
                WireFormat::Messages,
                "claude",
                &strat,
                None,
            )
            .unwrap();
        assert_eq!(run.mode, AgentRunMode::News);
        assert_eq!(run.strategy_version, Some(1)); // 冻结当时 active 版本
        assert_eq!(run.status, AgentRunStatus::Running);

        // 之后策略升到 v2，已创建的 run 仍冻结 v1（库里读出来不变）。
        strat
            .upsert(None, Some(1), "v2".into(), crate::domain::agent::runtime::StrategyStatus::Active, "r")
            .unwrap();
        // 新 run 才拿 v2。
        let run2 = runs
            .create_run(
                AgentRunTrigger::UserChat { message_id: "m".into() },
                "anthropic",
                WireFormat::Messages,
                "claude",
                &strat,
                None,
            )
            .unwrap();
        assert_eq!(run2.strategy_version, Some(2));
        assert_eq!(run2.mode, AgentRunMode::Dialogue);
    }

    #[test]
    fn finish_run_sets_terminal_and_emits() {
        let (runs, strat) = setup();
        let started = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let s2 = started.clone();
        let f2 = finished.clone();
        runs.set_event_sink(Arc::new(move |ev| match ev {
            RuntimeEvent::RunStarted { .. } => {
                s2.fetch_add(1, Ordering::SeqCst);
            }
            RuntimeEvent::RunFinished { status, .. } => {
                assert_eq!(status, AgentRunStatus::Completed);
                f2.fetch_add(1, Ordering::SeqCst);
            }
            RuntimeEvent::AnalysisResultEmitted { .. } => {}
        }));
        let run = runs
            .create_run(
                AgentRunTrigger::EodReview {
                    trade_date: crate::domain::shared::TradeDate::parse("20260603").unwrap(),
                },
                "anthropic",
                WireFormat::Messages,
                "claude",
                &strat,
                None,
            )
            .unwrap();
        runs.finish_run(&run.run_id, AgentRunStatus::Completed, None).unwrap();
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(finished.load(Ordering::SeqCst), 1);
    }
}
