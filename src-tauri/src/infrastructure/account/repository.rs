//! Position + PositionEvent 的 SQLite 读写。
//!
//! 两层关注点：
//! - **Raw JSON layer**（文件末尾）：`list_simulated_positions` / `commit_account_positions` /
//!   `append_position_event` / `list_position_events_batch` —— Tauri IPC 直读形态。
//! - **Domain layer**（PositionRepo struct）：domain 富类型 ↔ DB 扁平行的投射层。
//!   PositionRepo 内部调 raw-JSON layer 完成 DB IO。

use crate::domain::account::errors::AccountError;
use crate::domain::account::types::{
    CloseReason, EventSource, Position, PositionEvent, PositionEventKind, PositionId,
    PositionSignalKind, PositionStatus,
};
use crate::domain::shared::{OccurredAt, Shares, StockCode, Yuan};
use crate::infrastructure::db::{
    json_string, list_json_payloads, migrate, now, open_database, required_json_string,
};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DbPosition {
    id: String,
    code: String,
    name: String,
    entry_price: f64,
    shares: i64,
    entry_at: String,
    exit_price: Option<f64>,
    exit_at: Option<String>,
    close_reason: Option<String>,
    thesis: String,
    stop_loss: Option<f64>,
    take_profit: Option<f64>,
    #[serde(default)]
    time_stop_at: Option<String>,
    source_analysis_id: String,
    status: String,
    #[serde(default)]
    original_shares: Option<i64>,
    #[serde(default)]
    current_shares: Option<i64>,
    #[serde(default)]
    avg_entry_price: Option<f64>,
    /// 最近一次买入时间——T+1 判定基准。老数据缺这个字段时回退到 `entry_at`。
    #[serde(default)]
    last_acquisition_at: Option<String>,
}

impl DbPosition {
    fn current_shares(&self) -> i64 {
        self.current_shares.unwrap_or(self.shares)
    }

    fn avg_entry_price(&self) -> f64 {
        self.avg_entry_price.unwrap_or(self.entry_price)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DbPositionEvent {
    id: String,
    position_id: String,
    event_kind: String,
    occurred_at: String,
    source_kind: Option<String>,
    source_ref: Option<String>,
    payload: Option<Value>,
    agent_note_md: Option<String>,
}

pub struct PositionRepo {
    app: AppHandle,
}

impl PositionRepo {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }

    // ========================================================================
    // Positions
    // ========================================================================

    /// 读所有 positions（open + closed）。
    pub fn list_all(&self) -> Result<Vec<Position>, AccountError> {
        let raw = list_simulated_positions(self.app.clone()).map_err(AccountError::Io)?;
        let mut out = Vec::with_capacity(raw.len());
        for v in raw {
            let db_position: DbPosition = serde_json::from_value(v)
                .map_err(|e| AccountError::Io(format!("position 反序列化失败: {e}")))?;
            out.push(db_position_to_domain(db_position)?);
        }
        Ok(out)
    }

    /// 读所有 open positions。
    pub fn list_open(&self) -> Result<Vec<Position>, AccountError> {
        let all = self.list_all()?;
        Ok(all.into_iter().filter(|p| p.status.is_open()).collect())
    }

    /// 按 id 找单条。
    pub fn find(&self, id: &PositionId) -> Result<Option<Position>, AccountError> {
        let all = self.list_all()?;
        Ok(all.into_iter().find(|p| p.id == *id))
    }

    /// 原子提交：append 一条 event + 整列替换 positions。
    ///
    /// AccountService 写路径统一走这里，避免 event/state 撕裂。
    pub fn commit_event_and_positions(
        &self,
        event: &PositionEvent,
        positions: &[Position],
    ) -> Result<(), AccountError> {
        let rows = positions_to_db_rows(positions)?;
        commit_account_positions(self.app.clone(), Some(event_to_db_json(event)), rows, false)
            .map_err(AccountError::Io)?;
        Ok(())
    }

    /// 清空账户状态 + 事件链。用于 reset 重新训练。
    pub fn clear_all(&self) -> Result<(), AccountError> {
        commit_account_positions(self.app.clone(), None, Vec::new(), true)
            .map_err(AccountError::Io)?;
        Ok(())
    }
}

fn positions_to_db_rows(positions: &[Position]) -> Result<Vec<Value>, AccountError> {
    positions
        .iter()
        .map(|p| {
            serde_json::to_value(domain_to_db_position(p))
                .map_err(|e| AccountError::Io(format!("position 序列化失败: {e}")))
        })
        .collect()
}

impl PositionRepo {
    // ========================================================================
    // Events
    // ========================================================================

    /// 读单个 position 的事件链（按 occurred_at 升序）。
    pub fn list_events(
        &self,
        position_id: &PositionId,
    ) -> Result<Vec<PositionEvent>, AccountError> {
        self.list_events_batch(&[position_id.clone()])
    }

    /// 批量读事件——多 position 一次性查（valuation 用）。
    pub fn list_events_batch(
        &self,
        ids: &[PositionId],
    ) -> Result<Vec<PositionEvent>, AccountError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let id_strings: Vec<String> = ids.iter().map(|id| id.as_str().to_string()).collect();
        let raw =
            list_position_events_batch(self.app.clone(), id_strings).map_err(AccountError::Io)?;
        let mut out = Vec::with_capacity(raw.len());
        for v in raw {
            out.push(db_json_to_event(v)?);
        }
        out.sort_by_key(|e| e.occurred_at.value());
        Ok(out)
    }
}

// ============================================================================
// Position 投射：legacy SimulatedPosition ↔ domain Position
// ============================================================================

fn db_position_to_domain(row: DbPosition) -> Result<Position, AccountError> {
    let code = StockCode::new(&row.code)
        .map_err(|e| AccountError::Io(format!("非法 code {}: {e}", row.code)))?;

    let avg_value = row.avg_entry_price();
    let avg_entry_price =
        Yuan::new(avg_value).map_err(|e| AccountError::Io(format!("非法均价 {avg_value}: {e}")))?;

    // current_shares 不强校验整百——DB 里可能有历史违规值
    let current_shares = Shares::from_unchecked(row.current_shares());

    let entered_at = parse_rfc3339(&row.entry_at);
    // 老 schema 没 last_acquisition_at——回退到 entered_at（首次开仓即首次买入）
    let last_acquisition_at = row
        .last_acquisition_at
        .as_deref()
        .map(parse_rfc3339)
        .unwrap_or(entered_at);

    let status = match row.status.as_str() {
        "open" => PositionStatus::Open,
        "closed" => {
            let exit_price = Yuan::new(row.exit_price.unwrap_or(avg_value))
                .map_err(|e| AccountError::Io(format!("非法 exit_price: {e}")))?;
            let exit_at = row
                .exit_at
                .as_deref()
                .map(parse_rfc3339)
                .unwrap_or(entered_at);
            let reason = parse_close_reason(row.close_reason.as_deref().unwrap_or("manual"));
            PositionStatus::Closed {
                exit_price,
                exit_at,
                reason,
            }
        }
        other => {
            return Err(AccountError::Io(format!("未知 status: {other}")));
        }
    };

    Ok(Position {
        id: PositionId::from_string(row.id),
        code,
        name: row.name,
        avg_entry_price,
        current_shares,
        status,
        stop_loss: row.stop_loss.and_then(|v| Yuan::new(v).ok()),
        take_profit: row.take_profit.and_then(|v| Yuan::new(v).ok()),
        time_stop_at: row.time_stop_at.as_deref().map(parse_rfc3339),
        thesis: row.thesis,
        source_analysis_id: row.source_analysis_id,
        entered_at,
        last_acquisition_at,
    })
}

fn domain_to_db_position(p: &Position) -> DbPosition {
    let (status_str, exit_price, exit_at, close_reason) = match &p.status {
        PositionStatus::Open => ("open".to_string(), None, None, None),
        PositionStatus::Closed {
            exit_price,
            exit_at,
            reason,
        } => (
            "closed".to_string(),
            Some(exit_price.value()),
            Some(occurred_at_to_rfc3339(*exit_at)),
            Some(reason.as_str().to_string()),
        ),
    };

    DbPosition {
        id: p.id.as_str().to_string(),
        code: p.code.as_str().to_string(),
        name: p.name.clone(),
        entry_price: p.avg_entry_price.value(),
        shares: p.current_shares.value(),
        entry_at: occurred_at_to_rfc3339(p.entered_at),
        exit_price,
        exit_at,
        close_reason,
        thesis: p.thesis.clone(),
        stop_loss: p.stop_loss.map(|y| y.value()),
        take_profit: p.take_profit.map(|y| y.value()),
        time_stop_at: p.time_stop_at.map(occurred_at_to_rfc3339),
        source_analysis_id: p.source_analysis_id.clone(),
        status: status_str,
        // original_shares 暂存当前股数；要"真正首次股数"需查事件链
        original_shares: Some(p.current_shares.value()),
        current_shares: Some(p.current_shares.value()),
        avg_entry_price: Some(p.avg_entry_price.value()),
        last_acquisition_at: Some(occurred_at_to_rfc3339(p.last_acquisition_at)),
    }
}

fn parse_close_reason(s: &str) -> CloseReason {
    match s {
        "stop_loss" => CloseReason::StopLoss,
        "take_profit" => CloseReason::TakeProfit,
        "time_stop" => CloseReason::TimeStop,
        "invalidated" => CloseReason::Invalidated,
        _ => CloseReason::Manual,
    }
}

// ============================================================================
// Event 投射：legacy PositionEvent ↔ domain PositionEvent
// ============================================================================

fn event_to_db_json(event: &PositionEvent) -> Value {
    let (event_kind_tag, payload) = kind_to_tag_and_payload(&event.kind);
    let (source_kind, source_ref) = source_to_kind_and_ref(&event.source);

    json!({
        "id": event.id,
        "positionId": event.position_id.as_str(),
        "eventKind": event_kind_tag,
        "occurredAt": occurred_at_to_rfc3339(event.occurred_at),
        "sourceKind": source_kind,
        "sourceRef": source_ref,
        "payload": payload,
        "agentNoteMd": event.agent_note_md,
    })
}

fn db_json_to_event(v: Value) -> Result<PositionEvent, AccountError> {
    let raw: DbPositionEvent = serde_json::from_value(v)
        .map_err(|e| AccountError::Io(format!("event 反序列化失败: {e}")))?;
    let kind = tag_and_payload_to_kind(&raw.event_kind, raw.payload.as_ref())?;
    let source = kind_and_ref_to_source(raw.source_kind.as_deref(), raw.source_ref.as_deref());
    Ok(PositionEvent {
        id: raw.id,
        position_id: PositionId::from_string(raw.position_id),
        kind,
        occurred_at: parse_rfc3339(&raw.occurred_at),
        source,
        agent_note_md: raw.agent_note_md.unwrap_or_default(),
    })
}

fn kind_to_tag_and_payload(kind: &PositionEventKind) -> (&'static str, Value) {
    match kind {
        PositionEventKind::Opened {
            entry_price,
            shares,
            commission,
        } => (
            "opened",
            json!({
                "entryPrice": entry_price.value(),
                "shares": shares.value(),
                "commission": commission.value(),
            }),
        ),
        PositionEventKind::ScaledIn {
            delta,
            price,
            new_avg,
            commission,
        } => (
            "scaled_in",
            json!({
                "sharesDelta": delta.value(),
                "price": price.value(),
                "newAvgEntryPrice": new_avg.value(),
                "commission": commission.value(),
            }),
        ),
        PositionEventKind::ScaledOut {
            delta,
            price,
            commission,
            stamp_tax,
        } => (
            "scaled_out",
            json!({
                "sharesDelta": -(delta.value()), // 负数表示减仓（保持向旧 payload 兼容语义）
                "price": price.value(),
                "commission": commission.value(),
                "stampTax": stamp_tax.value(),
            }),
        ),
        PositionEventKind::Closed {
            exit_price,
            shares,
            reason,
            commission,
            stamp_tax,
        } => (
            "closed",
            json!({
                "exitPrice": exit_price.value(),
                "shares": shares.value(),
                "reason": reason.as_str(),
                "commission": commission.value(),
                "stampTax": stamp_tax.value(),
            }),
        ),
        PositionEventKind::StopsAdjusted {
            stop_loss,
            take_profit,
            time_stop_at,
        } => (
            "stops_adjusted",
            json!({
                "stopLoss": stop_loss.map(|y| y.value()),
                "takeProfit": take_profit.map(|y| y.value()),
                "timeStopAt": time_stop_at.map(occurred_at_to_rfc3339),
            }),
        ),
        PositionEventKind::Reviewed {
            thesis_status,
            confidence,
        } => (
            "reviewed",
            json!({
                "thesisStatus": thesis_status,
                "confidence": confidence,
            }),
        ),
        PositionEventKind::Signal { signal } => (signal.as_str(), json!({})),
    }
}

fn tag_and_payload_to_kind(
    tag: &str,
    payload: Option<&Value>,
) -> Result<PositionEventKind, AccountError> {
    let p = payload.ok_or_else(|| AccountError::Io(format!("event {tag} 缺 payload")))?;
    match tag {
        "opened" => {
            let entry_price = num_field(p, &["entryPrice"]).ok_or_else(missing("entryPrice"))?;
            let shares = int_field(p, &["shares"]).ok_or_else(missing("shares"))?;
            let commission = num_field(p, &["commission"]).unwrap_or(0.0);
            Ok(PositionEventKind::Opened {
                entry_price: Yuan::from_unchecked(entry_price),
                shares: Shares::from_unchecked(shares),
                commission: Yuan::from_unchecked(commission),
            })
        }
        "scaled_in" => {
            let delta = int_field(p, &["sharesDelta"]).ok_or_else(missing("sharesDelta"))?;
            let price = num_field(p, &["price"]).ok_or_else(missing("price"))?;
            let new_avg = num_field(p, &["newAvgEntryPrice"]).unwrap_or(price);
            let commission = num_field(p, &["commission"]).unwrap_or(0.0);
            Ok(PositionEventKind::ScaledIn {
                delta: Shares::from_unchecked(delta.abs()),
                price: Yuan::from_unchecked(price),
                new_avg: Yuan::from_unchecked(new_avg),
                commission: Yuan::from_unchecked(commission),
            })
        }
        "scaled_out" => {
            let delta_signed = int_field(p, &["sharesDelta"]).ok_or_else(missing("sharesDelta"))?;
            let price = num_field(p, &["price"]).ok_or_else(missing("price"))?;
            let commission = num_field(p, &["commission"]).unwrap_or(0.0);
            let stamp_tax = num_field(p, &["stampTax"]).unwrap_or(0.0);
            Ok(PositionEventKind::ScaledOut {
                delta: Shares::from_unchecked(delta_signed.abs()),
                price: Yuan::from_unchecked(price),
                commission: Yuan::from_unchecked(commission),
                stamp_tax: Yuan::from_unchecked(stamp_tax),
            })
        }
        "closed" => {
            let exit_price = num_field(p, &["exitPrice"]).ok_or_else(missing("exitPrice"))?;
            let shares = int_field(p, &["shares"]).unwrap_or(0);
            let reason = p
                .get("reason")
                .and_then(|v| v.as_str())
                .map(parse_close_reason)
                .unwrap_or(CloseReason::Manual);
            let commission = num_field(p, &["commission"]).unwrap_or(0.0);
            let stamp_tax = num_field(p, &["stampTax"]).unwrap_or(0.0);
            Ok(PositionEventKind::Closed {
                exit_price: Yuan::from_unchecked(exit_price),
                shares: Shares::from_unchecked(shares),
                reason,
                commission: Yuan::from_unchecked(commission),
                stamp_tax: Yuan::from_unchecked(stamp_tax),
            })
        }
        "stops_adjusted" => Ok(PositionEventKind::StopsAdjusted {
            stop_loss: num_field(p, &["stopLoss"]).map(Yuan::from_unchecked),
            take_profit: num_field(p, &["takeProfit"]).map(Yuan::from_unchecked),
            time_stop_at: p
                .get("timeStopAt")
                .and_then(|v| v.as_str())
                .map(parse_rfc3339),
        }),
        "reviewed" => Ok(PositionEventKind::Reviewed {
            thesis_status: p
                .get("thesisStatus")
                .and_then(|v| v.as_str())
                .map(String::from),
            confidence: num_field(p, &["confidence"]),
        }),
        "stop_triggered" => Ok(PositionEventKind::Signal {
            signal: PositionSignalKind::StopTriggered,
        }),
        "take_profit_hit" => Ok(PositionEventKind::Signal {
            signal: PositionSignalKind::TakeProfitHit,
        }),
        "time_stop_hit" => Ok(PositionEventKind::Signal {
            signal: PositionSignalKind::TimeStopHit,
        }),
        "invalidated" => Ok(PositionEventKind::Signal {
            signal: PositionSignalKind::Invalidated,
        }),
        other => Err(AccountError::Io(format!("未知 event_kind: {other}"))),
    }
}

fn source_to_kind_and_ref(src: &EventSource) -> (&'static str, Option<String>) {
    match src {
        EventSource::Briefing { analysis_id } => ("briefing", Some(analysis_id.clone())),
        EventSource::Review { analysis_id } => ("review", Some(analysis_id.clone())),
        EventSource::Chat { message_id } => ("chat", Some(message_id.clone())),
        EventSource::Manual => ("manual", None),
        EventSource::System => ("system", None),
    }
}

fn kind_and_ref_to_source(kind: Option<&str>, source_ref: Option<&str>) -> EventSource {
    match kind.unwrap_or("manual") {
        "briefing" => EventSource::Briefing {
            analysis_id: source_ref.unwrap_or_default().to_string(),
        },
        "review" => EventSource::Review {
            analysis_id: source_ref.unwrap_or_default().to_string(),
        },
        "chat" => EventSource::Chat {
            message_id: source_ref.unwrap_or_default().to_string(),
        },
        "system" => EventSource::System,
        _ => EventSource::Manual,
    }
}

// ============================================================================
// 时间投射
// ============================================================================

pub(crate) fn parse_rfc3339(s: &str) -> OccurredAt {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| OccurredAt::new(dt.timestamp_millis()))
        .unwrap_or_else(|_| OccurredAt::now())
}

pub(crate) fn occurred_at_to_rfc3339(ts: OccurredAt) -> String {
    ts.to_rfc3339()
}

// ============================================================================
// 字段查询小工具
// ============================================================================

fn num_field(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| v.get(k).and_then(|x| x.as_f64()))
}

fn int_field(v: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| v.get(k).and_then(|x| x.as_i64()))
}

fn missing(field: &'static str) -> impl FnOnce() -> AccountError {
    move || AccountError::Io(format!("event payload 缺字段: {field}"))
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::account::types::EventSource;

    fn make_position() -> Position {
        Position {
            id: PositionId::from_string("p1".into()),
            code: StockCode::new("600519").unwrap(),
            name: "贵州茅台".into(),
            avg_entry_price: Yuan::new(1789.5).unwrap(),
            current_shares: Shares::new(100).unwrap(),
            status: PositionStatus::Open,
            stop_loss: Some(Yuan::new(1700.0).unwrap()),
            take_profit: Some(Yuan::new(1900.0).unwrap()),
            time_stop_at: None,
            thesis: "技术面突破".into(),
            source_analysis_id: "a1".into(),
            entered_at: OccurredAt::new(1_700_000_000_000),
            last_acquisition_at: OccurredAt::new(1_700_000_000_000),
        }
    }

    #[test]
    fn position_round_trip_open() {
        let p = make_position();
        let row = domain_to_db_position(&p);
        let json_v = serde_json::to_value(&row).unwrap();
        let row_back: DbPosition = serde_json::from_value(json_v).unwrap();
        let back = db_position_to_domain(row_back).unwrap();
        assert_eq!(back.id, p.id);
        assert_eq!(back.code, p.code);
        assert_eq!(back.name, p.name);
        assert!((back.avg_entry_price.value() - p.avg_entry_price.value()).abs() < 1e-6);
        assert_eq!(back.current_shares.value(), p.current_shares.value());
        assert!(back.status.is_open());
    }

    #[test]
    fn position_round_trip_closed() {
        let mut p = make_position();
        p.status = PositionStatus::Closed {
            exit_price: Yuan::new(1820.0).unwrap(),
            exit_at: OccurredAt::new(1_700_000_100_000),
            reason: CloseReason::TakeProfit,
        };
        let row = domain_to_db_position(&p);
        let json_v = serde_json::to_value(&row).unwrap();
        let row_back: DbPosition = serde_json::from_value(json_v).unwrap();
        let back = db_position_to_domain(row_back).unwrap();
        match back.status {
            PositionStatus::Closed {
                exit_price, reason, ..
            } => {
                assert!((exit_price.value() - 1820.0).abs() < 1e-6);
                assert_eq!(reason, CloseReason::TakeProfit);
            }
            _ => panic!("expected Closed"),
        }
    }

    #[test]
    fn event_round_trip_opened() {
        let event = PositionEvent {
            id: "e1".into(),
            position_id: PositionId::from_string("p1".into()),
            kind: PositionEventKind::Opened {
                entry_price: Yuan::new(11.5).unwrap(),
                shares: Shares::new(200).unwrap(),
                commission: Yuan::new(5.0).unwrap(),
            },
            occurred_at: OccurredAt::new(1_700_000_000_000),
            source: EventSource::Briefing {
                analysis_id: "a-abc".into(),
            },
            agent_note_md: "test".into(),
        };
        let json = event_to_db_json(&event);
        let back = db_json_to_event(json).unwrap();
        assert_eq!(back.id, event.id);
        assert_eq!(back.position_id, event.position_id);
        match (back.kind, event.kind) {
            (
                PositionEventKind::Opened {
                    entry_price: ep1,
                    shares: s1,
                    commission: c1,
                },
                PositionEventKind::Opened {
                    entry_price: ep2,
                    shares: s2,
                    commission: c2,
                },
            ) => {
                assert!((ep1.value() - ep2.value()).abs() < 1e-6);
                assert_eq!(s1.value(), s2.value());
                assert!((c1.value() - c2.value()).abs() < 1e-6);
            }
            _ => panic!("kind mismatch"),
        }
        match back.source {
            EventSource::Briefing { analysis_id } => assert_eq!(analysis_id, "a-abc"),
            _ => panic!("source mismatch"),
        }
    }

    #[test]
    fn event_round_trip_closed_with_fees() {
        let event = PositionEvent {
            id: "e2".into(),
            position_id: PositionId::from_string("p1".into()),
            kind: PositionEventKind::Closed {
                exit_price: Yuan::new(12.5).unwrap(),
                shares: Shares::new(200).unwrap(),
                reason: CloseReason::TakeProfit,
                commission: Yuan::new(5.0).unwrap(),
                stamp_tax: Yuan::new(2.5).unwrap(),
            },
            occurred_at: OccurredAt::new(1_700_000_100_000),
            source: EventSource::Manual,
            agent_note_md: String::new(),
        };
        let json = event_to_db_json(&event);
        let back = db_json_to_event(json).unwrap();
        match back.kind {
            PositionEventKind::Closed {
                exit_price,
                reason,
                stamp_tax,
                ..
            } => {
                assert!((exit_price.value() - 12.5).abs() < 1e-6);
                assert_eq!(reason, CloseReason::TakeProfit);
                assert!((stamp_tax.value() - 2.5).abs() < 1e-6);
            }
            _ => panic!("kind mismatch"),
        }
    }

    #[test]
    fn close_reason_parser() {
        assert_eq!(parse_close_reason("stop_loss"), CloseReason::StopLoss);
        assert_eq!(parse_close_reason("take_profit"), CloseReason::TakeProfit);
        assert_eq!(parse_close_reason("time_stop"), CloseReason::TimeStop);
        assert_eq!(parse_close_reason("invalidated"), CloseReason::Invalidated);
        assert_eq!(parse_close_reason("anything_else"), CloseReason::Manual);
        assert_eq!(parse_close_reason(""), CloseReason::Manual);
    }
}

// ============================================================================
// Raw JSON layer——simulated_positions + position_events 表的 fn body
// （Tauri IPC 直读形态；PositionRepo 内部也调这层）
// ============================================================================

pub fn list_simulated_positions(app: AppHandle) -> Result<Vec<Value>, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    list_json_payloads(
        &connection,
        "select payload_json from simulated_positions order by created_at desc limit ?1",
        1000,
        "读取模拟持仓失败",
    )
}

/// Account 写事务：可选 append 一条 position_event，并整列替换 positions。
///
/// `AccountService` 的写路径必须保持 event/state 原子性：不能出现 event 已写但
/// positions 没更新，或 positions 更新但 event 缺失。reset 场景传 `clear_events=true`
/// 清空事件链并替换为空 positions，让账户从干净初始状态重练。
pub fn commit_account_positions(
    app: AppHandle,
    event: Option<Value>,
    positions: Vec<Value>,
    clear_events: bool,
) -> Result<(), String> {
    let mut connection = open_database(&app)?;
    migrate(&connection)?;
    let tx = connection
        .transaction()
        .map_err(|err| format!("提交账户事务失败：{err}"))?;

    if clear_events {
        tx.execute("delete from position_events", [])
            .map_err(|err| format!("清空持仓事件失败：{err}"))?;
    }

    if let Some(event) = event {
        insert_position_event_tx(&tx, &event)?;
    }

    replace_simulated_positions_tx(&tx, positions)?;

    tx.commit()
        .map_err(|err| format!("提交账户事务失败：{err}"))?;
    Ok(())
}

fn replace_simulated_positions_tx(
    tx: &Transaction<'_>,
    positions: Vec<Value>,
) -> Result<(), String> {
    tx.execute("delete from simulated_positions", [])
        .map_err(|err| format!("清理模拟持仓失败：{err}"))?;

    let now = now();
    for position in positions {
        let id = required_json_string(&position, "/id", "模拟持仓缺少 id")?;
        let code = required_json_string(&position, "/code", "模拟持仓缺少 code")?;
        let source_analysis_id = required_json_string(
            &position,
            "/sourceAnalysisId",
            "模拟持仓缺少 sourceAnalysisId",
        )?;
        let status = required_json_string(&position, "/status", "模拟持仓缺少 status")?;
        let created_at = json_string(&position, "/entryAt").unwrap_or_else(|| now.clone());
        let updated_at = json_string(&position, "/exitAt").unwrap_or_else(|| now.clone());
        tx.execute(
            "insert into simulated_positions
                (id, code, source_analysis_id, status, payload_json, created_at, updated_at)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                code,
                source_analysis_id,
                status,
                position.to_string(),
                created_at,
                updated_at
            ],
        )
        .map_err(|err| format!("写入模拟持仓失败：{err}"))?;
    }
    Ok(())
}

fn insert_position_event_tx(tx: &Transaction<'_>, event: &Value) -> Result<(), String> {
    let id = required_json_string(event, "/id", "持仓事件缺少 id")?;
    let position_id = required_json_string(event, "/positionId", "持仓事件缺少 positionId")?;
    let event_kind = required_json_string(event, "/eventKind", "持仓事件缺少 eventKind")?;
    let occurred_at = json_string(event, "/occurredAt").unwrap_or_else(now);
    let source_kind = json_string(event, "/sourceKind");
    let source_ref = json_string(event, "/sourceRef");
    let agent_note = json_string(event, "/agentNoteMd");

    tx.execute(
        "insert into position_events
            (id, position_id, event_kind, occurred_at, source_kind, source_ref,
             payload_json, agent_note_md, created_at)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id,
            position_id,
            event_kind,
            occurred_at,
            source_kind,
            source_ref,
            event.to_string(),
            agent_note,
            now()
        ],
    )
    .map_err(|err| format!("写入持仓事件失败：{err}"))?;
    Ok(())
}

/// 持仓事件（append-only 审计流）：opened / reviewed / adjusted / trimmed / added /
/// stop_triggered / invalidated / closed。事件不能被修改或删除，是 Agent 复盘时
/// 看到"这个仓位是怎么走过来的"的唯一可信来源。
pub fn append_position_event(app: AppHandle, event: Value) -> Result<Value, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let id = required_json_string(&event, "/id", "持仓事件缺少 id")?;
    let position_id = required_json_string(&event, "/positionId", "持仓事件缺少 positionId")?;
    let event_kind = required_json_string(&event, "/eventKind", "持仓事件缺少 eventKind")?;
    let occurred_at = json_string(&event, "/occurredAt").unwrap_or_else(now);
    let source_kind = json_string(&event, "/sourceKind");
    let source_ref = json_string(&event, "/sourceRef");
    let agent_note = json_string(&event, "/agentNoteMd");

    connection
        .execute(
            "insert into position_events
                (id, position_id, event_kind, occurred_at, source_kind, source_ref,
                 payload_json, agent_note_md, created_at)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                position_id,
                event_kind,
                occurred_at,
                source_kind,
                source_ref,
                event.to_string(),
                agent_note,
                now()
            ],
        )
        .map_err(|err| format!("写入持仓事件失败：{err}"))?;
    Ok(event)
}

/// 一次拉多个持仓的事件，按 occurred_at 升序。前端在内存里按 positionId 分组。
pub fn list_position_events_batch(
    app: AppHandle,
    position_ids: Vec<String>,
) -> Result<Vec<Value>, String> {
    if position_ids.is_empty() {
        return Ok(vec![]);
    }
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let placeholders = (0..position_ids.len())
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "select payload_json from position_events
         where position_id in ({placeholders})
         order by occurred_at asc
         limit 2000"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|err| format!("读取批量持仓事件失败：{err}"))?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(position_ids.iter()), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|err| format!("读取批量持仓事件失败：{err}"))?
        .filter_map(|raw| raw.ok())
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        .collect();
    Ok(rows)
}
