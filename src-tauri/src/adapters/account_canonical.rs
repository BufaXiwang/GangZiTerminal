//! Account canonical Tauri commands —— spec `account-module.md §4`。
//!
//! 三个 canonical 入口（spec 要求外部世界只通过它们读写账户）：
//! - `fetch_account` —— 聚合读：snapshot / positions / orders / watchlist / triggers（events 待 22 event-type 落地）
//! - `operate_account` —— **Tauri command 已 fail**（spec 不允许前端绕过 Agent）；
//!   Agent tool 入口通过 `pipeline::account::canonical::dispatch` 走全 7 action
//! - `update_watchlist` —— 非交易写入口（user / agent / system 共用）
//!
//! 请求 / 响应结构严格 spec 字段集；同时提供 `snapshot_to_packet_value` /
//! `trigger_to_packet` / `order_to_packet` 三个投影 helper 供 agent tool 复用。

#![allow(dead_code)] // 请求 DTO 持有 spec 全字段，部分参数（如 trigger_handled）由前端按需启用

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::AppHandle;

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FetchAccountInclude {
    #[serde(default)]
    pub snapshot: bool,
    #[serde(default)]
    pub positions: bool,
    #[serde(default)]
    pub orders: bool,
    #[serde(default)]
    pub watchlist: bool,
    #[serde(default)]
    pub events: bool,
    #[serde(default)]
    pub triggers: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FetchAccountRequest {
    #[serde(default)]
    pub include: Option<FetchAccountInclude>,
    #[serde(default)]
    pub position_status: Option<String>,
    #[serde(default)]
    pub order_active: Option<bool>,
    #[serde(default)]
    pub order_status_in: Option<Vec<String>>,
    #[serde(default)]
    pub trigger_handled: Option<Value>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FetchAccountResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub positions: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orders: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watchlist: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<Value>,
    /// spec WarningCode 闭集合（shared-types.md §5）；adapter 转 enum 再序列化。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::domain::shared::WarningCode>,
}

/// canonical fetch_account —— spec `account-module.md §4` `FetchAccountResponse`。
///
/// 包含多次同步 SQLite 调用：用 spawn_blocking 包裹避免阻塞 tokio worker。
#[tauri::command]
pub async fn fetch_account(
    app: AppHandle,
    request: Option<FetchAccountRequest>,
) -> Result<FetchAccountResponse, String> {
    tokio::task::spawn_blocking(move || fetch_account_inner(app, request))
        .await
        .map_err(|e| format!("fetch_account 任务异常：{e}"))?
}

fn fetch_account_inner(
    app: AppHandle,
    request: Option<FetchAccountRequest>,
) -> Result<FetchAccountResponse, String> {
    let req = request.unwrap_or_default();
    let inc = req.include.unwrap_or(FetchAccountInclude {
        snapshot: true,
        positions: true,
        orders: false,
        watchlist: true,
        events: false,
        triggers: false,
    });
    use crate::domain::shared::WarningCode;
    let mut resp = FetchAccountResponse::default();
    let mut warnings: Vec<WarningCode> = Vec::new();
    let limit = req.limit.unwrap_or(100).clamp(1, 500);
    let offset = req.offset.unwrap_or(0).max(0);
    let position_status = req.position_status.as_deref().unwrap_or("open");

    let svc = crate::pipeline::account::AccountService::new(app.clone());
    let snapshot = match svc.snapshot() {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(error = %e, "fetch_account snapshot 失败");
            warnings.push(WarningCode::DataPartial);
            None
        }
    };
    if inc.snapshot {
        resp.snapshot = snapshot.as_ref().map(|s| snapshot_to_packet_value(&app, s));
    }
    if inc.positions {
        if let Some(s) = &snapshot {
            let positions = match position_status {
                "open" => s.open_positions.clone(),
                "closed" => s.closed_positions.clone(),
                "all" => {
                    let mut all = s.open_positions.clone();
                    all.extend(s.closed_positions.clone());
                    all
                }
                other => {
                    return Err(format!(
                        "invalid_input: positionStatus 未知 `{other}`，应为 open|closed|all"
                    ));
                }
            };
            let arr = serde_json::to_value(&positions).unwrap_or(Value::Array(Vec::new()));
            if let Some(arr) = arr.as_array() {
                let start = (offset as usize).min(arr.len());
                let end = (start + limit as usize).min(arr.len());
                resp.positions = arr[start..end].to_vec();
            }
        }
    }
    if inc.watchlist {
        // spec PacketWatchlistItem：tsCode / name / addedAt / note / quote
        // - addedAt 来自最近一条 watchlist_added/watchlist_note_updated 事件
        // - note 来自 watchlist_notes 读模型
        // - name 来自 stocks 档案
        // - quote 摘要来自 MARKET_SNAPSHOT
        use crate::infrastructure::quotes::snapshot::market_snapshot;
        let codes = crate::infrastructure::account::watchlist::list_strings();
        let mut items = Vec::with_capacity(codes.len());
        for ts in codes {
            let quote_snip = market_snapshot::get(&ts).map(|q| {
                json!({
                    "price": q.price.as_ref().map(|p| p.value()),
                    "changePercent": q.change_percent,
                    "volume": q.day_volume.as_ref().map(|v| v.value()),
                    "amount": q.day_amount.as_ref().map(|v| v.value()),
                    "source": q.source.as_str(),
                    "freshness": q.freshness,
                })
            });
            let note = crate::infrastructure::account::watchlist_events::note_for(&app, &ts)
                .ok()
                .flatten();
            let added_at = latest_watchlist_added_at(&app, &ts);
            let name = quote_snip
                .as_ref()
                .and_then(|q| q.get("name").and_then(|v| v.as_str()).map(String::from))
                .or_else(|| stock_name_for(&app, &ts));
            let mut item = json!({"tsCode": ts});
            if let Some(n) = name {
                item.as_object_mut().unwrap().insert("name".into(), Value::String(n));
            }
            if let Some(t) = added_at {
                item.as_object_mut().unwrap().insert("addedAt".into(), Value::String(t));
            }
            if let Some(n) = note {
                item.as_object_mut().unwrap().insert("note".into(), Value::String(n));
            }
            if let Some(q) = quote_snip {
                item.as_object_mut().unwrap().insert("quote".into(), q);
            } else {
                item.as_object_mut().unwrap().insert(
                    "warnings".into(),
                    json!(["quote_missing"]),
                );
            }
            items.push(item);
        }
        resp.watchlist = items;
    }
    if inc.triggers {
        // spec：默认 triggerHandled = false（未处理）
        let handled = match &req.trigger_handled {
            None => Some(false),
            Some(v) if v.is_string() && v.as_str() == Some("all") => None,
            Some(v) => v.as_bool().or(Some(false)),
        };
        match crate::infrastructure::account::trigger_repo::list_filtered(
            &app, handled, limit, offset,
        ) {
            Ok(items) => {
                resp.triggers = items.iter().map(trigger_to_packet).collect();
            }
            Err(e) => {
                tracing::warn!(error = %e, "fetch_account triggers 失败");
                warnings.push(WarningCode::DataPartial);
            }
        }
    }
    if inc.orders {
        // spec §4：orderActive / orderStatusIn 联合过滤；默认 orderActive=true
        use crate::infrastructure::account::orders_repo;
        let active_default = req.order_active.unwrap_or(true);
        let orders = if let Some(statuses) = req.order_status_in.as_ref() {
            if statuses.is_empty() {
                Vec::new()
            } else {
                let str_refs: Vec<&str> = statuses.iter().map(String::as_str).collect();
                orders_repo::list_by_status(&app, &str_refs, limit, offset)
                    .unwrap_or_default()
            }
        } else if active_default {
            orders_repo::list_active(&app, limit, offset).unwrap_or_default()
        } else {
            orders_repo::list_all(&app, limit, offset).unwrap_or_default()
        };
        // spec agent-runtime-module.md §3 PacketOrder：orderId / tsCode / side /
        // orderType / limitPrice / quantity / filledQuantity / status / intent /
        // positionId / createdAt / expiresAt（不含 updatedAt / actor）
        resp.orders = orders.into_iter().map(order_to_packet).collect();
    }
    if inc.events {
        // spec §4 events 是可选 include；从 account_events 流读取（spec §2 21 类型）。
        use crate::infrastructure::account::account_events_repo;
        match account_events_repo::list_recent(&app, limit as usize, offset as usize) {
            Ok(events) => {
                resp.events = events
                    .iter()
                    .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
                    .collect();
            }
            Err(e) => {
                tracing::warn!(error = %e, "fetch_account events 失败");
                warnings.push(WarningCode::DataPartial);
            }
        }
    }
    if !warnings.is_empty() {
        resp.warnings = warnings;
    }
    Ok(resp)
}

/// spec agent-runtime-module.md §3 PacketAccountSnapshot 投影。
/// 字段集严格按 spec：capturedAt / cash / availableCash / frozenCash / marketValue /
/// totalAssets / realizedPnl / unrealizedPnl / totalPnl / pricedPositionCount /
/// unpricedPositionCount / valuationFreshness / openPositionCount / pendingOrderCount /
/// warnings?。不包含 openPositions / closedPositions / initialCash 等 domain 内部字段。
pub fn snapshot_to_packet_value(
    app: &AppHandle,
    s: &crate::domain::account::snapshot::AccountSnapshot,
) -> Value {
    use crate::infrastructure::account::orders_repo;
    use crate::infrastructure::quotes::snapshot::market_snapshot;
    let pending_orders = orders_repo::list_active(app, 500, 0).unwrap_or_default();
    let pending_order_count = pending_orders.len();
    let frozen_cash: f64 = pending_orders
        .iter()
        .filter(|o| matches!(o.side, crate::domain::account::order::OrderSide::Buy))
        .map(|o| {
            let lp = o.limit_price.as_ref().map(|p| p.value()).unwrap_or(0.0);
            let qty = o.quantity.value() as f64 - o.filled_quantity.value() as f64;
            lp * qty
        })
        .sum();
    let available_cash = (s.cash.value() - frozen_cash).max(0.0);
    use crate::domain::shared::WarningCode;
    let mut priced = 0usize;
    let mut unpriced = 0usize;
    let mut worst_status = "fresh";
    let mut warnings: Vec<WarningCode> = Vec::new();
    for p in &s.open_positions {
        if let Some(q) = market_snapshot::get(p.code.as_str()) {
            if q.price.is_some() {
                priced += 1;
                if matches!(
                    q.freshness.status,
                    crate::domain::shared::FreshnessStatus::Stale
                ) && worst_status == "fresh"
                {
                    worst_status = "stale";
                }
            } else {
                unpriced += 1;
                worst_status = "missing";
            }
        } else {
            unpriced += 1;
            worst_status = "missing";
        }
    }
    if unpriced > 0 {
        warnings.push(WarningCode::DataPartial);
    }
    // spec account-module.md §2「warnings 例如 quote_missing / quote_stale / data_partial」
    // 与 valuationFreshness.status 对齐
    if worst_status == "stale" {
        warnings.push(WarningCode::QuoteStale);
    }
    let captured_at = chrono::DateTime::from_timestamp_millis(s.captured_at.value())
        .map(|d| d.to_rfc3339())
        .unwrap_or_default();
    // spec shared-types.md §4 Freshness 完整字段：status / capturedAt? / exchangeTime? /
    // ageMs? / source? / warning?。ageMs 取所有 priced position 中最旧的 capturedAt。
    let now_ms = chrono::Utc::now().timestamp_millis();
    let oldest_quote_ms = s
        .open_positions
        .iter()
        .filter_map(|p| market_snapshot::get(p.code.as_str()))
        .map(|q| q.captured_at.value())
        .min();
    let age_ms = oldest_quote_ms.map(|t| now_ms - t);
    let valuation_warning = match worst_status {
        "stale" => Some("quote_stale"),
        "missing" => Some("quote_missing"),
        _ => None,
    };
    let mut valuation_freshness = json!({ "status": worst_status });
    if let Some(obj) = valuation_freshness.as_object_mut() {
        if let Some(ms) = age_ms {
            obj.insert("ageMs".into(), json!(ms));
        }
        if let Some(w) = valuation_warning {
            obj.insert("warning".into(), Value::String(w.into()));
        }
        obj.insert("capturedAt".into(), Value::String(captured_at.clone()));
    }
    let mut out = json!({
        "capturedAt": captured_at,
        "cash": s.cash.value(),
        "availableCash": available_cash,
        "frozenCash": frozen_cash,
        "marketValue": s.market_value.value(),
        "totalAssets": s.total_assets.value(),
        "realizedPnl": s.realized_pnl.value(),
        "unrealizedPnl": s.unrealized_pnl.value(),
        "totalPnl": s.total_pnl.value(),
        "pricedPositionCount": priced,
        "unpricedPositionCount": unpriced,
        "valuationFreshness": valuation_freshness,
        "openPositionCount": s.open_positions.len(),
        "pendingOrderCount": pending_order_count,
    });
    if !warnings.is_empty() {
        out.as_object_mut().unwrap().insert(
            "warnings".into(),
            Value::Array(warnings.iter().map(|w| json!(w.as_str())).collect()),
        );
    }
    out
}

/// spec agent-runtime-module.md §3 PacketAccountTrigger 投影：trigger_id / triggerType /
/// positionId? / orderId? / tsCode? / price? / threshold? / quoteFreshness? / warnings? /
/// occurredAt / handled。不含内部 `eventId`。
pub fn trigger_to_packet(t: &crate::domain::account::trigger::AccountTrigger) -> Value {
    let mut out = json!({
        "triggerId": t.trigger_id,
        "triggerType": t.trigger_type.as_str(),
        "occurredAt": t.occurred_at,
        "handled": t.handled,
    });
    let obj = out.as_object_mut().unwrap();
    if let Some(p) = &t.position_id {
        obj.insert("positionId".into(), Value::String(p.clone()));
    }
    if let Some(o) = &t.order_id {
        obj.insert("orderId".into(), Value::String(o.clone()));
    }
    if let Some(ts) = &t.ts_code {
        obj.insert("tsCode".into(), Value::String(ts.clone()));
    }
    if let Some(p) = t.price {
        obj.insert("price".into(), json!(p));
    }
    if let Some(thr) = &t.threshold {
        obj.insert("threshold".into(), serde_json::to_value(thr).unwrap_or(Value::Null));
    }
    if let Some(f) = &t.quote_freshness {
        obj.insert("quoteFreshness".into(), serde_json::to_value(f).unwrap_or(Value::Null));
    }
    if !t.warnings.is_empty() {
        obj.insert(
            "warnings".into(),
            Value::Array(
                t.warnings
                    .iter()
                    .map(|w| Value::String(w.as_str().to_string()))
                    .collect(),
            ),
        );
    }
    out
}

/// spec agent-runtime-module.md §3 PacketOrder 投影。
fn order_to_packet(o: crate::domain::account::order::Order) -> Value {
    use crate::domain::account::order::{OrderIntent, OrderSide, OrderType};
    let side = match o.side {
        OrderSide::Buy => "buy",
        OrderSide::Sell => "sell",
    };
    let order_type = match o.order_type {
        OrderType::Market => "market",
        OrderType::Limit => "limit",
    };
    let intent = match o.intent {
        OrderIntent::OpenPosition => "open_position",
        OrderIntent::ScaleIn => "scale_in",
        OrderIntent::ScaleOut => "scale_out",
        OrderIntent::ClosePosition => "close_position",
        OrderIntent::DirectOrder => "direct_order",
    };
    let created_at = chrono::DateTime::from_timestamp_millis(o.created_at.value())
        .map(|t| t.to_rfc3339())
        .unwrap_or_default();
    let expires_at = o
        .expires_at
        .as_ref()
        .and_then(|t| chrono::DateTime::from_timestamp_millis(t.value()).map(|d| d.to_rfc3339()));
    let mut out = json!({
        "orderId": o.order_id,
        "tsCode": o.ts_code.as_str(),
        "side": side,
        "orderType": order_type,
        "quantity": o.quantity.value(),
        "filledQuantity": o.filled_quantity.value(),
        "status": o.status.as_str(),
        "intent": intent,
        "createdAt": created_at,
    });
    if let Some(p) = o.limit_price.as_ref() {
        out.as_object_mut()
            .unwrap()
            .insert("limitPrice".into(), json!(p.value()));
    }
    if let Some(pid) = o.position_id {
        out.as_object_mut()
            .unwrap()
            .insert("positionId".into(), Value::String(pid));
    }
    if let Some(exp) = expires_at {
        out.as_object_mut()
            .unwrap()
            .insert("expiresAt".into(), Value::String(exp));
    }
    out
}

fn latest_watchlist_added_at(app: &AppHandle, ts_code: &str) -> Option<String> {
    use crate::infrastructure::db::{migrate, open_database};
    let c = open_database(app).ok()?;
    migrate(&c).ok()?;
    c.query_row(
        "select occurred_at from watchlist_events
         where ts_code = ?1 and event_type = 'watchlist_added'
         order by occurred_at desc limit 1",
        rusqlite::params![ts_code],
        |r| r.get::<_, String>(0),
    )
    .ok()
}

fn stock_name_for(app: &AppHandle, ts_code: &str) -> Option<String> {
    use crate::infrastructure::quotes::repository as qrepo;
    // StockRow.code 是 6 位数字；先把 ts_code 拆出 code 部分对照
    let code6 = ts_code.split('.').next()?;
    let stocks = qrepo::list_stocks(app).ok()?;
    stocks.into_iter().find(|s| s.code == code6).map(|s| s.name)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperateAccountRequest {
    pub episode_id: String,
    pub account_input: Value,
}

/// spec `account-module.md §4 OperateAccountResponse` —— 与 pipeline canonical 完全
/// 同一 DTO，避免两套类型分叉。
pub use crate::pipeline::account::canonical::OperateAccountResult as OperateAccountResponse;

/// canonical operate_account —— **仅 Agent tool 入口**。
///
/// spec `account-module.md §4`「`operate_account` 只允许 Agent tool / 外部自动化
/// 决策运行时调用，人工 UI 和 `system` 维护流程都不能直接调用它创建订单。前端
/// 不能绕过 Agent 下单」。Tauri command 入口被显式拒绝，避免前端 / 手工调用绕过
/// TradeIntent 状态机和 RunScope evidence 校验。
#[tauri::command]
pub async fn operate_account(
    _app: AppHandle,
    _request: OperateAccountRequest,
) -> Result<OperateAccountResponse, String> {
    Err(
        "forbidden: operate_account 只能由 Agent tool 入口调用（spec account-module.md §4），\
         前端不允许绕过 Agent 下单"
            .into(),
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateWatchlistRequest {
    pub action: String,
    pub ts_code: String,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateWatchlistResponse {
    pub accepted: bool,
    /// spec `shared-types.md §5` ErrorCode 闭集合。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<crate::domain::shared::ErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[tauri::command]
pub async fn update_watchlist(
    app: AppHandle,
    request: UpdateWatchlistRequest,
) -> Result<UpdateWatchlistResponse, String> {
    use crate::domain::shared::ErrorCode;
    use crate::pipeline::account::{
        parse_watchlist_action, update_watchlist_dispatch, UpdateWatchlistInput,
    };
    let Some(action) = parse_watchlist_action(&request.action) else {
        return Ok(UpdateWatchlistResponse {
            accepted: false,
            reason: Some(ErrorCode::InvalidInput),
            message: Some(format!("unknown action `{}`", request.action)),
        });
    };
    let resp = update_watchlist_dispatch(
        &app,
        "user",
        UpdateWatchlistInput {
            action,
            ts_code: request.ts_code,
            note: request.note,
            reason: request.reason,
        },
    );
    Ok(UpdateWatchlistResponse {
        accepted: resp.accepted,
        reason: resp.reason,
        message: resp.message,
    })
}
