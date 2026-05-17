//! 后端流水线——目前只剩 chat（briefing/review 已下线）。
//!
//! chat 流程：
//! 1. 立刻写 user message（emit chat-message-appended，UI 即刻渲染）
//! 2. 读上下文（行情/持仓/记忆/学习/最近消息）
//! 3. 构 AgentRequest（identity+instructions 进 system，上下文+用户输入进 user）
//! 4. 启 agent_run（agent_runs 表先插一行）
//! 5. spawn forwarder：把 AgentEvent 流转发给前端 + 累计文本
//! 6. await run_agent → 拿 RunSummary + 最终文本
//! 7. 写 assistant message + finalize agent_runs
//!
//! Memory 更新由 agent 通过 update_memory / remove_memory 工具自己写。

pub mod account;
pub mod agent;
pub mod chat;
pub mod chat_attachments;
pub mod history;
pub mod kline_warm;
pub mod market_overview;
pub mod market_refresh;
pub mod market_universe;
pub mod news;
pub mod scheduler;
pub mod stocks;

use crate::domain::account::types::{Position, PositionEvent};
use crate::domain::agent::memory::{default_investor_memory, InvestorMemory};
use crate::domain::quotes::{MarketOverview, StockQuote};
use crate::infrastructure::account::PositionRepo;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use tauri::{AppHandle, Emitter};

// ====== 常量 ======

pub const KEY_INVESTOR_MEMORY: &str = "gangzi-terminal.investor-memory";

// 后端 → 前端事件名
pub const EVENT_AGENT_STATUS: &str = "agent-status";
/// 行情拉取状态——只在"有问题"时 emit（成功不打扰）。
pub const EVENT_QUOTES_FETCH_STATUS: &str = "quotes-fetch-status";

// ====== 状态广播 ======

pub fn emit_status(app: &AppHandle, phase: &str, message: &str) {
    let _ = app.emit(
        EVENT_AGENT_STATUS,
        json!({ "phase": phase, "message": message }),
    );
}

// ====== app_state 读写 ======

pub fn read_investor_memory(app: &AppHandle) -> InvestorMemory {
    match crate::infrastructure::app_state::load_app_state_value(app, KEY_INVESTOR_MEMORY) {
        Ok(Some(value)) => serde_json::from_value::<InvestorMemory>(value)
            .unwrap_or_else(|_| default_investor_memory()),
        _ => default_investor_memory(),
    }
}

pub fn save_investor_memory(app: &AppHandle, memory: &InvestorMemory) -> Result<(), String> {
    let value = serde_json::to_value(memory).map_err(|e| format!("memory 序列化失败：{e}"))?;
    crate::infrastructure::app_state::save_app_state_value(app, KEY_INVESTOR_MEMORY, &value)
}

// ====== SQLite 上下文读取 ======

/// 读所有 open 持仓——走 domain repo（含 status==Open 过滤）。
pub fn read_positions(app: &AppHandle) -> Result<Vec<Position>, String> {
    let repo = PositionRepo::new(app.clone());
    repo.list_open().map_err(|e| e.to_string())
}

/// 对 open 持仓查事件链——key 是 PositionId.as_str()，方便 prompt formatter 索引。
pub fn read_position_events_for_open(
    app: &AppHandle,
    positions: &[Position],
) -> HashMap<String, Vec<PositionEvent>> {
    if positions.is_empty() {
        return HashMap::new();
    }
    let repo = PositionRepo::new(app.clone());
    let ids: Vec<crate::domain::account::types::PositionId> =
        positions.iter().map(|p| p.id.clone()).collect();
    let events = match repo.list_events_batch(&ids) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    let mut map: HashMap<String, Vec<PositionEvent>> = HashMap::new();
    for event in events {
        map.entry(event.position_id.as_str().to_string())
            .or_default()
            .push(event);
    }
    map
}

// ====== 行情/市场实时拉取 ======

pub async fn fetch_market_overview(app: &AppHandle) -> Option<MarketOverview> {
    market_overview::fetch_market_overview(app).await.ok()
}

/// 行情拉取的完整状态——拿到的 quotes + 失败可见信息。
#[derive(Debug, Clone, Default)]
pub struct QuotesFetchResult {
    pub quotes: Vec<StockQuote>,
    pub requested: Vec<String>,
    pub missing: Vec<String>,
    pub provider_error: Option<String>,
}

impl QuotesFetchResult {
    pub fn from_partial(quotes: Vec<StockQuote>, requested: Vec<String>) -> Self {
        let returned: HashSet<String> =
            quotes.iter().map(|q| q.code.as_str().to_string()).collect();
        let missing: Vec<String> = requested
            .iter()
            .filter(|c| !returned.contains(*c))
            .cloned()
            .collect();
        Self {
            quotes,
            requested,
            missing,
            provider_error: None,
        }
    }

    pub fn has_any_issue(&self) -> bool {
        self.provider_error.is_some() || !self.missing.is_empty()
    }

    pub fn to_prompt_section(&self) -> Option<String> {
        if !self.has_any_issue() {
            return None;
        }
        if let Some(err) = &self.provider_error {
            let err_short: String = err.chars().take(160).collect();
            return Some(format!(
                "🔴 行情接口异常\n- 错误：{}\n- 请求 {} 只均未拿到实时数据\n- 后续分析请避免依赖盘中价格；可用昨收 / 历史 K 线 / 涨停池 / 公告等离线信息判断",
                err_short,
                self.requested.len()
            ));
        }
        let missing_preview: String = self
            .missing
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join("、");
        let suffix = if self.missing.len() > 8 {
            format!("（共 {} 只缺数据）", self.missing.len())
        } else {
            String::new()
        };
        Some(format!(
            "⚠️ 行情数据部分缺失\n- 请求 {} 只，拿到 {} 只\n- 缺数据：{}{}\n- 这些代码可能停牌或接口未返回；分析时请明示，不要凭推断填具体价格",
            self.requested.len(),
            self.quotes.len(),
            missing_preview,
            suffix
        ))
    }
}

/// 拉行情 + 自动 emit 失败可见事件——pipeline 的统一入口。
pub async fn fetch_quotes_with_visibility(
    app: &AppHandle,
    stage: &str,
    codes: Vec<String>,
) -> QuotesFetchResult {
    let result = if codes.is_empty() {
        QuotesFetchResult::default()
    } else {
        use crate::infrastructure::quotes::snapshot::market_snapshot;
        let mut quotes: Vec<StockQuote> = Vec::with_capacity(codes.len());
        for code in &codes {
            if let Some(ts) = crate::infrastructure::quotes::repository::resolve_stock_ts_code(app, code) {
                if let Some(q) = market_snapshot::get(&ts) {
                    quotes.push(q);
                }
            }
        }
        QuotesFetchResult::from_partial(quotes, codes)
    };
    if result.has_any_issue() {
        let _ = app.emit(
            EVENT_QUOTES_FETCH_STATUS,
            json!({
                "stage": stage,
                "requested": result.requested.len(),
                "ok": result.quotes.len(),
                "missing": result.missing,
                "providerError": result.provider_error,
            }),
        );
    }
    result
}

/// 把 watchlist + open positions 的 code 合并，去重后返回
pub fn collect_relevant_codes(watchlist: &[String], positions: &[Position]) -> Vec<String> {
    let mut set: HashSet<String> = watchlist.iter().cloned().collect();
    for p in positions {
        if p.status.is_open() {
            set.insert(p.code.as_str().to_string());
        }
    }
    set.into_iter().collect()
}

// ====== 通用 helpers ======

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
