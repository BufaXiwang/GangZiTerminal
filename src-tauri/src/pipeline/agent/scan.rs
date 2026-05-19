//! Scan pipeline——9-tick 两阶段架构（规则信号扫描 → LLM mini-scan）。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 5。
//!
//! **阶段 1（纯代码）**：对 watchlist 50 只逐个跑 24 个 signal detector +
//! news 距离上次 tick 增量。0 LLM 调用。
//!
//! **阶段 2（LLM mini-scan）**：仅当某只股触发 ≥2 signals 时，构造 mini-scan
//! prompt 喂 LLM 决定 create_expectation / update / cancel / no_action。
//! Budget 控制：单股 30min 内最多 2 次；全日上限 100 次。
//!
//! DDD 边界：本 pipeline 接受调用方注入 ToolRegistry（adapter 层构造），
//! 不直接 import `adapters::agent_tools`——遵循 pipeline → adapter 单向规则。

use crate::domain::agent::types::{
    AgentEvent, AgentOptions, AgentRequest, Block, ContextBudget, Message, PipelineKind,
    ProviderKind, Role, ServerSideTool, SystemBlock, ToolDef,
};
use crate::domain::quotes::indicators::{compute_indicators, IndicatorConfig};
use crate::domain::quotes::regime::Regime;
use crate::domain::quotes::types::KlinePoint;
use crate::domain::shared::signal::SignalKind;
use crate::domain::shared::{Lots, OccurredAt, StockCode, TradeDate, Yuan};
use crate::infrastructure::account::watchlist;
use crate::infrastructure::agent::signal_detection_repo;
use crate::infrastructure::news::news_tag_repo;
use crate::infrastructure::quotes::cache::kline_cache;
use crate::infrastructure::quotes::signal_detector::{self, DetectorConfig, ScanContext};
use crate::infrastructure::quotes::tushare::flow as ts_flow;
use crate::domain::quotes::{NorthMoneyFlow, TopListItem};
use crate::infrastructure::quotes::snapshot::market_snapshot;
use crate::pipeline::agent::config::{build_provider_for_channel, read_agent_config};
use crate::pipeline::agent::observer;
use crate::pipeline::agent::prompt::AGENT_IDENTITY;
use crate::pipeline::agent::run_agent;
use crate::pipeline::agent::tools::{ToolContext, ToolRegistry};
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

pub const EVENT_SCAN_TICK_COMPLETED: &str = "scan-tick-completed";

// ====== 配置 ============================================================

#[derive(Debug, Clone, Copy)]
pub struct ScanBudget {
    /// 单股 N 分钟内 mini-scan 次数上限
    pub per_stock_window_minutes: u32,
    pub per_stock_max_in_window: u32,
    /// 全日 LLM mini-scan 总次数上限
    pub daily_global_max: u32,
    /// 触发 mini-scan 的最小信号汇合数
    pub min_signals_for_mini_scan: usize,
}

impl Default for ScanBudget {
    fn default() -> Self {
        Self {
            per_stock_window_minutes: 30,
            per_stock_max_in_window: 2,
            daily_global_max: 100,
            min_signals_for_mini_scan: 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TickResult {
    pub tick_id: String,
    pub stocks_scanned: usize,
    pub signals_detected: usize,
    pub mini_scans_triggered: usize,
    pub mini_scans_skipped_budget: usize,
    pub mini_scans_failed: usize,
}

// ====== 阶段 1 入口：定时 tick =============================================

/// 跑一个 tick——扫所有自选股，对触发 ≥min_signals 的 spawn LLM mini-scan。
///
/// `trigger_kind` 落到 agent_episodes（"scheduled" / "user_chat" 等）；
/// `tick_label` 写到 trigger_ref（如 "10:10"）。
pub async fn run_tick(
    app: AppHandle,
    registry: Arc<ToolRegistry>,
    trigger_kind: &str,
    tick_label: &str,
    budget: ScanBudget,
) -> Result<TickResult, String> {
    let tick_id = uuid::Uuid::new_v4().to_string();
    let mut result = TickResult {
        tick_id: tick_id.clone(),
        stocks_scanned: 0,
        signals_detected: 0,
        mini_scans_triggered: 0,
        mini_scans_skipped_budget: 0,
        mini_scans_failed: 0,
    };

    let codes = watchlist::list_strings();
    let now = OccurredAt::now();

    // 一次性拉本 tick 的资金面 context——top_list 和 north_flow 都是当日全局数据，
    // 同一 tick 内每只股共享。失败时降级到空 context（detector 自动跳过资金面信号）。
    let (north_flow, dragon_tiger) = prefetch_capital_context(&app).await;

    // 阶段 1：纯规则扫描
    let mut all_detections: Vec<(String, SignalKind, OccurredAt)> = Vec::new();
    let mut triggered_per_stock: Vec<(StockCode, Vec<SignalKind>)> = Vec::new();
    for code_str in &codes {
        let Ok(code) = StockCode::new(code_str) else {
            continue;
        };
        result.stocks_scanned += 1;
        let signals = scan_one_stock_with_capital(
            &app,
            &code,
            now,
            north_flow.as_deref(),
            dragon_tiger.as_deref(),
        );
        if signals.is_empty() {
            continue;
        }
        result.signals_detected += signals.len();
        for sig in &signals {
            all_detections.push((code.as_str().to_string(), sig.clone(), now));
        }
        if signals.len() >= budget.min_signals_for_mini_scan {
            triggered_per_stock.push((code, signals));
        }
    }
    let _ = signal_detection_repo::record_batch(&app, &tick_id, &all_detections);

    // 阶段 2：LLM mini-scan（按 budget 过滤）
    let allowed = apply_budget(&app, &triggered_per_stock, budget);
    result.mini_scans_skipped_budget = triggered_per_stock.len() - allowed.len();

    for (code, signals) in allowed {
        match run_mini_scan(
            app.clone(),
            registry.clone(),
            code.clone(),
            signals,
            trigger_kind,
            &tick_id,
            tick_label,
        )
        .await
        {
            Ok(_) => result.mini_scans_triggered += 1,
            Err(e) => {
                result.mini_scans_failed += 1;
                tracing::warn!(code = %code, error = %e, "mini-scan 失败");
            }
        }
    }

    tracing::info!(
        tick_id = %tick_id,
        scanned = result.stocks_scanned,
        signals = result.signals_detected,
        mini_scans = result.mini_scans_triggered,
        skipped = result.mini_scans_skipped_budget,
        "scan tick 完成"
    );

    let _ = app.emit(
        EVENT_SCAN_TICK_COMPLETED,
        serde_json::json!({
            "tickId": result.tick_id.clone(),
            "tickLabel": tick_label,
            "stocksScanned": result.stocks_scanned,
            "signalsDetected": result.signals_detected,
            "miniScansTriggered": result.mini_scans_triggered,
        }),
    );

    Ok(result)
}

/// 在 tick 开始时一次性拉资金面数据——返回 (北向 10 日序列, 今日龙虎榜)。
/// 都失败时返 (None, None)，detector 会跳过资金面信号——降级行为符合"detector 字段 None 跳过"的约定。
pub async fn prefetch_capital_context(
    app: &AppHandle,
) -> (Option<Vec<NorthMoneyFlow>>, Option<Vec<TopListItem>>) {
    let north = match ts_flow::fetch_north_flow(app, 10).await {
        Ok(v) if !v.is_empty() => Some(v),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "scan tick: 拉北向资金失败，资金面信号本轮跳过");
            None
        }
    };
    let dragon = match ts_flow::fetch_top_list(app, None).await {
        Ok(v) if !v.is_empty() => Some(v),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "scan tick: 拉龙虎榜失败，OnDragonTigerList 本轮跳过");
            None
        }
    };
    (north, dragon)
}

/// 给单只股票跑信号检测——纯技术面 + news（不含资金面）。保留为兼容入口；
/// 调用方若想要资金面，请用 [`scan_one_stock_with_capital`]。
pub fn scan_one_stock(app: &AppHandle, code: &StockCode, now: OccurredAt) -> Vec<SignalKind> {
    scan_one_stock_with_capital(app, code, now, None, None)
}

/// 带资金面 context 的扫描——`north_flow` / `dragon_tiger` 由 tick 顶层一次性拉好后注入。
pub fn scan_one_stock_with_capital(
    app: &AppHandle,
    code: &StockCode,
    now: OccurredAt,
    north_flow: Option<&[NorthMoneyFlow]>,
    dragon_tiger: Option<&[TopListItem]>,
) -> Vec<SignalKind> {
    let mut out = Vec::new();

    // 拉 K 线（近 60 日）+ indicators
    let ts_code = to_ts_code(code);
    let rows = kline_cache::read_klines(
        app,
        &ts_code,
        crate::domain::quotes::types::KlinePeriod::Day,
        crate::domain::quotes::types::AdjMode::Qfq,
        60,
    );
    if rows.len() >= 30 {
        let klines = rows_to_kline_points(&rows);
        let cfg = IndicatorConfig::default();
        if let Some(snap) = compute_indicators(&klines, &cfg) {
            // prev_snap：把最后一条砍掉再算一遍——用于 cross 信号
            let prev_snap = if klines.len() >= 2 {
                compute_indicators(&klines[..klines.len() - 1], &cfg)
            } else {
                None
            };
            let quote = market_snapshot::get(&ts_code);
            let ctx = ScanContext {
                north_flow,
                dragon_tiger_today: dragon_tiger,
                ..ScanContext::default()
            };
            let signals = signal_detector::scan_one_with_context(
                &klines,
                &snap,
                prev_snap.as_ref(),
                quote.as_ref(),
                &DetectorConfig::default(),
                &ctx,
            );
            out.extend(signals);
        }
    }

    // News 信号——读距离 24h 前涉及该股的资讯，转 NewsCatalystMatched
    let since = OccurredAt::new(now.value() - 24 * 3600 * 1000);
    if let Ok(news_ids) = news_tag_repo::list_news_for_code_since(app, code.as_str(), since) {
        for nid in news_ids {
            if let Ok(Some(tags)) = news_tag_repo::get(app, &nid) {
                out.push(SignalKind::NewsCatalystMatched {
                    news_kind: tags.kind,
                    importance: tags.importance,
                });
            }
        }
    }

    out
}

// ====== Budget 过滤 =====================================================

fn apply_budget(
    app: &AppHandle,
    triggered: &[(StockCode, Vec<SignalKind>)],
    budget: ScanBudget,
) -> Vec<(StockCode, Vec<SignalKind>)> {
    let conn = match crate::infrastructure::db::open_database(app) {
        Ok(c) => c,
        Err(_) => return triggered.to_vec(),
    };
    // 全日上限
    let today_start = (chrono::Utc::now() - chrono::Duration::hours(8))
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .to_rfc3339();
    let daily: u32 = conn
        .query_row(
            "select count(*) from agent_episodes
             where trigger_kind = 'scan' and started_at >= ?1",
            rusqlite::params![today_start],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if daily >= budget.daily_global_max {
        tracing::warn!(daily, max = budget.daily_global_max, "scan budget: 当日上限已到");
        return Vec::new();
    }

    let mut remaining = budget.daily_global_max.saturating_sub(daily);
    let cutoff_min_ago = chrono::Utc::now()
        - chrono::Duration::minutes(budget.per_stock_window_minutes as i64);
    let cutoff_iso = cutoff_min_ago.to_rfc3339();
    let mut out = Vec::new();
    for (code, signals) in triggered {
        if remaining == 0 {
            break;
        }
        // 单股 30min 内 mini-scan 次数
        let per_stock: u32 = conn
            .query_row(
                "select count(*) from agent_episodes
                 where trigger_kind = 'scan' and started_at >= ?1
                       and trigger_ref like ?2",
                rusqlite::params![cutoff_iso, format!("%{}%", code.as_str())],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if per_stock >= budget.per_stock_max_in_window {
            continue;
        }
        out.push((code.clone(), signals.clone()));
        remaining = remaining.saturating_sub(1);
    }
    out
}

// ====== 阶段 2：LLM mini-scan ==========================================

/// 单只股票 LLM mini-scan——构造 prompt + 起 episode + run_agent。
///
/// 用法：scheduled tick 触发时由 `run_tick` 调用；chat 涉及自选股时由 `chat.rs` 调用。
pub async fn run_mini_scan(
    app: AppHandle,
    registry: Arc<ToolRegistry>,
    code: StockCode,
    signals: Vec<SignalKind>,
    trigger_kind: &str,
    tick_id: &str,
    tick_label: &str,
) -> Result<String, String> {
    let cfg = read_agent_config(&app);
    let (channel_ref, model_ref) = cfg.resolve_pipeline(PipelineKind::Chat)?;
    let channel = channel_ref.clone();
    let model = model_ref.to_string();

    let context_text = build_mini_scan_context(&app, &code, &signals);
    let mut tools = registry.to_tool_defs(true);
    // 跟 chat pipeline 同样的 server-side web_search 注入逻辑——9 tick 自驱
    // mini-scan 看到异动时也能 web 查最新消息（否则 agent 只看技术信号会瞎判）。
    let want_web_search = match channel.wire_format {
        ProviderKind::Anthropic => channel.enable_native_web_search,
        ProviderKind::OpenAIResponses => channel.enable_web_search,
        ProviderKind::OpenAIChatCompletions => false,
    };
    if want_web_search {
        tools.push(ToolDef::ServerSide(ServerSideTool::AnthropicWebSearch {
            name: "web_search".into(),
            max_uses: Some(cfg.agent.max_search_calls_per_run),
            allowed_domains: vec![],
            blocked_domains: vec![],
        }));
    }

    let req = AgentRequest {
        system: vec![
            SystemBlock {
                text: AGENT_IDENTITY.to_string(),
                cache_control: false,
            },
            SystemBlock {
                text: MINI_SCAN_INSTRUCTIONS.to_string(),
                cache_control: true,
            },
        ],
        tools,
        messages: vec![Message {
            role: Role::User,
            content: vec![Block::Text {
                text: context_text,
                cache_control: false,
            }],
        }],
        options: AgentOptions {
            model: model.clone(),
            max_tokens: 4096,
            temperature: Some(0.5),
            top_p: None,
            thinking: channel.thinking_config(),
            effort: channel.default_effort,
            max_turns: cfg.agent.max_turns_per_run.min(10),
            stop_sequences: vec![],
            tool_timeout_secs: Some(cfg.agent.tool_timeout_secs),
        },
        budget: ContextBudget {
            soft_limit_tokens: cfg.agent.context_soft_limit_tokens,
            hard_limit_tokens: cfg.agent.context_hard_limit_tokens,
            compact_keep_last_n: cfg.agent.compact_keep_last_n_turns,
            max_search_calls: cfg.agent.max_search_calls_per_run,
        },
        trigger_message_id: None,
        pipeline: PipelineKind::Chat,
    };

    let run_id = uuid::Uuid::new_v4().to_string();
    let trigger_ref = format!("scan:{}|{}|{}", tick_label, code.as_str(), tick_id);
    observer::start_episode(
        &app,
        &run_id,
        trigger_kind,
        Some(&trigger_ref),
        channel.wire_format.as_str(),
        &model,
        None,
        None,
    )?;
    let provider = build_provider_for_channel(&channel)
        .map_err(|e| format!("构建 provider 失败：{e}"))?;

    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let collector = tokio::spawn(async move {
        let mut answer = String::new();
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::TextDelta { delta, .. } = &ev {
                answer.push_str(delta);
            }
        }
        answer
    });

    let ctx = ToolContext {
        run_id: run_id.clone(),
    };
    let summary_result = run_agent(provider, None, registry.clone(), req, ctx, tx).await;

    let answer = collector
        .await
        .map_err(|e| format!("collector join 失败：{e}"))?;

    match summary_result {
        Ok(summary) => {
            let outcome = answer.chars().take(500).collect::<String>();
            let _ = observer::finalize_with_context(&app, &summary, None, None, Some(&outcome));
            Ok(run_id)
        }
        Err(e) => {
            let err_msg = e.to_string();
            let _ = observer::finalize_failure(&app, &run_id, &err_msg);
            Err(err_msg)
        }
    }
}

// ====== Prompt 构造 =====================================================

const MINI_SCAN_INSTRUCTIONS: &str = r#"
# Mini-scan 模式

你被一个自动 tick 唤醒，看到某只自选股触发了一组信号。你的任务：

1. 检查当前该股是否已有 active expectation——
   - 有 → 根据新信号决定 update_expectation / cancel_expectation / no_action
   - 无 → 根据 strategies 列表决定是否 create_expectation
2. **不要无脑建仓**——如果信号汇合但 strategy 命中率历史很差 / regime 不匹配 / 风险太大 → no_action 是合理选择
3. 必要时调 analyze_chart 看图佐证形态
4. 决策完毕给一句话 outcome（最多 500 字符，会落 agent_episodes.outcome_summary）

禁忌：
- 不允许在 mini-scan 里凭空开仓——必须先 create_expectation
- 不允许 reasoning 字段写"市场情绪"等无法验证表达
"#;

fn build_mini_scan_context(app: &AppHandle, code: &StockCode, signals: &[SignalKind]) -> String {
    let mut s = String::with_capacity(2048);
    s.push_str(&format!("# Mini-scan: {}\n\n", code.as_str()));

    // 当前 quote
    if let Some(q) = market_snapshot::get(&to_ts_code(code)) {
        s.push_str(&format!(
            "当前快照：price={:?} change_pct={:?} 成交额={:?}\n",
            q.price.map(|y| y.value()),
            q.change_percent,
            q.day_amount.map(|y| y.value()),
        ));
    }

    s.push_str("\n## 触发信号\n");
    for sig in signals {
        s.push_str(&format!("- {}\n", sig.family_str()));
    }

    // 现有 active expectations
    if let Ok(actives) = crate::infrastructure::account::expectation_repo::list_pending_for_code(app, code) {
        s.push_str(&format!("\n## 当前 active expectations（{}条）\n", actives.len()));
        for e in actives.iter().take(3) {
            s.push_str(&format!(
                "- id={} direction={} target={:?} horizon={}d reasoning={}\n",
                e.id.as_str(),
                e.direction.as_str(),
                e.target_price.as_ref().map(|y| y.value()),
                e.horizon_days,
                truncate(&e.reasoning, 100),
            ));
        }
    }

    // 现有 strategies
    if let Ok(strats) = crate::infrastructure::agent::strategy_repo::list_enabled(app) {
        s.push_str(&format!("\n## 可用 strategies（{}条）\n", strats.len()));
        for st in strats.iter().take(5) {
            let conf = st.confidence().map(|c| format!("{:.0}%", c * 100.0)).unwrap_or_else(|| "样本不足".into());
            s.push_str(&format!(
                "- {} (hit/applied={}/{}, confidence={}): {}\n",
                st.name, st.hit_count, st.applied_count, conf, st.description,
            ));
        }
    }

    s.push_str("\n---\n请按 Mini-scan 模式决定建/调/撤 expectation 或 no_action。");
    s
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ====== K 线行转 domain KlinePoint ======================================

fn rows_to_kline_points(rows: &[kline_cache::KlineRow]) -> Vec<KlinePoint> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let date_int = r.date.parse::<i32>().ok().unwrap_or(0);
        out.push(KlinePoint {
            date: TradeDate::from_unchecked(date_int),
            open: Yuan::from_unchecked(r.open),
            close: Yuan::from_unchecked(r.close),
            high: Yuan::from_unchecked(r.high),
            low: Yuan::from_unchecked(r.low),
            volume: Lots::from_unchecked(r.volume.unwrap_or(0.0) as i64),
            amount: Yuan::from_unchecked(r.amount.unwrap_or(0.0)),
        });
    }
    // 升序——signal_detector 依赖
    out.sort_by(|a, b| a.date.value().cmp(&b.date.value()));
    out
}

fn to_ts_code(code: &StockCode) -> String {
    // A 股：00/30 → SZ；其余主板 60/68 → SH；北交所 4/8 → BJ
    let s = code.as_str();
    let suffix = match s.chars().next() {
        Some('0') | Some('3') => "SZ",
        Some('6') => "SH",
        Some('4') | Some('8') => "BJ",
        _ => "SH",
    };
    format!("{}.{}", s, suffix)
}

// 未使用的 Regime 引入要保留以防 Phase 2 集成（regime 注入 mini-scan prompt）
#[allow(dead_code)]
fn _regime_marker(_r: Regime) {}
