//! news 待分析 buffer 消费策略 —— 滚动 4h / drain / 最新优先 / M·N 触发 / age-out。
//!
//! Spec: docs/design/agent-runtime-module.md §5 news 分析机制
//!
//! 本层是 **policy**（阈值 + 触发判定 + age-out 窗口计算）；底层队列存取在 `AgentRuntimeRepo`。
//! 生产者：`news-refreshed` 后把 `newIds ∪ updatedIds ∪ articleUpdatedNewsIds`（去重）ingest。
//! 触发：`pending ≥ M` 立即 OR 每 `N` 兜底（兜底由 scheduler tick 驱动）。
//! 消费：取最新 ≤M 条 → 建 news run → mark in_batch → 成功 mark analyzed（drain）。
//! age-out：`enteredAt` 超 `window`（缺省 4h）仍 pending → dropped（返回计数供 emit）。

use std::sync::Arc;

use chrono::{Duration, Utc};

use crate::domain::shared::OccurredAt;
use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;

/// news buffer 阈值（来自 Runtime settings，spec §8）。
#[derive(Debug, Clone, Copy)]
pub struct NewsBufferConfig {
    /// `news_agent_batch_size`：pending ≥ M 立即触发；也是单批上限。缺省 50。
    pub batch_size: u32,
    /// `news_agent_max_wait_secs`：兜底触发间隔（scheduler 用）。缺省 600。
    pub max_wait_secs: u64,
    /// `news_buffer_window_secs`：回填 + age-out 窗口。缺省 14400（4h）。
    pub window_secs: i64,
}

impl Default for NewsBufferConfig {
    fn default() -> Self {
        Self {
            batch_size: 50,
            max_wait_secs: 600,
            window_secs: 14_400,
        }
    }
}

/// 生产者合并：`newIds ∪ updatedIds ∪ articleUpdatedNewsIds` 去重，保序（spec §5）。
/// 纯 failed/warnings 变化的 id 不在这三组里，自然不入队。
pub fn merge_refreshed_ids<'a>(
    new_ids: &'a [String],
    updated_ids: &'a [String],
    article_updated_ids: &'a [String],
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for id in new_ids
        .iter()
        .chain(updated_ids.iter())
        .chain(article_updated_ids.iter())
    {
        if seen.insert(id.as_str()) {
            out.push(id.clone());
        }
    }
    out
}

pub struct NewsBufferService {
    repo: Arc<AgentRuntimeRepo>,
    config: NewsBufferConfig,
}

impl NewsBufferService {
    pub fn new(repo: Arc<AgentRuntimeRepo>, config: NewsBufferConfig) -> Self {
        Self { repo, config }
    }

    pub fn config(&self) -> NewsBufferConfig {
        self.config
    }

    /// 生产者入队（已存在则忽略，幂等）。返回新入队条数。
    /// `items`：`(news_id, published_at?)`；`entered_at` 取 `now`（滚动 4h 锚点）。
    pub fn ingest(
        &self,
        items: &[(String, Option<OccurredAt>)],
        now: OccurredAt,
    ) -> rusqlite::Result<usize> {
        let mut added = 0;
        for (id, pub_at) in items {
            if self.repo.push_news_pending(id, now, *pub_at)? {
                added += 1;
            }
        }
        Ok(added)
    }

    pub fn pending_count(&self) -> rusqlite::Result<u64> {
        self.repo.count_pending_news()
    }

    /// 是否应因数量阈值立即触发（`pending ≥ M`）。
    pub fn should_trigger_now(&self) -> rusqlite::Result<bool> {
        Ok(self.pending_count()? >= self.config.batch_size as u64)
    }

    /// 取最新 ≤M 条 pending（newest-first）。调用方随后建 run 并 `mark_in_batch`。
    pub fn take_batch(&self) -> rusqlite::Result<Vec<String>> {
        self.repo.take_newest_pending(self.config.batch_size)
    }

    pub fn mark_in_batch(&self, news_ids: &[String], run_id: &str) -> rusqlite::Result<()> {
        self.repo.mark_news_in_batch(news_ids, run_id)
    }

    /// 成功分析后 drain（标 analyzed）。
    pub fn mark_analyzed(&self, news_ids: &[String]) -> rusqlite::Result<()> {
        self.repo.mark_news_analyzed(news_ids)
    }

    /// 可恢复失败：本批回 pending（清 run_id），下个触发窗重试（spec §5）。
    pub fn revert_to_pending(&self, news_ids: &[String]) -> rusqlite::Result<()> {
        self.repo.revert_news_to_pending(news_ids)
    }

    /// 不可恢复失败：本批标 dropped（不再重试，spec §5）。
    pub fn mark_dropped(&self, news_ids: &[String]) -> rusqlite::Result<()> {
        self.repo.mark_news_dropped(news_ids)
    }

    /// age-out：丢弃 `enteredAt < now - window` 仍 pending 的条目，返回丢弃数（供 emit 计数）。
    pub fn age_out(&self, now: OccurredAt) -> rusqlite::Result<u64> {
        let cutoff = now - Duration::seconds(self.config.window_secs);
        self.repo.age_out_news(cutoff)
    }

    /// 启动恢复：孤儿 `in_batch`（所属 run 非 running）回 pending。
    pub fn reset_orphans(&self) -> rusqlite::Result<u64> {
        self.repo.reset_orphan_in_batch_news()
    }

    /// 便利：当前时刻 age-out。
    pub fn age_out_now(&self) -> rusqlite::Result<u64> {
        self.age_out(Utc::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::db::{run_migrations, AppDb};

    fn svc(cfg: NewsBufferConfig) -> NewsBufferService {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        NewsBufferService::new(Arc::new(AgentRuntimeRepo::new(db)), cfg)
    }

    #[test]
    fn merge_refreshed_ids_unions_and_dedups_in_order() {
        let new_ids = vec!["a".to_string(), "b".to_string()];
        let updated = vec!["b".to_string(), "c".to_string()]; // b 与 new 重复
        let article = vec!["c".to_string(), "d".to_string()]; // c 重复
        let merged = merge_refreshed_ids(&new_ids, &updated, &article);
        // 三组并集去重，保序（new → updated → article）。
        assert_eq!(merged, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn ingest_dedups_and_counts() {
        let s = svc(NewsBufferConfig::default());
        let now = Utc::now();
        let added = s
            .ingest(&[("a".into(), Some(now)), ("b".into(), Some(now)), ("a".into(), Some(now))], now)
            .unwrap();
        assert_eq!(added, 2); // a 去重
        assert_eq!(s.pending_count().unwrap(), 2);
    }

    #[test]
    fn triggers_at_batch_size() {
        let s = svc(NewsBufferConfig { batch_size: 3, ..Default::default() });
        let now = Utc::now();
        s.ingest(&[("a".into(), Some(now)), ("b".into(), Some(now))], now).unwrap();
        assert!(!s.should_trigger_now().unwrap()); // 2 < 3
        s.ingest(&[("c".into(), Some(now))], now).unwrap();
        assert!(s.should_trigger_now().unwrap()); // 3 >= 3
    }

    #[test]
    fn take_batch_newest_first_and_drain() {
        let s = svc(NewsBufferConfig { batch_size: 2, ..Default::default() });
        let t0 = Utc::now();
        let older = t0 - Duration::hours(2);
        let mid = t0 - Duration::hours(1);
        let newest = t0;
        s.ingest(
            &[("old".into(), Some(older)), ("mid".into(), Some(mid)), ("new".into(), Some(newest))],
            t0,
        )
        .unwrap();
        let batch = s.take_batch().unwrap();
        assert_eq!(batch, vec!["new".to_string(), "mid".to_string()]); // newest-first, ≤M
        s.mark_in_batch(&batch, "run1").unwrap();
        s.mark_analyzed(&batch).unwrap();
        assert_eq!(s.pending_count().unwrap(), 1); // 只剩 old
    }

    #[test]
    fn revert_to_pending_after_recoverable_failure() {
        let s = svc(NewsBufferConfig { batch_size: 5, ..Default::default() });
        let now = Utc::now();
        s.ingest(&[("a".into(), Some(now)), ("b".into(), Some(now))], now).unwrap();
        let batch = s.take_batch().unwrap();
        s.mark_in_batch(&batch, "run1").unwrap();
        assert_eq!(s.pending_count().unwrap(), 0); // 都 in_batch
        // 可恢复失败 → 回 pending（清 run_id）。
        s.revert_to_pending(&batch).unwrap();
        assert_eq!(s.pending_count().unwrap(), 2);
    }

    #[test]
    fn mark_dropped_after_unrecoverable_failure() {
        let s = svc(NewsBufferConfig { batch_size: 5, ..Default::default() });
        let now = Utc::now();
        s.ingest(&[("a".into(), Some(now))], now).unwrap();
        let batch = s.take_batch().unwrap();
        s.mark_in_batch(&batch, "run1").unwrap();
        s.mark_dropped(&batch).unwrap();
        // dropped 不再 pending，也不被重试。
        assert_eq!(s.pending_count().unwrap(), 0);
        assert_eq!(s.take_batch().unwrap().len(), 0);
    }

    #[test]
    fn age_out_by_window() {
        let s = svc(NewsBufferConfig { window_secs: 4 * 3600, ..Default::default() });
        let t0 = Utc::now();
        let stale = t0 - Duration::hours(5); // 超 4h
        let fresh = t0 - Duration::hours(1);
        // entered_at = now 参数（这里直接传 stale/fresh 作为入队时刻）。
        s.ingest(&[("stale".into(), Some(stale))], stale).unwrap();
        s.ingest(&[("fresh".into(), Some(fresh))], fresh).unwrap();
        assert_eq!(s.pending_count().unwrap(), 2);
        let dropped = s.age_out(t0).unwrap();
        assert_eq!(dropped, 1); // 只丢 stale（entered_at 超 4h）
        assert_eq!(s.pending_count().unwrap(), 1);
    }
}
