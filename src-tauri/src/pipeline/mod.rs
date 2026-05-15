//! 后端 Agent 流水线——briefing / review / chat 全在这里跑。
//!
//! 每个流水线：
//! 1. 从 SQLite + 行情接口读上下文
//! 2. 用 `prompt::build_*_prompt` 构造 user 文本（identity 由 system block 提供）
//! 3. 调 agent loop（`pipeline::runner::run_agent_text` 或 `pipeline::chat` 内联）
//! 4. chat 直接拿 TextDelta 拼最终文本；briefing/review 用 `prompt::parse_*` 解析 JSON
//! 5. 落盘（chat_messages / analysis_records / simulated_positions / position_events / app_state）
//! 6. emit Tauri event 通知前端 refetch + emit AgentEvent 给前端流式 UI

pub mod account;
pub mod briefing;
pub mod chat;
pub mod history;
pub mod kline_warm;
pub mod market_overview;
pub mod market_refresh;
pub mod market_universe;

pub mod refresh;
pub mod review;
pub mod runner;
pub mod stocks;

use crate::agent_io::{InvestorMemory, PositionEvent, SimulatedPosition, StoredAnalysisRecord};
use crate::db;
use crate::domain::quotes::{MarketOverview, StockQuote};
use crate::memory::default_investor_memory;
use crate::prompt::RecentBriefing;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tauri::{AppHandle, Emitter};

// ====== 常量 ======

pub const SIMULATION_INITIAL_CASH: f64 = 20000.0;

pub const KEY_INVESTOR_MEMORY: &str = "gangzi-terminal.investor-memory";
pub const KEY_LAST_BRIEFING_AT: &str = "gangzi-terminal.last-briefing-at";

// 后端 → 前端事件名
pub const EVENT_AGENT_STATUS: &str = "agent-status";
pub const EVENT_BRIEFING_PUBLISHED: &str = "briefing-published";
pub const EVENT_REVIEW_PUBLISHED: &str = "review-published";
pub const EVENT_POSITIONS_CHANGED: &str = "positions-changed";
/// 行情拉取状态——只在"有问题"时 emit（成功不打扰）。
/// payload: { stage, requested, ok, missing: string[], providerError: string|null }
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
    match db::load_app_state_value(app, KEY_INVESTOR_MEMORY) {
        Ok(Some(value)) => serde_json::from_value::<InvestorMemory>(value)
            .unwrap_or_else(|_| default_investor_memory()),
        _ => default_investor_memory(),
    }
}

pub fn save_investor_memory(app: &AppHandle, memory: &InvestorMemory) -> Result<(), String> {
    let value = serde_json::to_value(memory).map_err(|e| format!("memory 序列化失败：{e}"))?;
    db::save_app_state_value(app, KEY_INVESTOR_MEMORY, &value)
}

pub fn save_last_briefing_at(app: &AppHandle, ts_millis: i64) -> Result<(), String> {
    db::save_app_state_value(app, KEY_LAST_BRIEFING_AT, &json!(ts_millis))
}

// ====== SQLite 上下文读取 ======

pub fn read_positions(app: &AppHandle) -> Result<Vec<SimulatedPosition>, String> {
    let values = db::list_simulated_positions(app.clone())?;
    Ok(values
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect())
}

pub fn read_position_events_for_open(
    app: &AppHandle,
    positions: &[SimulatedPosition],
) -> HashMap<String, Vec<PositionEvent>> {
    let open_ids: Vec<String> = positions
        .iter()
        .filter(|p| p.status == "open")
        .map(|p| p.id.clone())
        .collect();
    if open_ids.is_empty() {
        return HashMap::new();
    }
    let values = match db::list_position_events_batch(app.clone(), open_ids) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    let mut map: HashMap<String, Vec<PositionEvent>> = HashMap::new();
    for v in values {
        if let Ok(event) = serde_json::from_value::<PositionEvent>(v) {
            map.entry(event.position_id.clone())
                .or_default()
                .push(event);
        }
    }
    map
}

pub fn read_recent_records(
    app: &AppHandle,
    limit: i64,
) -> Result<Vec<StoredAnalysisRecord>, String> {
    let values = db::list_analysis_records(app.clone(), Some(limit))?;
    Ok(values
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect())
}

pub fn read_recent_briefings(app: &AppHandle, take: usize) -> Vec<RecentBriefing> {
    let raw_msgs = db::list_chat_messages(app.clone(), None, Some(30)).unwrap_or_default();
    raw_msgs
        .into_iter()
        .filter(|v| v.get("kind").and_then(Value::as_str) == Some("briefing"))
        .take(take)
        .filter_map(|v| {
            Some(RecentBriefing {
                created_at: v.get("createdAt").and_then(Value::as_str)?.to_string(),
                summary_md: v.get("contentMd").and_then(Value::as_str)?.to_string(),
            })
        })
        .collect()
}

// ====== 行情/市场实时拉取 ======

pub async fn fetch_market_overview(app: &AppHandle) -> Option<MarketOverview> {
    market_overview::fetch_market_overview(app).await.ok()
}

/// 行情拉取的完整状态——拿到的 quotes + 失败可见信息。
///
/// 三种状态：
/// - 成功：quotes.len() == requested.len()，missing 空，provider_error 空
/// - 部分失败：某些 code 在所有源都没拿到数据（停牌 / 接口 partial）
/// - 全失败：三源全挂，provider_error 有内容
///
/// pipeline 调用方根据这个区分行为：refresh 拒绝用"假"价格强平，prompt
/// 把降级信息告诉 agent，前端 toast/badge 提示用户。
#[derive(Debug, Clone, Default)]
pub struct QuotesFetchResult {
    pub quotes: Vec<StockQuote>,
    pub requested: Vec<String>,
    pub missing: Vec<String>,
    pub provider_error: Option<String>,
}

impl QuotesFetchResult {
    /// quotes 拿到部分（或全部）——missing = requested - returned。
    /// requested 必须是原始请求 codes（顺序无所谓），用来算 missing。
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

    /// 三源全失败——missing = requested 全集，provider_error 给底层错误信息。
    pub fn from_error(requested: Vec<String>, err: String) -> Self {
        Self {
            quotes: Vec::new(),
            missing: requested.clone(),
            requested,
            provider_error: Some(err),
        }
    }

    /// 全失败：接口报错 + 一条都没拿到。
    /// refresh 看到这个会拒绝跑 stop_loss/take_profit/time_stop 触发。
    pub fn is_full_failure(&self) -> bool {
        self.provider_error.is_some() && self.quotes.is_empty()
    }

    /// 任何问题——接口失败 or 部分 code 缺数据。决定要不要 emit 事件 / 注入 prompt。
    pub fn has_any_issue(&self) -> bool {
        self.provider_error.is_some() || !self.missing.is_empty()
    }

    /// 给 agent prompt 用的"数据可用性"提示文本——成功时返回 None。
    /// 注入到 prompt 后，agent 看到接口异常会自动降级表达（避免输出依赖盘中价格的开仓建议）。
    pub fn to_prompt_section(&self) -> Option<String> {
        if !self.has_any_issue() {
            return None;
        }
        if let Some(err) = &self.provider_error {
            // 错误信息可能很长（含 URL / 堆栈）——截一下避免污染 prompt。
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
///
/// 三件事打包：
/// 1. 调用 quotes 三级 fallback 拿数据
/// 2. 把结果包成 `QuotesFetchResult`，区分全失败/部分失败/全成功
/// 3. 有问题时 emit `quotes-fetch-status` 给前端 UI；成功不打扰
///
/// `stage` 取 "chat" / "briefing" / "review" / "refresh"，前端按场景渲染。
pub async fn fetch_quotes_with_visibility(
    app: &AppHandle,
    stage: &str,
    codes: Vec<String>,
) -> QuotesFetchResult {
    let result = if codes.is_empty() {
        QuotesFetchResult::default()
    } else {
        // 走 MARKET_SNAPSHOT（scheduler 维护）——不再单独 fetch EM
        use crate::infrastructure::quotes::snapshot::market_snapshot;
        let mut quotes: Vec<StockQuote> = Vec::with_capacity(codes.len());
        for code in &codes {
            if let Some(ts) = crate::db::resolve_stock_ts_code(app, code) {
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
pub fn collect_relevant_codes(
    watchlist: &[String],
    positions: &[SimulatedPosition],
) -> Vec<String> {
    let mut set: HashSet<String> = watchlist.iter().cloned().collect();
    for p in positions {
        if p.status == "open" {
            set.insert(p.code.clone());
        }
    }
    set.into_iter().collect()
}

// ====== 通用 helpers ======

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 模拟交易的时间止损绝对时间——entry_at 加 7 个日历日。
/// prompt 里 timeStop 的语义是"3-5 个交易日仍未验证则复盘退出"——A 股大约 5 个交易日 = 7 日历日。
/// 用日历日避免后端要查节假日表；过头一两天对训练目的影响微弱，且训练用户在事件链上一眼能看到 reason=time_stop。
pub fn derive_time_stop_at(entry_at: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(entry_at)
        .ok()
        .and_then(|d| d.checked_add_signed(chrono::Duration::days(7)))
        .map(|d| d.to_rfc3339())
}

// ====== 仓位平仓的旧流水线投射 helper ======
//
// review 仍在迁移中：它会先用旧 `SimulatedPosition` 组装 review commit，再由
// db::commit_review 统一事务落盘。真正的 account 写入口已经迁到
// `pipeline::account::AccountService`。

#[derive(Debug, Clone)]
pub struct PositionClose {
    pub position_id: String,
    pub reason: String, // stop_loss / take_profit / time_stop / invalidated / manual_reset
    pub exit_price: f64,
    /// closed 事件的 sourceKind——system / review / manual
    pub source_kind: String,
    /// closed 事件的 sourceRef（例如 review 里的 record_id），可空
    pub source_ref: Option<String>,
    pub agent_note_md: String,
}

/// 纯函数版本：把 closes 应用到 positions 列表上，不做 I/O。
/// 抽出来是为了单元测试——验证状态翻转语义而不需要 AppHandle。
pub(crate) fn apply_closes_in_memory(
    all_positions: Vec<SimulatedPosition>,
    closes: &[PositionClose],
    now: &str,
) -> Vec<SimulatedPosition> {
    let close_map: HashMap<&String, &PositionClose> =
        closes.iter().map(|c| (&c.position_id, c)).collect();
    all_positions
        .into_iter()
        .map(|mut p| {
            if let Some(c) = close_map.get(&p.id) {
                p.status = "closed".into();
                p.exit_at = Some(now.to_string());
                p.exit_price = Some(c.exit_price);
                p.close_reason = Some(c.reason.clone());
            }
            p
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_position(id: &str, code: &str, status: &str) -> SimulatedPosition {
        SimulatedPosition {
            id: id.into(),
            code: code.into(),
            name: "Test".into(),
            entry_price: 10.0,
            shares: 100,
            entry_at: "2026-05-01T00:00:00Z".into(),
            exit_price: None,
            exit_at: None,
            close_reason: None,
            thesis: "test thesis".into(),
            stop_loss: Some(9.0),
            take_profit: Some(12.0),
            time_stop_at: Some("2026-05-08T00:00:00Z".into()),
            source_analysis_id: "rec-1".into(),
            status: status.into(),
            original_shares: Some(100),
            current_shares: Some(100),
            avg_entry_price: Some(10.0),
        }
    }

    fn make_close(id: &str, reason: &str, exit_price: f64) -> PositionClose {
        PositionClose {
            position_id: id.into(),
            reason: reason.into(),
            exit_price,
            source_kind: "system".into(),
            source_ref: None,
            agent_note_md: format!("test {reason}"),
        }
    }

    #[test]
    fn apply_closes_marks_only_targeted_positions() {
        let positions = vec![
            make_position("a", "600519", "open"),
            make_position("b", "300750", "open"),
            make_position("c", "000001", "open"),
        ];
        let closes = vec![make_close("a", "stop_loss", 8.5)];
        let updated = apply_closes_in_memory(positions, &closes, "2026-05-08T10:00:00Z");

        let a = updated.iter().find(|p| p.id == "a").unwrap();
        assert_eq!(a.status, "closed");
        assert_eq!(a.exit_price, Some(8.5));
        assert_eq!(a.close_reason.as_deref(), Some("stop_loss"));
        assert_eq!(a.exit_at.as_deref(), Some("2026-05-08T10:00:00Z"));

        let b = updated.iter().find(|p| p.id == "b").unwrap();
        assert_eq!(b.status, "open");
        assert!(b.exit_price.is_none());

        let c = updated.iter().find(|p| p.id == "c").unwrap();
        assert_eq!(c.status, "open");
    }

    #[test]
    fn apply_closes_supports_each_reason_kind() {
        let positions = vec![
            make_position("sl", "1", "open"),
            make_position("tp", "2", "open"),
            make_position("ts", "3", "open"),
            make_position("inv", "4", "open"),
            make_position("mr", "5", "open"),
        ];
        let closes = vec![
            make_close("sl", "stop_loss", 8.0),
            make_close("tp", "take_profit", 12.5),
            make_close("ts", "time_stop", 10.1),
            make_close("inv", "invalidated", 9.9),
            make_close("mr", "manual_reset", 10.0),
        ];
        let updated = apply_closes_in_memory(positions, &closes, "2026-05-08T10:00:00Z");
        for p in &updated {
            assert_eq!(p.status, "closed", "{} should be closed", p.id);
            assert!(
                p.close_reason.is_some(),
                "{} should have close_reason",
                p.id
            );
        }
        assert_eq!(
            updated
                .iter()
                .find(|p| p.id == "sl")
                .unwrap()
                .close_reason
                .as_deref(),
            Some("stop_loss")
        );
        assert_eq!(
            updated
                .iter()
                .find(|p| p.id == "tp")
                .unwrap()
                .close_reason
                .as_deref(),
            Some("take_profit")
        );
        assert_eq!(
            updated
                .iter()
                .find(|p| p.id == "ts")
                .unwrap()
                .close_reason
                .as_deref(),
            Some("time_stop")
        );
        assert_eq!(
            updated
                .iter()
                .find(|p| p.id == "inv")
                .unwrap()
                .close_reason
                .as_deref(),
            Some("invalidated")
        );
        assert_eq!(
            updated
                .iter()
                .find(|p| p.id == "mr")
                .unwrap()
                .close_reason
                .as_deref(),
            Some("manual_reset")
        );
    }

    #[test]
    fn apply_closes_empty_closes_returns_unchanged() {
        let positions = vec![make_position("a", "600519", "open")];
        let updated = apply_closes_in_memory(positions.clone(), &[], "2026-05-08T10:00:00Z");
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].status, "open");
        assert!(updated[0].exit_price.is_none());
    }

    #[test]
    fn derive_time_stop_at_adds_seven_days() {
        let entry = "2026-05-01T00:00:00+00:00";
        let stop = derive_time_stop_at(entry).unwrap();
        // 解析回去验证差了 7 天
        let entry_t = chrono::DateTime::parse_from_rfc3339(entry).unwrap();
        let stop_t = chrono::DateTime::parse_from_rfc3339(&stop).unwrap();
        let diff = stop_t.signed_duration_since(entry_t);
        assert_eq!(diff.num_days(), 7);
    }

    #[test]
    fn derive_time_stop_at_returns_none_for_garbage() {
        assert!(derive_time_stop_at("not an iso timestamp").is_none());
    }
}
