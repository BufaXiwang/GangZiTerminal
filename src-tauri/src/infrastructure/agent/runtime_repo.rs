//! Agent Runtime 持久化仓储 —— AgentRun / InvestmentStrategy / AnalysisResult /
//! AgentTrade / orderId→runId 索引 / news buffer / 事件消费 / heartbeat。
//!
//! Spec: docs/design/agent-runtime-module.md §3 领域模型 / §8 幂等与可靠性
//!
//! 责任：纯持久化（接受 / 返回 domain 类型）；不做编排、不调 LLM、不调其它 BC。
//! 表见 `migrations.rs` MIGRATION_004_RUNTIME。

use crate::domain::agent::runtime::{
    AccountResultRef, AgentRun, AgentRunStatus, AgentRunTrigger, AgentTrade, AgentTradeStatus,
    AnalysisResult, InvestmentStrategy, ReviewSuggestion, StrategyStatus,
};
use crate::domain::shared::{OccurredAt, TradeDate};
use crate::infrastructure::db::AppDb;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;

pub struct AgentRuntimeRepo {
    db: AppDb,
}

// ---- serde / 时间 小工具 ----------------------------------------------------

fn enum_str<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|x| x.as_str().map(String::from))
        .unwrap_or_default()
}

fn enum_from<T: DeserializeOwned>(s: &str) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

fn ts(dt: &OccurredAt) -> String {
    dt.to_rfc3339()
}

fn parse_ts(s: &str) -> rusqlite::Result<OccurredAt> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

fn opt_ts(s: Option<String>) -> rusqlite::Result<Option<OccurredAt>> {
    s.map(|x| parse_ts(&x)).transpose()
}

impl AgentRuntimeRepo {
    pub fn new(db: AppDb) -> Self {
        Self { db }
    }

    // ======================= AgentRun =======================

    pub fn insert_run(&self, run: &AgentRun, now: OccurredAt) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO agent_runs
                 (run_id, mode, trigger_json, parent_run_id, provider, wire_format, model,
                  strategy_version, causation_run_id, status, started_at, ended_at, error, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    run.run_id,
                    enum_str(&run.mode),
                    serde_json::to_string(&run.trigger).unwrap(),
                    run.parent_run_id,
                    run.provider,
                    enum_str(&run.wire_format),
                    run.model,
                    run.strategy_version,
                    run.causation_run_id,
                    enum_str(&run.status),
                    run.started_at.as_ref().map(ts),
                    run.ended_at.as_ref().map(ts),
                    run.error,
                    ts(&now),
                ],
            )?;
            Ok(())
        })
    }

    pub fn set_run_status(
        &self,
        run_id: &str,
        status: AgentRunStatus,
        started_at: Option<OccurredAt>,
        ended_at: Option<OccurredAt>,
        error: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE agent_runs SET status=?2,
                   started_at=COALESCE(?3, started_at),
                   ended_at=COALESCE(?4, ended_at),
                   error=COALESCE(?5, error)
                 WHERE run_id=?1",
                params![
                    run_id,
                    enum_str(&status),
                    started_at.as_ref().map(ts),
                    ended_at.as_ref().map(ts),
                    error,
                ],
            )?;
            Ok(())
        })
    }

    pub fn set_causation_run(&self, run_id: &str, causation_run_id: &str) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE agent_runs SET causation_run_id=?2 WHERE run_id=?1",
                params![run_id, causation_run_id],
            )?;
            Ok(())
        })
    }

    pub fn get_run(&self, run_id: &str) -> rusqlite::Result<Option<AgentRun>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT run_id, mode, trigger_json, parent_run_id, provider, wire_format, model,
                        strategy_version, causation_run_id, status, started_at, ended_at, error
                 FROM agent_runs WHERE run_id=?1",
                params![run_id],
                row_to_run,
            )
            .optional()
        })
    }

    /// 启动恢复：所有 `running` run（重启时视为被中断）。
    pub fn list_running_runs(&self) -> rusqlite::Result<Vec<AgentRun>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT run_id, mode, trigger_json, parent_run_id, provider, wire_format, model,
                        strategy_version, causation_run_id, status, started_at, ended_at, error
                 FROM agent_runs WHERE status='running'",
            )?;
            let rows = stmt.query_map([], row_to_run)?;
            rows.collect()
        })
    }

    /// 最近 N 个 run（前端总览：按 started_at 倒序，NULL 起时间排末）。
    pub fn list_recent_runs(&self, limit: u32) -> rusqlite::Result<Vec<AgentRun>> {
        self.list_recent_runs_paged(limit, 0)
    }

    /// 最近 runs，带 offset 分页（spec §9 fetch_agent_state `offset`）。
    pub fn list_recent_runs_paged(&self, limit: u32, offset: u32) -> rusqlite::Result<Vec<AgentRun>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT run_id, mode, trigger_json, parent_run_id, provider, wire_format, model,
                        strategy_version, causation_run_id, status, started_at, ended_at, error
                 FROM agent_runs ORDER BY started_at DESC NULLS LAST LIMIT ?1 OFFSET ?2",
            )?;
            let rows = stmt.query_map(params![limit, offset], row_to_run)?;
            rows.collect()
        })
    }

    // ======================= InvestmentStrategy =======================

    /// 写一个策略新版本（version 由调用方在 active+1 上算好；单点写）。
    pub fn insert_strategy_version(
        &self,
        s: &InvestmentStrategy,
        reason: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO agent_investment_strategy
                 (strategy_id, version, strategy, status, reason, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    s.strategy_id,
                    s.version,
                    s.strategy,
                    enum_str(&s.status),
                    reason,
                    ts(&s.created_at),
                    ts(&s.updated_at),
                ],
            )?;
            Ok(())
        })
    }

    /// 当前 active 策略（最高版本的 active 行）。
    pub fn active_strategy(&self) -> rusqlite::Result<Option<InvestmentStrategy>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT strategy_id, version, strategy, status, created_at, updated_at
                 FROM agent_investment_strategy WHERE status='active'
                 ORDER BY version DESC LIMIT 1",
                [],
                row_to_strategy,
            )
            .optional()
        })
    }

    pub fn latest_version(&self, strategy_id: &str) -> rusqlite::Result<Option<u32>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT MAX(version) FROM agent_investment_strategy WHERE strategy_id=?1",
                params![strategy_id],
                |r| r.get::<_, Option<u32>>(0),
            )
            .optional()
            .map(Option::flatten)
        })
    }

    /// 历史版本（前端展示）：`(version, updated_at, reason)`，倒序。
    pub fn list_strategy_versions(
        &self,
        strategy_id: &str,
    ) -> rusqlite::Result<Vec<(u32, OccurredAt, Option<String>)>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT version, updated_at, reason FROM agent_investment_strategy
                 WHERE strategy_id=?1 ORDER BY version DESC",
            )?;
            let rows = stmt.query_map(params![strategy_id], |r| {
                Ok((r.get::<_, u32>(0)?, parse_ts(&r.get::<_, String>(1)?)?, r.get::<_, Option<String>>(2)?))
            })?;
            rows.collect()
        })
    }

    // ======================= AnalysisResult =======================

    pub fn insert_analysis_result(&self, a: &AnalysisResult) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO agent_analysis_results
                 (result_id, run_id, kind, summary, related_codes_json, trade_ids_json, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    a.result_id,
                    a.run_id,
                    enum_str(&a.kind),
                    a.summary,
                    serde_json::to_string(&a.related_codes).unwrap(),
                    serde_json::to_string(&a.trade_ids).unwrap(),
                    ts(&a.created_at),
                ],
            )?;
            Ok(())
        })
    }

    /// 最近 N 条 AnalysisResult（前端右侧列表：按 created_at 倒序）。
    pub fn list_recent_analysis_results(
        &self,
        limit: u32,
    ) -> rusqlite::Result<Vec<AnalysisResult>> {
        self.list_recent_analysis_results_paged(limit, 0)
    }

    /// 最近 AnalysisResult，带 offset 分页（spec §9 fetch_agent_state `offset`）。
    pub fn list_recent_analysis_results_paged(
        &self,
        limit: u32,
        offset: u32,
    ) -> rusqlite::Result<Vec<AnalysisResult>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT result_id, run_id, kind, summary, related_codes_json, trade_ids_json, created_at
                 FROM agent_analysis_results ORDER BY created_at DESC LIMIT ?1 OFFSET ?2",
            )?;
            let rows = stmt.query_map(params![limit, offset], row_to_analysis)?;
            rows.collect()
        })
    }

    /// 某 run 的全部 AnalysisResult（account_trigger 的「原始建仓 run 摘要」用，spec §3/§6）。
    pub fn list_analysis_results_by_run(
        &self,
        run_id: &str,
    ) -> rusqlite::Result<Vec<AnalysisResult>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT result_id, run_id, kind, summary, related_codes_json, trade_ids_json, created_at
                 FROM agent_analysis_results WHERE run_id=?1 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map(params![run_id], row_to_analysis)?;
            rows.collect()
        })
    }

    // ======================= AgentTrade =======================

    /// 调 Account 前先落 `submitting`（崩溃恢复锚点）。
    pub fn insert_trade_submitting(&self, t: &AgentTrade) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO agent_trades
                 (trade_id, run_id, client_order_id, strategy_version, reason,
                  account_input_summary, status, account_result_json, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    t.trade_id,
                    t.run_id,
                    t.client_order_id,
                    t.strategy_version,
                    t.reason,
                    t.account_input_summary,
                    enum_str(&AgentTradeStatus::Submitting),
                    Option::<String>::None,
                    ts(&t.created_at),
                    ts(&t.updated_at),
                ],
            )?;
            Ok(())
        })
    }

    /// 拿到 Account 结果后转 `settled`，写 account_result_ref。
    pub fn settle_trade(
        &self,
        trade_id: &str,
        result: &AccountResultRef,
        updated_at: OccurredAt,
    ) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE agent_trades SET status=?2, account_result_json=?3, updated_at=?4
                 WHERE trade_id=?1",
                params![
                    trade_id,
                    enum_str(&AgentTradeStatus::Settled),
                    serde_json::to_string(result).unwrap(),
                    ts(&updated_at),
                ],
            )?;
            Ok(())
        })
    }

    /// 启动恢复：所有 `submitting` 悬挂 trade。
    pub fn list_submitting_trades(&self) -> rusqlite::Result<Vec<AgentTrade>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT trade_id, run_id, client_order_id, strategy_version, reason,
                        account_input_summary, status, account_result_json, created_at, updated_at
                 FROM agent_trades WHERE status='submitting'",
            )?;
            let rows = stmt.query_map([], row_to_trade)?;
            rows.collect()
        })
    }

    /// 最近 AgentTrade，按创建时间倒序，带 offset 分页（spec §9 fetch_agent_state `trades`）。
    pub fn list_recent_trades(&self, limit: u32, offset: u32) -> rusqlite::Result<Vec<AgentTrade>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT trade_id, run_id, client_order_id, strategy_version, reason,
                        account_input_summary, status, account_result_json, created_at, updated_at
                 FROM agent_trades ORDER BY created_at DESC LIMIT ?1 OFFSET ?2",
            )?;
            let rows = stmt.query_map(params![limit, offset], row_to_trade)?;
            rows.collect()
        })
    }

    /// 自 `since` 起（含）创建的 AgentTrade，按时间升序。
    ///
    /// 用于「当日已下单意图」L3 注入（防自我打架，spec §6）：caller 传当日 0 点。
    pub fn list_trades_since(&self, since: OccurredAt) -> rusqlite::Result<Vec<AgentTrade>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT trade_id, run_id, client_order_id, strategy_version, reason,
                        account_input_summary, status, account_result_json, created_at, updated_at
                 FROM agent_trades WHERE created_at >= ?1 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map(params![ts(&since)], row_to_trade)?;
            rows.collect()
        })
    }

    /// 某 run 的全部 AgentTrade（account_trigger 的「原始建仓 run 摘要」用，spec §3/§6），按时间升序。
    pub fn list_trades_by_run(&self, run_id: &str) -> rusqlite::Result<Vec<AgentTrade>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT trade_id, run_id, client_order_id, strategy_version, reason,
                        account_input_summary, status, account_result_json, created_at, updated_at
                 FROM agent_trades WHERE run_id=?1 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map(params![run_id], row_to_trade)?;
            rows.collect()
        })
    }

    pub fn find_trade_by_client_order_id(
        &self,
        client_order_id: &str,
    ) -> rusqlite::Result<Option<AgentTrade>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT trade_id, run_id, client_order_id, strategy_version, reason,
                        account_input_summary, status, account_result_json, created_at, updated_at
                 FROM agent_trades WHERE client_order_id=?1",
                params![client_order_id],
                row_to_trade,
            )
            .optional()
        })
    }

    // ======================= orderId → runId 索引 =======================

    pub fn upsert_order_run_index(
        &self,
        order_id: &str,
        run_id: &str,
        trade_id: &str,
        client_order_id: &str,
        created_at: OccurredAt,
    ) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO agent_order_run_index (order_id, run_id, trade_id, client_order_id, created_at)
                 VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(order_id) DO UPDATE SET run_id=?2, trade_id=?3, client_order_id=?4",
                params![order_id, run_id, trade_id, client_order_id, ts(&created_at)],
            )?;
            Ok(())
        })
    }

    /// account_trigger 归因：order_id → (run_id, trade_id)。
    pub fn find_run_by_order_id(
        &self,
        order_id: &str,
    ) -> rusqlite::Result<Option<(String, String)>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT run_id, trade_id FROM agent_order_run_index WHERE order_id=?1",
                params![order_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
        })
    }

    // ======================= news buffer =======================

    /// 入队一条 pending（已存在则忽略，幂等）。返回是否新入队。
    pub fn push_news_pending(
        &self,
        news_id: &str,
        entered_at: OccurredAt,
        published_at: Option<OccurredAt>,
    ) -> rusqlite::Result<bool> {
        self.db.with(|c| {
            let n = c.execute(
                "INSERT OR IGNORE INTO agent_news_buffer (news_id, entered_at, published_at, status, run_id)
                 VALUES (?1,?2,?3,'pending',NULL)",
                params![news_id, ts(&entered_at), published_at.as_ref().map(ts)],
            )?;
            Ok(n > 0)
        })
    }

    pub fn count_pending_news(&self) -> rusqlite::Result<u64> {
        self.db.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM agent_news_buffer WHERE status='pending'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n.max(0) as u64)
        })
    }

    /// 取最新（publishedAt 降序，NULL 末尾）的 ≤limit 条 pending newsId。
    pub fn take_newest_pending(&self, limit: u32) -> rusqlite::Result<Vec<String>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT news_id FROM agent_news_buffer WHERE status='pending'
                 ORDER BY (published_at IS NULL), published_at DESC, entered_at DESC
                 LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit], |r| r.get::<_, String>(0))?;
            rows.collect()
        })
    }

    pub fn mark_news_in_batch(&self, news_ids: &[String], run_id: &str) -> rusqlite::Result<()> {
        self.db.with(|c| {
            let tx = c.transaction()?;
            for id in news_ids {
                tx.execute(
                    "UPDATE agent_news_buffer SET status='in_batch', run_id=?2 WHERE news_id=?1",
                    params![id, run_id],
                )?;
            }
            tx.commit()
        })
    }

    pub fn mark_news_analyzed(&self, news_ids: &[String]) -> rusqlite::Result<()> {
        self.db.with(|c| {
            let tx = c.transaction()?;
            for id in news_ids {
                tx.execute(
                    "UPDATE agent_news_buffer SET status='analyzed' WHERE news_id=?1",
                    params![id],
                )?;
            }
            tx.commit()
        })
    }

    /// 可恢复失败：本批 `in_batch` 回 `pending`（清 run_id），下个触发窗重试（spec §5）。
    pub fn revert_news_to_pending(&self, news_ids: &[String]) -> rusqlite::Result<()> {
        self.db.with(|c| {
            let tx = c.transaction()?;
            for id in news_ids {
                tx.execute(
                    "UPDATE agent_news_buffer SET status='pending', run_id=NULL WHERE news_id=?1",
                    params![id],
                )?;
            }
            tx.commit()
        })
    }

    /// 不可恢复失败：本批 `in_batch` 标 `dropped`（不再重试，spec §5）。
    pub fn mark_news_dropped(&self, news_ids: &[String]) -> rusqlite::Result<()> {
        self.db.with(|c| {
            let tx = c.transaction()?;
            for id in news_ids {
                tx.execute(
                    "UPDATE agent_news_buffer SET status='dropped' WHERE news_id=?1",
                    params![id],
                )?;
            }
            tx.commit()
        })
    }

    /// age-out：超 cutoff 仍 pending 的标 dropped，返回丢弃条数（emit 计数用）。
    pub fn age_out_news(&self, cutoff: OccurredAt) -> rusqlite::Result<u64> {
        self.db.with(|c| {
            let n = c.execute(
                "UPDATE agent_news_buffer SET status='dropped'
                 WHERE status='pending' AND entered_at < ?1",
                params![ts(&cutoff)],
            )?;
            Ok(n as u64)
        })
    }

    /// 读一条 buffer 项的 `(status, run_id?)`（不存在返回 None）。测试 + 诊断用。
    pub fn news_buffer_status(
        &self,
        news_id: &str,
    ) -> rusqlite::Result<Option<(String, Option<String>)>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT status, run_id FROM agent_news_buffer WHERE news_id=?1",
                params![news_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()
        })
    }

    /// 启动恢复：`in_batch` 但所属 run 已不存在/失败的回 `pending`。
    pub fn reset_orphan_in_batch_news(&self) -> rusqlite::Result<u64> {
        self.db.with(|c| {
            let n = c.execute(
                "UPDATE agent_news_buffer SET status='pending', run_id=NULL
                 WHERE status='in_batch' AND (run_id IS NULL OR run_id NOT IN
                     (SELECT run_id FROM agent_runs WHERE status='running'))",
                [],
            )?;
            Ok(n as u64)
        })
    }

    // ======================= 事件消费幂等 =======================

    /// 尝试开始消费。已是 `consumed`/`ignored`/`processing` → 返回 false（不重复触发）；
    /// 新建（或之前 `failed` 可重试）→ 置 `processing` 返回 true。
    pub fn begin_consumption(
        &self,
        event_type: &str,
        event_key: &str,
        consumer: &str,
        now: OccurredAt,
    ) -> rusqlite::Result<bool> {
        self.db.with(|c| {
            let existing: Option<String> = c
                .query_row(
                    "SELECT status FROM agent_event_consumption
                     WHERE event_type=?1 AND event_key=?2 AND consumer=?3",
                    params![event_type, event_key, consumer],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            match existing.as_deref() {
                Some("consumed") | Some("ignored") | Some("processing") => Ok(false),
                _ => {
                    c.execute(
                        "INSERT INTO agent_event_consumption
                         (event_type, event_key, consumer, status, run_id, error, created_at, updated_at)
                         VALUES (?1,?2,?3,'processing',NULL,NULL,?4,?4)
                         ON CONFLICT(event_type,event_key,consumer)
                         DO UPDATE SET status='processing', error=NULL, updated_at=?4",
                        params![event_type, event_key, consumer, ts(&now)],
                    )?;
                    Ok(true)
                }
            }
        })
    }

    pub fn mark_consumption(
        &self,
        event_type: &str,
        event_key: &str,
        consumer: &str,
        status: &str,
        run_id: Option<&str>,
        error: Option<&str>,
        now: OccurredAt,
    ) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "UPDATE agent_event_consumption SET status=?4, run_id=?5, error=?6, updated_at=?7
                 WHERE event_type=?1 AND event_key=?2 AND consumer=?3",
                params![event_type, event_key, consumer, status, run_id, error, ts(&now)],
            )?;
            Ok(())
        })
    }

    /// 启动恢复 ③（spec §8「processing 超时可回收」）：列出所有未终态（`processing`）的事件消费记录
    /// （(event_type, event_key, consumer)）。重启 = 进程内执行被中断，故启动时一律视为「超时」回收。
    pub fn list_processing_consumptions(
        &self,
    ) -> rusqlite::Result<Vec<(String, String, String)>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT event_type, event_key, consumer FROM agent_event_consumption
                 WHERE status='processing'",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            rows.collect()
        })
    }

    /// 启动恢复 ③：把一条 `processing` 记录回收为 `failed`（标 error，可被下次 `begin_consumption`
    /// 重新处理——`begin` 对非 consumed/ignored/processing 的记录会重置为 processing，spec §8）。
    pub fn reset_processing_consumption(
        &self,
        event_type: &str,
        event_key: &str,
        consumer: &str,
        now: OccurredAt,
    ) -> rusqlite::Result<()> {
        self.mark_consumption(
            event_type,
            event_key,
            consumer,
            "failed",
            None,
            Some("interrupted_by_restart"),
            now,
        )
    }

    // ======================= heartbeat =======================

    pub fn record_heartbeat_ok(&self, loop_name: &str, now: OccurredAt) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO agent_heartbeats (loop_name, last_ok_at, consecutive_failures)
                 VALUES (?1,?2,0)
                 ON CONFLICT(loop_name) DO UPDATE SET last_ok_at=?2, consecutive_failures=0",
                params![loop_name, ts(&now)],
            )?;
            Ok(())
        })
    }

    pub fn record_heartbeat_error(
        &self,
        loop_name: &str,
        err: &str,
        now: OccurredAt,
    ) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO agent_heartbeats (loop_name, last_error_at, last_error, consecutive_failures)
                 VALUES (?1,?2,?3,1)
                 ON CONFLICT(loop_name) DO UPDATE SET last_error_at=?2, last_error=?3,
                     consecutive_failures = consecutive_failures + 1",
                params![loop_name, ts(&now), err],
            )?;
            Ok(())
        })
    }

    /// 读某 loop 的 heartbeat：`(last_ok_at, last_error_at, last_error, consecutive_failures)`。
    /// 缺失返回 None。可观测/测试用（如 mapping_missing 是否记了 error）。
    #[allow(clippy::type_complexity)]
    pub fn get_heartbeat(
        &self,
        loop_name: &str,
    ) -> rusqlite::Result<Option<(Option<String>, Option<String>, Option<String>, i64)>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT last_ok_at, last_error_at, last_error, consecutive_failures
                 FROM agent_heartbeats WHERE loop_name=?1",
                params![loop_name],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
        })
    }

    // ======================= Settings kv (spec §8) =======================

    /// 读单个 settings 值（缺失返回 None）。
    pub fn get_setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT value FROM agent_settings WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })
    }

    /// 写 settings 值（upsert）。
    pub fn set_setting(&self, key: &str, value: &str, now: OccurredAt) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO agent_settings (key, value, updated_at)
                 VALUES (?1,?2,?3)
                 ON CONFLICT(key) DO UPDATE SET value=?2, updated_at=?3",
                params![key, value, ts(&now)],
            )?;
            Ok(())
        })
    }

    /// 读全部 settings（key → value）。
    pub fn get_all_settings(&self) -> rusqlite::Result<Vec<(String, String)>> {
        self.db.with(|c| {
            let mut stmt = c.prepare("SELECT key, value FROM agent_settings ORDER BY key")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // ======================= ReviewSuggestion（复盘 follow-up）=======================

    /// 落一条复盘策略建议（review run 经 `record_review_suggestion` 声明，spec §3 ④）。
    pub fn insert_review_suggestion(&self, s: &ReviewSuggestion) -> rusqlite::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO agent_review_suggestions
                 (suggestion_id, review_run_id, trade_date, text, created_at)
                 VALUES (?1,?2,?3,?4,?5)",
                params![
                    s.suggestion_id,
                    s.review_run_id,
                    s.trade_date.to_string(),
                    s.text,
                    ts(&s.created_at),
                ],
            )?;
            Ok(())
        })
    }

    /// 读某交易日的全部复盘建议（下次 review follow-up 对账用，spec §3 ④），按 created_at 升序。
    pub fn list_review_suggestions_by_date(
        &self,
        trade_date: &TradeDate,
    ) -> rusqlite::Result<Vec<ReviewSuggestion>> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT suggestion_id, review_run_id, trade_date, text, created_at
                 FROM agent_review_suggestions WHERE trade_date=?1 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map(params![trade_date.to_string()], row_to_review_suggestion)?;
            rows.collect()
        })
    }

    // 注：当日组合「日初权益」基线已下沉到 Account（账户财务事实单一所有者，spec
    // account-module.md §2「账户财务事实只读 facade」）—— 见 AccountRepository::observe_day_equity /
    // AccountService::daily_return。原 agent_daily_equity 表的读写已撤；表本身因 migration append-only
    // 约束保留为历史 migration（不再读写）。
}

fn row_to_review_suggestion(r: &rusqlite::Row) -> rusqlite::Result<ReviewSuggestion> {
    let td: String = r.get(2)?;
    Ok(ReviewSuggestion {
        suggestion_id: r.get(0)?,
        review_run_id: r.get(1)?,
        trade_date: TradeDate::parse(&td)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e)))?,
        text: r.get(3)?,
        created_at: parse_ts(&r.get::<_, String>(4)?)?,
    })
}

// ---- row mappers ------------------------------------------------------------

fn row_to_run(r: &rusqlite::Row) -> rusqlite::Result<AgentRun> {
    let trigger_json: String = r.get(2)?;
    let trigger: AgentRunTrigger = serde_json::from_str(&trigger_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e)))?;
    Ok(AgentRun {
        run_id: r.get(0)?,
        mode: enum_from(&r.get::<_, String>(1)?)?,
        trigger,
        parent_run_id: r.get(3)?,
        provider: r.get(4)?,
        wire_format: enum_from(&r.get::<_, String>(5)?)?,
        model: r.get(6)?,
        strategy_version: r.get(7)?,
        causation_run_id: r.get(8)?,
        status: enum_from(&r.get::<_, String>(9)?)?,
        started_at: opt_ts(r.get(10)?)?,
        ended_at: opt_ts(r.get(11)?)?,
        error: r.get(12)?,
    })
}

fn row_to_strategy(r: &rusqlite::Row) -> rusqlite::Result<InvestmentStrategy> {
    Ok(InvestmentStrategy {
        strategy_id: r.get(0)?,
        version: r.get(1)?,
        strategy: r.get(2)?,
        status: enum_from::<StrategyStatus>(&r.get::<_, String>(3)?)?,
        created_at: parse_ts(&r.get::<_, String>(4)?)?,
        updated_at: parse_ts(&r.get::<_, String>(5)?)?,
    })
}

fn row_to_analysis(r: &rusqlite::Row) -> rusqlite::Result<AnalysisResult> {
    let related_json: String = r.get(4)?;
    let trade_json: String = r.get(5)?;
    Ok(AnalysisResult {
        result_id: r.get(0)?,
        run_id: r.get(1)?,
        kind: enum_from(&r.get::<_, String>(2)?)?,
        summary: r.get(3)?,
        related_codes: serde_json::from_str(&related_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
        })?,
        trade_ids: serde_json::from_str(&trade_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
        })?,
        created_at: parse_ts(&r.get::<_, String>(6)?)?,
    })
}

fn row_to_trade(r: &rusqlite::Row) -> rusqlite::Result<AgentTrade> {
    let result_json: Option<String> = r.get(7)?;
    let account_result_ref = match result_json {
        Some(j) => Some(serde_json::from_str::<AccountResultRef>(&j).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?),
        None => None,
    };
    Ok(AgentTrade {
        trade_id: r.get(0)?,
        run_id: r.get(1)?,
        client_order_id: r.get(2)?,
        strategy_version: r.get(3)?,
        reason: r.get(4)?,
        account_input_summary: r.get(5)?,
        status: enum_from(&r.get::<_, String>(6)?)?,
        account_result_ref,
        created_at: parse_ts(&r.get::<_, String>(8)?)?,
        updated_at: parse_ts(&r.get::<_, String>(9)?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::runtime::{
        AgentRunMode, AgentRunTrigger, AnalysisResult, AnalysisResultKind, ReviewSuggestion,
    };
    use crate::domain::agent::WireFormat;
    use crate::domain::shared::{TradeDate, TsCode};
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::db::run_migrations;

    fn repo() -> AgentRuntimeRepo {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        AgentRuntimeRepo::new(db)
    }

    fn sample_run(id: &str) -> AgentRun {
        AgentRun {
            run_id: id.into(),
            mode: AgentRunMode::News,
            trigger: AgentRunTrigger::NewsBatch { news_ids: vec!["n1".into()] },
            parent_run_id: None,
            provider: "anthropic".into(),
            wire_format: WireFormat::Messages,
            model: "claude".into(),
            strategy_version: Some(2),
            causation_run_id: None,
            status: AgentRunStatus::Running,
            started_at: Some(Utc::now()),
            ended_at: None,
            error: None,
        }
    }

    #[test]
    fn run_insert_get_status_roundtrip() {
        let r = repo();
        let run = sample_run("run1");
        r.insert_run(&run, Utc::now()).unwrap();
        let got = r.get_run("run1").unwrap().unwrap();
        assert_eq!(got.mode, AgentRunMode::News);
        assert_eq!(got.strategy_version, Some(2));
        assert!(matches!(got.trigger, AgentRunTrigger::NewsBatch { .. }));

        assert_eq!(r.list_running_runs().unwrap().len(), 1);
        r.set_run_status("run1", AgentRunStatus::Completed, None, Some(Utc::now()), None)
            .unwrap();
        assert_eq!(r.list_running_runs().unwrap().len(), 0);
        assert_eq!(r.get_run("run1").unwrap().unwrap().status, AgentRunStatus::Completed);
    }

    #[test]
    fn strategy_versioning_and_active() {
        let r = repo();
        let now = Utc::now();
        r.insert_strategy_version(
            &InvestmentStrategy {
                strategy_id: "s".into(),
                version: 1,
                strategy: "v1".into(),
                status: StrategyStatus::Active,
                created_at: now,
                updated_at: now,
            },
            Some("seed"),
        )
        .unwrap();
        r.insert_strategy_version(
            &InvestmentStrategy {
                strategy_id: "s".into(),
                version: 2,
                strategy: "v2 更保守".into(),
                status: StrategyStatus::Active,
                created_at: now,
                updated_at: now,
            },
            Some("用户确认收紧仓位"),
        )
        .unwrap();
        let active = r.active_strategy().unwrap().unwrap();
        assert_eq!(active.version, 2);
        assert_eq!(active.strategy, "v2 更保守");
        assert_eq!(r.latest_version("s").unwrap(), Some(2));
        assert_eq!(r.list_strategy_versions("s").unwrap().len(), 2);
    }

    #[test]
    fn trade_submitting_then_settled_and_order_index() {
        let r = repo();
        r.insert_run(&sample_run("run1"), Utc::now()).unwrap();
        let now = Utc::now();
        let tr = AgentTrade {
            trade_id: "t1".into(),
            run_id: "run1".into(),
            client_order_id: "co1".into(),
            strategy_version: Some(2),
            reason: "开仓".into(),
            account_input_summary: "buy 600519 100".into(),
            status: AgentTradeStatus::Submitting,
            account_result_ref: None,
            created_at: now,
            updated_at: now,
        };
        r.insert_trade_submitting(&tr).unwrap();
        assert_eq!(r.list_submitting_trades().unwrap().len(), 1);
        assert!(r.find_trade_by_client_order_id("co1").unwrap().is_some());

        let res = AccountResultRef {
            accepted: true,
            order_id: Some("ord1".into()),
            fill_ids: vec!["f1".into()],
            position_id: Some("pos1".into()),
            account_event_ids: vec!["e1".into()],
            rejection_event_id: None,
            reason: None,
            message: None,
        };
        r.settle_trade("t1", &res, Utc::now()).unwrap();
        assert_eq!(r.list_submitting_trades().unwrap().len(), 0);

        r.upsert_order_run_index("ord1", "run1", "t1", "co1", Utc::now()).unwrap();
        assert_eq!(
            r.find_run_by_order_id("ord1").unwrap(),
            Some(("run1".into(), "t1".into()))
        );
    }

    #[test]
    fn analysis_result_insert() {
        let r = repo();
        r.insert_run(&sample_run("run1"), Utc::now()).unwrap();
        r.insert_analysis_result(&AnalysisResult {
            result_id: "a1".into(),
            run_id: "run1".into(),
            kind: AnalysisResultKind::NoAction,
            summary: "已 price-in，观望".into(),
            related_codes: vec![TsCode::parse("600519.SH").unwrap()],
            trade_ids: vec![],
            created_at: Utc::now(),
        })
        .unwrap();
    }

    #[test]
    fn news_buffer_drain_newest_first_and_age_out() {
        let r = repo();
        let t0 = Utc::now();
        // 三条：older / mid / newest（按 published_at）。
        let older = t0 - chrono::Duration::hours(5); // 超 4h
        let mid = t0 - chrono::Duration::hours(1);
        let newest = t0;
        assert!(r.push_news_pending("old", older, Some(older)).unwrap());
        assert!(r.push_news_pending("mid", mid, Some(mid)).unwrap());
        assert!(r.push_news_pending("new", newest, Some(newest)).unwrap());
        // 重复入队幂等。
        assert!(!r.push_news_pending("new", newest, Some(newest)).unwrap());
        assert_eq!(r.count_pending_news().unwrap(), 3);

        // newest-first 取 2 条。
        let batch = r.take_newest_pending(2).unwrap();
        assert_eq!(batch, vec!["new".to_string(), "mid".to_string()]);
        r.mark_news_in_batch(&batch, "run1").unwrap();
        r.mark_news_analyzed(&batch).unwrap();
        assert_eq!(r.count_pending_news().unwrap(), 1); // 只剩 old

        // age-out：entered_at 超 4h 的 old 被丢弃。
        let cutoff = t0 - chrono::Duration::hours(4);
        assert_eq!(r.age_out_news(cutoff).unwrap(), 1);
        assert_eq!(r.count_pending_news().unwrap(), 0);
    }

    #[test]
    fn event_consumption_idempotent() {
        let r = repo();
        let now = Utc::now();
        assert!(r.begin_consumption("account-triggered", "tg1", "runtime", now).unwrap());
        // 再次 begin → false（processing 中，不重复触发）。
        assert!(!r.begin_consumption("account-triggered", "tg1", "runtime", now).unwrap());
        r.mark_consumption("account-triggered", "tg1", "runtime", "consumed", Some("run1"), None, now)
            .unwrap();
        // consumed 后仍 false。
        assert!(!r.begin_consumption("account-triggered", "tg1", "runtime", now).unwrap());
    }

    #[test]
    fn heartbeat_ok_resets_failures() {
        let r = repo();
        let now = Utc::now();
        r.record_heartbeat_error("news_loop", "boom", now).unwrap();
        r.record_heartbeat_error("news_loop", "boom2", now).unwrap();
        r.record_heartbeat_ok("news_loop", now).unwrap();
        let n: i64 = r
            .db
            .with(|c| {
                c.query_row(
                    "SELECT consecutive_failures FROM agent_heartbeats WHERE loop_name='news_loop'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn review_suggestion_insert_and_list_by_date() {
        let r = repo();
        let td = TradeDate::parse("20260604").unwrap();
        let s = ReviewSuggestion {
            suggestion_id: "rs_1".into(),
            review_run_id: "rev1".into(),
            trade_date: td.clone(),
            text: "单票上限收紧到 15%".into(),
            created_at: Utc::now(),
        };
        r.insert_review_suggestion(&s).unwrap();
        let got = r.list_review_suggestions_by_date(&td).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "单票上限收紧到 15%");
        assert_eq!(got[0].review_run_id, "rev1");
        // 别的交易日读不到。
        assert!(r
            .list_review_suggestions_by_date(&TradeDate::parse("20260605").unwrap())
            .unwrap()
            .is_empty());
    }

    // 注：当日日初权益基线幂等性测试已随计算下沉到 Account
    // （见 AccountRepository::observe_day_equity 单测 / AccountService::daily_return 单测）。
}
