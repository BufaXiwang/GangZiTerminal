//! EOD review orchestration — 收盘复盘 + 基准对照 + follow-up + 报告落盘。
//!
//! Spec: docs/design/agent-runtime-module.md §3 / §6 编排流（review trigger）

use std::sync::Arc;

use chrono::{DateTime, FixedOffset, TimeZone, Timelike, Utc};

use crate::domain::agent::runtime::{AgentRun, AgentRunStatus, AgentRunTrigger};
use crate::domain::shared::{OccurredAt, TradeDate};

use super::{autonomous_task_msg, OrchestrationError, RuntimeServices};

/// 把 `TradeDate`（CN 日期）→ 当日 00:00:00..23:59:59 的 UTC 范围。
fn trade_date_range(td: &TradeDate) -> (DateTime<Utc>, DateTime<Utc>) {
    let cn = FixedOffset::east_opt(8 * 3600).expect("valid offset");
    let naive = td.as_naive();
    let start = cn
        .from_local_datetime(&naive.and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .with_timezone(&Utc);
    let end = cn
        .from_local_datetime(&naive.and_hms_opt(23, 59, 59).unwrap())
        .single()
        .unwrap()
        .with_timezone(&Utc);
    (start, end)
}
use crate::pipeline::agent_runtime::context::RealtimeSection;
use crate::pipeline::agent_runtime::executor::{execute_run, AgentEventSink, ExecuteRunParams};

/// 收盘复盘结果：run + 落盘报告路径（None = 写盘失败）。
#[derive(Debug, Clone)]
pub struct ReviewRunResult {
    pub run: AgentRun,
    pub report_path: Option<String>,
}

/// Runtime 确定性复盘段（spec §3 ②③④⑤）：由 Runtime 计算、既注入 agent 上下文又确定性写进报告。
pub(super) struct ReviewDeterminism {
    /// 样本量与置信度声明文本（不足时含「样本不足…」声明）。
    pub sample: String,
    /// 当日有效交易笔数是否 < `review_min_sample_trades`（true → 禁绩效结论）。
    pub sample_short: bool,
    /// 组合当日收益率 + 基准 + 逐指数超额（来源 = `core_indexes()` + Account 权益 + 日初基线）。
    pub benchmark: String,
    /// 上次建议 follow-up（ReviewSuggestion ↔ 后续 upsert ↔ 活跃版本，确定性对账，spec §3 ④）。
    pub followup: String,
}

impl RuntimeServices {
    /// 收盘后自动复盘（调度器每 tick 调）：CN 时间 ≥ `eod_review_time` 且当日未复盘 → 起一次 EOD review。
    pub async fn maybe_run_eod_review(&self) {
        let cn = FixedOffset::east_opt(8 * 3600).expect("offset");
        let now = Utc::now().with_timezone(&cn);
        let (h, m) = self.parse_eod_review_time();
        let after_close =
            now.hour() > h || (now.hour() == h && now.minute() >= m);
        if !after_close {
            return;
        }
        if self.channels.active().ok().flatten().is_none() {
            return;
        }
        let today = now.date_naive();
        let date = TradeDate::from_naive(today);
        let key = date.to_string();

        match self.triggers.begin(super::super::triggers::EV_EOD_REVIEW, &key) {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                tracing::warn!(target: "runtime.review", error = %e, "eod_review begin lock failed");
                return;
            }
        }

        match self.run_eod_review(date).await {
            Ok(res) => {
                let _ = self.triggers.mark_consumed(
                    super::super::triggers::EV_EOD_REVIEW,
                    &key,
                    Some(&res.run.run_id),
                );
            }
            Err(e) => {
                tracing::warn!(target: "runtime.review", error = %e, "auto eod review failed");
                let _ = self
                    .triggers
                    .mark_failed(super::super::triggers::EV_EOD_REVIEW, &key, &e.to_string());
            }
        }
    }

    /// 解析 settings `eod_review_time`（格式 `"HH:MM Asia/Shanghai"`）→ (hour, minute)。
    /// 解析失败 fail-closed 用缺省 15:30 + warn（spec §6/§8）。
    pub(super) fn parse_eod_review_time(&self) -> (u32, u32) {
        let raw = self.settings.eod_review_time();
        let time_part = raw.split_whitespace().next().unwrap_or("");
        let parse = || -> Option<(u32, u32)> {
            let (h, m) = time_part.split_once(':')?;
            let h: u32 = h.trim().parse().ok()?;
            let m: u32 = m.trim().parse().ok()?;
            if h < 24 && m < 60 {
                Some((h, m))
            } else {
                None
            }
        };
        match parse() {
            Some(hm) => hm,
            None => {
                tracing::warn!(
                    target: "runtime.review",
                    raw = %raw,
                    "invalid eod_review_time; falling back to default 15:30 (fail-closed)"
                );
                (15, 30)
            }
        }
    }

    /// 收盘复盘（只读 review-mode run，非 fork）。
    pub async fn run_eod_review(
        &self,
        trade_date: TradeDate,
    ) -> Result<ReviewRunResult, OrchestrationError> {
        let channel = self.active_channel()?;
        let providers = self.providers(&channel)?;

        let det = self.collect_review_determinism(&trade_date).await;
        let prev_report = self.read_prev_review_report(&trade_date);
        let realtime = self.collect_review_context(&trade_date, &det, prev_report.as_deref()).await;

        let buf: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
        let buf2 = buf.clone();
        let forward = self.event_sink.clone();
        let capture_sink: AgentEventSink = Arc::new(move |ev| {
            match &ev {
                crate::domain::agent::events::AgentEvent::TextDelta { delta, .. } => {
                    if let Ok(mut g) = buf2.lock() {
                        g.push_str(delta);
                    }
                }
                crate::domain::agent::events::AgentEvent::ToolStart { .. } => {
                    if let Ok(mut g) = buf2.lock() {
                        g.clear();
                    }
                }
                _ => {}
            }
            if let Some(f) = forward.as_ref() {
                f(ev);
            }
        });

        let task = autonomous_task_msg(
            "请基于系统提示中的当日决策链、账户结果与基准对照，对策略表现做客观复盘；样本不足时不得\
             给出绩效结论。",
        );

        let params = ExecuteRunParams {
            trigger: AgentRunTrigger::EodReview {
                trade_date: trade_date.clone(),
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
            event_sink: Some(capture_sink),
        cancel_registry: Some(self.cancel_registry.clone()),
        token_budget: self.token_budget,
        on_run_created: None,
        };
        let run = execute_run(&self.runs, &self.strategy, params).await?;

        if run.status == AgentRunStatus::Failed {
            return Ok(ReviewRunResult { run, report_path: None });
        }

        let conclusion = buf.lock().map(|g| g.clone()).unwrap_or_default();
        let report_path = self
            .write_review_report(&trade_date, &run.run_id, &det, &conclusion)
            .await;

        Ok(ReviewRunResult { run, report_path })
    }

    pub(super) async fn collect_review_determinism(&self, trade_date: &TradeDate) -> ReviewDeterminism {
        let (day_start, day_end) = trade_date_range(trade_date);
        let trades = self.deps.records.list_trades_in_range(day_start, day_end).unwrap_or_default();

        let benchmark = self.collect_benchmark(trade_date).await;
        let followup = self.collect_followup(trade_date);

        let n = trades.len() as u32;
        let sample_short = n < self.review_min_sample_trades;
        let sample = if sample_short {
            format!(
                "\u{26a0} 样本不足（当日 {n} 笔 < {} 笔）：不构成策略有效性证据，仅作过程复盘，禁止输出「策略有效/无效」绩效结论。",
                self.review_min_sample_trades
            )
        } else {
            format!("当日有效交易 {n} 笔，样本量足够，可做绩效评估。")
        };

        ReviewDeterminism { sample, sample_short, benchmark, followup }
    }

    pub(super) fn collect_followup(&self, trade_date: &TradeDate) -> String {
        let Some(prev_naive) = trade_date.as_naive().pred_opt() else {
            return "（无上一交易日，跳过 follow-up）".into();
        };
        let prev_td = TradeDate::from_naive(prev_naive);
        let suggestions = self
            .deps
            .records
            .list_review_suggestions_by_date(&prev_td)
            .unwrap_or_default();
        if suggestions.is_empty() {
            return format!("（上一交易日 {prev_td} 无登记的复盘策略建议，跳过 follow-up）");
        }

        let versions: Vec<(u32, OccurredAt, Option<String>)> = match self.strategy.active() {
            Ok(Some(active)) => self.strategy.list_versions(&active.strategy_id).unwrap_or_default(),
            _ => Vec::new(),
        };
        let active_version = versions.iter().map(|(v, _, _)| *v).max();

        let mut out = String::new();
        for s in &suggestions {
            let adopted_after: Option<&(u32, OccurredAt, Option<String>)> = versions
                .iter()
                .filter(|(_, updated_at, _)| *updated_at > s.created_at)
                .min_by_key(|(_, updated_at, _)| *updated_at);
            match adopted_after {
                Some((v, at, reason)) => {
                    out.push_str(&format!(
                        "\u{b7} 建议：「{}」\n  \u{2192} 已采纳：建议后发生过 upsert_investment_strategy（\u{2192} v{} @ {}{}）；当前活跃版本 v{}。\n",
                        s.text,
                        v,
                        at.to_rfc3339(),
                        reason.as_deref().map(|r| format!("，理由：{r}")).unwrap_or_default(),
                        active_version.unwrap_or(*v),
                    ));
                }
                None => {
                    out.push_str(&format!(
                        "\u{b7} 建议：「{}」\n  \u{2192} 未采纳：建议后未发生 upsert_investment_strategy；活跃策略仍为 v{}。\n",
                        s.text,
                        active_version.map(|v| v.to_string()).unwrap_or_else(|| "（无策略）".into()),
                    ));
                }
            }
        }
        out
    }

    pub(super) fn read_prev_review_report(&self, trade_date: &TradeDate) -> Option<String> {
        let prev = trade_date.as_naive().pred_opt()?;
        let prev_td = TradeDate::from_naive(prev);
        let path = self.reports_dir.join(format!("{}.md", prev_td));
        std::fs::read_to_string(path).ok()
    }

    pub(super) async fn collect_review_context(
        &self,
        trade_date: &TradeDate,
        det: &ReviewDeterminism,
        prev_report: Option<&str>,
    ) -> Vec<RealtimeSection> {
        let (day_start, day_end) = trade_date_range(trade_date);
        let trades = self.deps.records.list_trades_in_range(day_start, day_end).unwrap_or_default();
        let results = self
            .runtime_repo
            .list_analysis_results_in_range(day_start, day_end)
            .unwrap_or_default();

        let mut chain = String::new();
        for r in results.iter().take(40) {
            chain.push_str(&format!(
                "\u{b7} [{}] {}（run={}）\n",
                if matches!(r.kind, crate::domain::agent::runtime::AnalysisResultKind::Action) {
                    "action"
                } else {
                    "no_action"
                },
                r.summary,
                &r.run_id[..r.run_id.len().min(10)],
            ));
        }
        for t in &trades {
            chain.push_str(&format!("\u{b7} trade: {} \u{2014}\u{2014} {}\n", t.account_input_summary, t.reason));
        }
        if chain.is_empty() {
            chain.push_str("（当日无决策记录）");
        }

        let account = match self
            .deps
            .account
            .fetch(serde_json::json!({"include": {"snapshot": true, "positions": true}}))
            .await
        {
            Ok(j) => j
                .get("snapshot")
                .map(|s| s.to_string())
                .unwrap_or_else(|| "（无快照）".into()),
            Err(_) => "（账户读取失败）".into(),
        };

        let mut sample_note = det.sample.clone();
        if det.sample_short {
            sample_note.push_str(
                "\n（L3 指令：样本不足时不得下「策略有效/无效」结论，只做过程复盘。）",
            );
        }

        let prev = prev_report
            .map(|s| s.to_string())
            .unwrap_or_else(|| "（无上一交易日复盘报告，跳过 follow-up）".into());

        vec![
            RealtimeSection::new(format!("复盘日期 {}", trade_date), sample_note),
            RealtimeSection::new("当日决策链", chain),
            RealtimeSection::new("账户结果", account),
            RealtimeSection::new("组合收益 vs 基准（Runtime 确定性算，可直接引用）", det.benchmark.clone()),
            RealtimeSection::new(
                "上次建议 follow-up（Runtime 确定性对账：建议↔是否 upsert 采纳↔活跃版本，可直接引用）",
                det.followup.clone(),
            ),
            RealtimeSection::new("上次复盘报告全文（补充「采纳后表现」的定性参考）", prev),
            RealtimeSection::new(
                "报告要求",
                "产出复盘结论：①交易清单与按策略版本归因 ②组合 vs 基准超额（数字已由 Runtime 给，结合定性）\
                 ③样本量声明 ④上次建议 follow-up 的「采纳后表现」定性 ⑤决策质量（纪律/止损/no_action 是否恰当）\
                 ⑥策略评估与建议（不自动改策略；有建议时调用 record_review_suggestion 登记供下次对账）。",
            ),
        ]
    }

    pub(super) async fn collect_benchmark(&self, _trade_date: &TradeDate) -> String {
        let port_ret_pct: Option<f64> = self
            .deps
            .account
            .daily_return(Utc::now())
            .map(|r| r * 100.0);

        let mut out = String::new();
        match port_ret_pct {
            Some(p) => out.push_str(&format!("组合当日收益率 {p:+.2}%\n")),
            None => out.push_str("组合当日收益率不可算（账户权益或日初基线缺失）\n"),
        }

        let indexes = self.deps.quotes.core_indexes();
        if indexes.is_empty() {
            out.push_str("（无核心基准指数 core_indexes，跳过基准超额对照）\n");
            return out;
        }
        match self
            .deps
            .quotes
            .fetch(serde_json::json!({
                "tsCodes": indexes,
                "include": {"quote": true}
            }))
            .await
        {
            Ok(j) => {
                if let Some(items) = j.get("items").and_then(|v| v.as_array()) {
                    for it in items {
                        let code = it.get("tsCode").and_then(|v| v.as_str()).unwrap_or("?");
                        let pct = it
                            .get("quote")
                            .and_then(|q| q.get("changePercent"))
                            .and_then(|v| v.as_f64());
                        match (pct, port_ret_pct) {
                            (Some(b), Some(p)) => out.push_str(&format!(
                                "{code} 当日 {b:+.2}%；超额 = 组合 \u{2212} 基准 = {:+.2}%\n",
                                p - b
                            )),
                            (Some(b), None) => out.push_str(&format!(
                                "{code} 当日 {b:+.2}%；超额不可算（组合收益率缺失）\n"
                            )),
                            (None, _) => out.push_str(&format!("{code} 涨幅缺失\n")),
                        }
                    }
                } else {
                    out.push_str("（基准读取为空）\n");
                }
            }
            Err(_) => out.push_str("（基准读取失败）\n"),
        }
        out
    }

    pub(super) async fn write_review_report(
        &self,
        trade_date: &TradeDate,
        run_id: &str,
        det: &ReviewDeterminism,
        conclusion: &str,
    ) -> Option<String> {
        if let Err(e) = tokio::fs::create_dir_all(&self.reports_dir).await {
            tracing::warn!(target: "runtime.review", error = %e, "create reports dir failed");
            return None;
        }
        let path = self.reports_dir.join(format!("{}.md", trade_date));
        let body = format!(
            "# 收盘复盘 {date}\n\n- run_id: `{run_id}`\n\n## 样本量与置信度声明\n\n{sample}\n\n\
             ## 组合收益 vs 基准（vs core_indexes，超额 = 组合 \u{2212} 基准）\n\n{benchmark}\n\n\
             ## 上次建议 follow-up（建议 \u{2194} 是否 upsert 采纳 \u{2194} 活跃版本）\n\n{followup}\n\n\
             ---\n\n## Agent 复盘结论\n\n{conclusion}\n",
            date = trade_date,
            run_id = run_id,
            sample = det.sample,
            benchmark = det.benchmark,
            followup = det.followup,
            conclusion = if conclusion.trim().is_empty() {
                "（本次复盘无文本结论）"
            } else {
                conclusion
            },
        );

        let tmp = self.reports_dir.join(format!("{}.md.tmp", trade_date));
        if let Err(e) = tokio::fs::write(&tmp, &body).await {
            tracing::warn!(target: "runtime.review", error = %e, "write temp report failed");
            let _ = tokio::fs::remove_file(&tmp).await;
            return None;
        }
        match tokio::fs::rename(&tmp, &path).await {
            Ok(_) => Some(path.to_string_lossy().to_string()),
            Err(e) => {
                tracing::warn!(target: "runtime.review", error = %e, "rename report failed");
                let _ = tokio::fs::remove_file(&tmp).await;
                None
            }
        }
    }

    /// 列出已落盘的复盘报告（最新在前）：(文件名, 绝对路径)。
    pub fn list_review_reports(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.reports_dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) == Some("md") {
                    if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                        out.push((name.to_string(), p.to_string_lossy().to_string()));
                    }
                }
            }
        }
        out.sort_by(|a, b| b.0.cmp(&a.0));
        out
    }
}
