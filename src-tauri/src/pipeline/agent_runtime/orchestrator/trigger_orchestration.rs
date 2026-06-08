//! Account trigger + eval tick orchestration。
//!
//! Spec: docs/design/agent-runtime-module.md §6 编排流（account_trigger / eval tick）

use crate::domain::agent::runtime::{AgentRun, AgentRunTrigger};

use super::{autonomous_task_msg, OrchestrationError, RuntimeServices, MAX_EVAL_DRAIN_BATCHES};
use crate::pipeline::agent_runtime::context::RealtimeSection;
use crate::pipeline::agent_runtime::executor::{execute_run, ExecuteRunParams};
use crate::pipeline::agent_runtime::triggers::Attribution;

impl RuntimeServices {
    /// 账户自驱 quote tick 节拍（spec §6/§8 `account_trigger_eval_interval_secs`，缺省 10s）。
    pub fn account_trigger_eval_interval_secs(&self) -> u64 {
        self.settings.account_trigger_eval_interval_secs()
    }

    /// account_trigger：账户事件 → 隔离单次。L3 = 当日已下单意图；run 起后归因到原始建仓 run。
    ///
    /// `order_id` 为触发关联的订单（止损/止盈命中的原单）；用于 orderId→runId 归因（spec §6）。
    pub async fn run_account_trigger(
        &self,
        trigger_id: String,
        order_id: Option<String>,
        event_summary: String,
    ) -> Result<AgentRun, OrchestrationError> {
        let channel = self.active_channel()?;
        let providers = self.providers(&channel)?;
        let intents = self.collect_intraday_intents().await;
        let event_section = RealtimeSection::new("触发事件", event_summary);

        // 归因前置（spec §3 mode 表 / §6）：execute_run **之前**先反查原始建仓 run，把其摘要注入本次
        // account_trigger run 的 L3，让模型看到原单的判断链（mapping_missing 在 resolve 内 warn+heartbeat）。
        let attr: Attribution = self.triggers.resolve_attribution(order_id.as_deref())?;
        let mut realtime = vec![event_section];
        let origin_run_id: Option<String> = match &attr {
            Attribution::Origin { run_id, .. } => {
                let summary = self
                    .triggers
                    .origin_run_summary(run_id)
                    .unwrap_or_else(|e| format!("（原始建仓 run 摘要读取失败：{e}）"));
                realtime.push(RealtimeSection::new("原始建仓 run 摘要", summary));
                Some(run_id.clone())
            }
            Attribution::MappingMissing => {
                realtime.push(RealtimeSection::new(
                    "原始建仓 run 摘要",
                    "（触发关联订单反查不到原始建仓 run：归因缺失 mapping_missing）".to_string(),
                ));
                None
            }
            Attribution::NoOrder => None,
        };
        realtime.push(intents);

        // run 创建后（拿到真 run_id）把 causation_run_id 写回（spec §3：把 causationRunId 写到本 run）。
        let triggers = self.triggers.clone();
        let on_run_created: Option<Box<dyn FnOnce(&str) + Send>> = origin_run_id.map(|origin| {
            let b: Box<dyn FnOnce(&str) + Send> = Box::new(move |run_id: &str| {
                if let Err(e) = triggers.set_causation(run_id, &origin) {
                    tracing::warn!(target: "runtime.account_trigger", error = %e, "set_causation failed");
                }
            });
            b
        });

        // 自主 run 注入一条 user-role 任务指令消息驱动本轮（spec §3 L3 / §6）：避免空 messages 数组。
        let task = autonomous_task_msg(
            "请基于系统提示中的账户触发事件、原始建仓 run 摘要与当前账户/行情，做出实时处置决策\
             （平仓/调仓/调整保护/不处置），并说明理由。",
        );

        let params = ExecuteRunParams {
            trigger: AgentRunTrigger::AccountTrigger {
                trigger_id: trigger_id.clone(),
            },
            channel,
            providers,
            deps: self.deps.clone(),
            augment_registry: self.augment.clone(),
            realtime,
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
        let run = execute_run(&self.runs, &self.strategy, params).await?;
        Ok(run)
    }

    /// 启动恢复 ⑤（spec §8）：从 Account 补扫未 handled trigger，对每个走既有 account_trigger 路由。
    ///
    /// dedupe 保证不重复（`begin_account_trigger` 幂等：已 consumed/processing → 跳过）。每个 run 终态后
    /// `mark_trigger_handled`（Account 侧）+ `mark_account_trigger_consumed`（Runtime 侧消费记录），与
    /// 实时 account-triggered 链一致。无 active channel → 跳过（无渠道时不应起跑自动 run）。
    /// 返回成功路由（起跑并终态）的 trigger 数。
    pub async fn rescan_unhandled_triggers(&self, limit: u32) -> u64 {
        if self.channels.active().ok().flatten().is_none() {
            return 0; // 无渠道：静默跳过（下次有渠道时定时 eval / 重启再补）。
        }
        let triggers = match self.deps.account.list_unhandled_triggers(limit).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(target: "runtime.recovery", error = %e.message, "list_unhandled_triggers failed");
                return 0;
            }
        };
        let mut routed = 0u64;
        for t in triggers {
            // dedupe：已消费 / 处理中 → 跳过（不重复）。
            match self.triggers.begin_account_trigger(&t.trigger_id) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    tracing::warn!(target: "runtime.recovery", error = %e, "begin_account_trigger failed");
                    continue;
                }
            }
            match self
                .run_account_trigger(t.trigger_id.clone(), t.order_id.clone(), t.summary.clone())
                .await
            {
                Ok(run) => {
                    if let Err(e) = self.deps.account.mark_trigger_handled(&t.trigger_id).await {
                        tracing::warn!(
                            target: "runtime.recovery",
                            trigger_id = %t.trigger_id,
                            error = %e.message,
                            "mark_trigger_handled failed (non-fatal)"
                        );
                    }
                    let _ = self
                        .triggers
                        .mark_account_trigger_consumed(&t.trigger_id, Some(&run.run_id));
                    routed += 1;
                }
                Err(e) => {
                    let _ = self
                        .triggers
                        .mark_account_trigger_failed(&t.trigger_id, &e.to_string());
                    tracing::warn!(target: "runtime.recovery", error = %e, "rescan account_trigger run failed");
                }
            }
        }
        routed
    }

    /// 账户自驱 quote tick（spec agent-runtime §6 行情 / 账户维护调度）。
    ///
    /// 账户评估的数据依赖是**有界的**——只需 `持仓 ∪ 挂单 ∪ 自选 = subscribed_codes`（再 ∪
    /// `core_indexes` 供 review 基准对照）的行情，与全市场无关。因此本 tick **不搭 universe 全市场
    /// 刷新的便车**（那会陪一批与账户无关的 BJ fallback，延时几十秒），而是自驱：
    ///
    /// ① `codes = subscribed_codes ∪ core_indexes`；codes 为空（空仓且无挂单且无自选）→ 跳过（零成本）。
    /// ② Quotes `refresh_quotes(codes)`——**focused、同步、恒 final=true、亚秒级**。
    /// ③ `rebuild_account_snapshot`。
    /// ④ `evaluate_account_triggers` 按 `has_more`/`next_cursor` 分页耗尽（命中的 trigger 经
    ///    `account-triggered` 事件流走既有 account_trigger 消费链）。
    pub async fn run_account_eval_tick(&self) {
        // ① 派生有界关注集合：subscribed ∪ core_indexes（去重）。
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut codes: Vec<String> = Vec::new();
        for c in self.deps.account.subscribed_codes() {
            if seen.insert(c.clone()) {
                codes.push(c);
            }
        }
        for c in self.deps.quotes.core_indexes() {
            if seen.insert(c.clone()) {
                codes.push(c);
            }
        }
        if codes.is_empty() {
            return; // 空仓且无挂单且无自选 → 跳过本 tick（零成本）。
        }

        // ② focused refresh（同步 final=true，亚秒级）。失败非致命：仍尝试用既有 snapshot 重建/评估。
        if let Err(e) = self.deps.quotes.refresh_quotes(codes).await {
            tracing::warn!(
                target: "runtime.account_tick",
                error = %e.message,
                "focused refresh_quotes failed (non-fatal); proceeding with existing snapshot"
            );
        }

        // ③ 重建账户快照。
        if let Err(e) = self.deps.account.rebuild_account_snapshot().await {
            tracing::warn!(
                target: "runtime.account_tick",
                error = %e.message,
                "rebuild_account_snapshot failed (non-fatal)"
            );
            return;
        }

        // ④ 评估触发器：按 cursor 分页耗尽（上界保护，避免单 tick 内死循环）。
        let mut cursor: Option<String> = None;
        let mut batches: u32 = 0;
        loop {
            match self
                .deps
                .account
                .evaluate_account_triggers(cursor.clone(), self.eval_batch_size)
                .await
            {
                Ok(page) => {
                    batches += 1;
                    if page.has_more && batches < MAX_EVAL_DRAIN_BATCHES {
                        if let Some(next) = page.next_cursor {
                            cursor = Some(next);
                            continue;
                        }
                        tracing::warn!(
                            target: "runtime.account_tick",
                            "has_more=true but next_cursor=None; stopping drain"
                        );
                    } else if page.has_more {
                        tracing::warn!(
                            target: "runtime.account_tick",
                            batches,
                            "eval drain budget exhausted; remainder rolls to fallback timer / next tick"
                        );
                    }
                    break;
                }
                Err(e) => {
                    tracing::warn!(
                        target: "runtime.account_tick",
                        error = %e.message,
                        "evaluate_account_triggers failed (non-fatal)"
                    );
                    return;
                }
            }
        }
    }
}
