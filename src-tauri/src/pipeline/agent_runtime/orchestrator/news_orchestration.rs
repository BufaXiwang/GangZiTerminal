//! News batch orchestration — buffer 消费 / age-out / 自动分析开关。
//!
//! Spec: docs/design/agent-runtime-module.md §5 news 机制 / §6 编排流

use chrono::Utc;

use crate::domain::agent::runtime::{AgentRun, AgentRunStatus, AgentRunTrigger};
use crate::domain::shared::OccurredAt;

use super::{autonomous_task_msg, OrchestrationError, RuntimeServices};
use crate::pipeline::agent_runtime::context::RealtimeSection;
use crate::pipeline::agent_runtime::executor::{execute_run, ExecuteRunParams};

impl RuntimeServices {
    /// news：一批 news_ids → 隔离单次。L3 = 本批 news + 当日已下单意图。
    ///
    /// run 创建后经 `on_run_created` 把本批标 `in_batch`+runId（in-flight 保护，spec §5）。
    pub async fn run_news_batch(
        &self,
        news_ids: Vec<String>,
    ) -> Result<AgentRun, OrchestrationError> {
        let channel = self.active_channel()?;
        let providers = self.providers(&channel)?;
        let news_section = self.collect_news_batch(&news_ids).await;
        let record_instruction = self.news_record_instruction();
        let intents = self.collect_intraday_intents().await;

        // run 创建后（拿到真 run_id）立刻 mark_in_batch：in-flight 保护 + 标 runId（spec §5）。
        let buf = self.news_buffer.clone();
        let batch_for_mark = news_ids.clone();
        let on_run_created: Option<Box<dyn FnOnce(&str) + Send>> = Some(Box::new(move |run_id: &str| {
            if let Err(e) = buf.mark_in_batch(&batch_for_mark, run_id) {
                tracing::warn!(target: "runtime.sched.news", error = %e, "mark_in_batch failed");
            }
        }));

        // 自主 run 注入一条 user-role 任务指令消息驱动本轮（spec §3 L3 / §6）：L1/L2/L3 全在 system
        // prompt，无 user message → 发给 messages API 的 messages 数组为空 → provider 400。注入任务指令
        // 使 messages 非空并触发工具使用。
        let task = autonomous_task_msg(
            "请基于系统提示中的本批新闻与当前账户/行情/策略做出投资判断；形成结论后调用 \
             record_analysis 声明 action/no_action 及理由（含是否已 price-in / 为什么现在进还来得及）。",
        );

        let params = ExecuteRunParams {
            trigger: AgentRunTrigger::NewsBatch { news_ids },
            channel,
            providers,
            deps: self.deps.clone(),
            augment_registry: self.augment.clone(),
            realtime: vec![news_section, record_instruction, intents],
            input: vec![task],
            conversation_id: None,
            max_turns: self.max_turns,
            parent_run_id: None,
            repo: None,
            event_sink: self.event_sink.clone(),
        cancel_registry: Some(self.cancel_registry.clone()),
        token_budget: self.token_budget,
        on_run_created,
        };
        Ok(execute_run(&self.runs, &self.strategy, params).await?)
    }

    /// news 自动分析开关（spec §5：默认关闭）。
    pub fn news_auto_analysis_enabled(&self) -> bool {
        self.settings.news_auto_analysis_enabled()
    }

    /// 写 news 自动分析开关；false→true 时回填最近 `news_buffer_window_secs`（缺省 4h）内的 news
    /// 入 buffer（spec §5「开启→回填最近 4h news 入 buffer」）。返回回填条数。
    pub async fn set_news_auto_analysis_enabled(&self, enabled: bool) -> rusqlite::Result<usize> {
        let was = self.settings.news_auto_analysis_enabled();
        self.settings.set_news_auto_analysis_enabled(enabled)?;
        if enabled && !was {
            return Ok(self.backfill_recent_news().await);
        }
        Ok(0)
    }

    /// 回填最近窗口内的 news 入 buffer（开启自动分析时调，spec §5）。返回新入队条数。
    /// 经 News gateway 按 `publishedFrom` 查最近窗口的 news（带 publishedAt）→ ingest（去重）。
    async fn backfill_recent_news(&self) -> usize {
        let now = Utc::now();
        let window = self.news_buffer.config().window_secs;
        let from = now - chrono::Duration::seconds(window);
        let req = serde_json::json!({
            "publishedFrom": from.to_rfc3339(),
            "limit": 200,
            "order": "desc",
        });
        let resp = match self.deps.news.fetch(req).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(target: "runtime.news_buffer", code = ?e.code, "backfill fetch_news failed");
                return 0;
            }
        };
        let items: Vec<(String, Option<OccurredAt>)> = resp
            .get("items")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|it| {
                        let id = it.get("id").and_then(|v| v.as_str())?.to_string();
                        let pub_at = it
                            .get("publishedAt")
                            .and_then(|v| v.as_str())
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .map(|d| d.with_timezone(&Utc));
                        Some((id, pub_at))
                    })
                    .collect()
            })
            .unwrap_or_default();
        match self.news_buffer.ingest(&items, now) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(target: "runtime.news_buffer", error = %e, "backfill ingest failed");
                0
            }
        }
    }

    /// news buffer age-out：丢弃过期 pending 并 emit 计数（spec §5「必须让用户看见丢了多少」）。
    /// 返回丢弃条数。
    pub fn age_out_news_buffer(&self) -> u64 {
        let dropped = match self.news_buffer.age_out_now() {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(target: "runtime.sched.news", error = %e, "age_out failed");
                return 0;
            }
        };
        if dropped > 0 {
            let window = self.news_buffer.config().window_secs.max(0) as u32;
            tracing::warn!(target: "runtime.sched.news", dropped, window_secs = window, "news buffer age-out");
            if let Some(sink) = self.buffer_dropped_sink.as_ref() {
                sink(dropped as u32, window);
            }
        }
        dropped
    }

    /// news batch drain：在 `agent.news_batch` lock 下取批 → run → 成功 analyzed / 可恢复回 pending /
    /// 不可恢复 dropped（spec §5/§8）。`Some(run_id)` 表示本次跑了一批（成功）。
    /// `try_lock` 失败（已有 batch 在跑）→ 直接返回 None（in-flight 保护）。
    pub async fn drain_news_batch(&self) -> Option<String> {
        let _guard = match self.news_batch_lock.try_lock() {
            Ok(g) => g,
            Err(_) => return None, // 已有 news batch 在跑（spec §8 in-flight lock）
        };
        let batch = match self.news_buffer.take_batch() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(target: "runtime.sched.news", error = %e, "take_batch failed");
                return None;
            }
        };
        if batch.is_empty() {
            return None;
        }
        match self.run_news_batch(batch.clone()).await {
            Ok(run) if run.status == AgentRunStatus::Completed => {
                // 校验本 run 是否产出了 AnalysisResult（spec §3：news mode 必 emit）。
                let has_result = self.deps.records
                    .list_analysis_results_by_run(&run.run_id)
                    .map(|r| !r.is_empty())
                    .unwrap_or(false);
                if has_result {
                    if let Err(e) = self.news_buffer.mark_analyzed(&batch) {
                        tracing::warn!(target: "runtime.sched.news", error = %e, "mark_analyzed failed");
                    }
                    Some(run.run_id)
                } else {
                    // Completed 但无 AnalysisResult → 审计链断裂，回 pending 重试（spec §3 / §5）。
                    tracing::warn!(
                        target: "runtime.sched.news",
                        run_id = %run.run_id,
                        "news run Completed but no AnalysisResult produced → revert to pending"
                    );
                    if let Err(e) = self.news_buffer.revert_to_pending(&batch) {
                        tracing::warn!(target: "runtime.sched.news", error = %e, "revert_to_pending failed");
                    }
                    None
                }
            }
            Ok(run) => {
                // run 落到非 Completed 终态（failed/cancelled）→ 视为可恢复，本批回 pending 重试（spec §5）。
                tracing::warn!(
                    target: "runtime.sched.news",
                    run_id = %run.run_id, status = ?run.status,
                    "news run 非 Completed 终态 → 本批回 pending"
                );
                if let Err(e) = self.news_buffer.revert_to_pending(&batch) {
                    tracing::warn!(target: "runtime.sched.news", error = %e, "revert_to_pending failed");
                }
                None
            }
            Err(e) => {
                // 失败分流（spec §5）：可恢复回 pending 下窗重试；不可恢复 dropped。
                let recoverable = e.is_recoverable();
                tracing::warn!(
                    target: "runtime.sched.news",
                    error = %e, recoverable,
                    "run_news_batch failed"
                );
                let res = if recoverable {
                    self.news_buffer.revert_to_pending(&batch)
                } else {
                    self.news_buffer.mark_dropped(&batch)
                };
                if let Err(e2) = res {
                    tracing::warn!(target: "runtime.sched.news", error = %e2, "post-fail disposition failed");
                }
                None
            }
        }
    }

    /// 采集「本批 news」段：按 ids 取回 → 渲染标题/来源清单。
    pub(super) async fn collect_news_batch(&self, news_ids: &[String]) -> RealtimeSection {
        let content = match self
            .deps
            .news
            .fetch(serde_json::json!({"ids": news_ids, "limit": news_ids.len().max(1)}))
            .await
        {
            Ok(j) => super::summarize_news(&j),
            Err(_) => "（本批 news 读取失败，请用 fetch_news 按 ids 重试）".to_string(),
        };
        RealtimeSection::new("本批 news", content)
    }

    /// news L3 定论指令段（spec §3 产生机制 / §6）：引导 agent 形成判断后必调 record_analysis。
    pub(super) fn news_record_instruction(&self) -> RealtimeSection {
        RealtimeSection::new(
            "定论要求",
            "分析完本批 news 形成判断后，**必须**调用一次 record_analysis 声明结论：kind=action（已下单/改自选）\
             或 no_action（观望）；summary 用 **Markdown 格式**书写，包含：\n\
             1. **结论**（一句话：action/no_action + 核心判断）\n\
             2. **理由**（分点列出，含「为什么现在进还来得及 / 已 price-in / 情绪周期阶段」判断）\n\
             3. relatedCodes 填相关标的代码。大多数 news 应为 no_action。",
        )
    }
}
