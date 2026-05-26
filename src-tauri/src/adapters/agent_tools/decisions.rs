//! `record_decision_episode` / `record_decision_review` —— Agent Runtime 审计写工具。
//!
//! 对齐 spec `agent-runtime-module.md §4` `RecordDecisionEpisodeToolInput` /
//! `RecordDecisionReviewToolInput`。第一阶段：接受 canonical input JSON，
//! evidence_selectors 直接持久化为 `EvidenceRef::ToolCall` snapshot 占位；
//! 真实 hydrate（按 selector kind/source 校验是否来自本 run packet / tool_result）在 。

use crate::domain::agent::types::ToolResultContent;
use crate::infrastructure::agent_runtime::{episodes_repo, reviews_repo};
use crate::pipeline::agent::tools::{err_text, ok_json, Tool, ToolContext};
use crate::domain::agent_runtime::decisions::{
    DecisionEpisode, DecisionReview, DecisionReviewResult, DecisionReviewTrigger,
    EpisodeAction, EpisodeActionStatus, EvidenceRef, RiskPlan,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::AppHandle;

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 按 run_id 查 `agent_runs.trigger_kind`，让 `record_decision_episode` 写入
/// 本 run 真实的 trigger 而不是硬编码 "user_chat"。
fn resolve_run_trigger_kind(app: &AppHandle, run_id: &str) -> Option<String> {
    use crate::infrastructure::db::{migrate, open_database};
    let conn = open_database(app).ok()?;
    migrate(&conn).ok()?;
    conn.query_row(
        "select trigger_kind from agent_runs where run_id = ?1",
        rusqlite::params![run_id],
        |r| r.get::<_, String>(0),
    )
    .ok()
}

fn parse_action(s: &str) -> Option<EpisodeAction> {
    Some(match s {
        "no_action" => EpisodeAction::NoAction,
        "add_watchlist" => EpisodeAction::AddWatchlist,
        "remove_watchlist" => EpisodeAction::RemoveWatchlist,
        "place_order" => EpisodeAction::PlaceOrder,
        "cancel_order" => EpisodeAction::CancelOrder,
        "open_position" => EpisodeAction::OpenPosition,
        "scale_position" => EpisodeAction::ScalePosition,
        "close_position" => EpisodeAction::ClosePosition,
        "adjust_protection" => EpisodeAction::AdjustProtection,
        "record_invalidation_signal" => EpisodeAction::RecordInvalidationSignal,
        _ => return None,
    })
}

fn parse_action_status(s: &str) -> Option<EpisodeActionStatus> {
    Some(match s {
        "no_action" => EpisodeActionStatus::NoAction,
        "intended" => EpisodeActionStatus::Intended,
        "submitted" => EpisodeActionStatus::Submitted,
        "blocked" => EpisodeActionStatus::Blocked,
        "deferred" => EpisodeActionStatus::Deferred,
        _ => return None,
    })
}

fn parse_review_trigger(s: &str) -> Option<DecisionReviewTrigger> {
    Some(match s {
        "position_closed" => DecisionReviewTrigger::PositionClosed,
        "stop_loss" => DecisionReviewTrigger::StopLoss,
        "take_profit" => DecisionReviewTrigger::TakeProfit,
        "time_stop" => DecisionReviewTrigger::TimeStop,
        "invalidated" => DecisionReviewTrigger::Invalidated,
        "order_filled" => DecisionReviewTrigger::OrderFilled,
        "order_rejected" => DecisionReviewTrigger::OrderRejected,
        "order_expired" => DecisionReviewTrigger::OrderExpired,
        "scheduled_review" => DecisionReviewTrigger::ScheduledReview,
        "manual_review" => DecisionReviewTrigger::ManualReview,
        _ => return None,
    })
}

/// 把 `evidence_selectors` 校验并 hydrate 成结构化 EvidenceRef。
///
/// spec §2 hydrate 校验集：
/// - `kind ∈ {news, quote, account_snapshot, position, order, account_trigger, strategy, tool_call}`
/// - `source ∈ {packet, tool_result, recent_episode, recent_review, linked_episode, replay_ref}`
/// - `tool_result` source：`id` 必须是本 run 内已注册的 tool_call_id
/// - `news` / `quote` / `position` / `order` / `account_trigger` / `strategy`：根据 id 去对应
///   本地读模型抽取结构化 snapshot；找不到则拒绝
/// - `recent_episode` / `recent_review` / `linked_episode`：当前阶段允许「id 必须在本 run
///   或当前 packet 已注入的 recent summaries 范围」——简化为：episode/review id 必须存在
/// - `replay_ref`：当前阶段保留 selector 但 snapshot 只承载 schemaVersion + ref
async fn hydrate_evidence(
    app: &AppHandle,
    run_id: &str,
    selectors: &Value,
) -> Result<Vec<EvidenceRef>, String> {
    let arr = match selectors.as_array() {
        Some(a) => a,
        None => return Ok(Vec::new()),
    };
    const KNOWN_KINDS: &[&str] = &[
        "news",
        "quote",
        "account_snapshot",
        "position",
        "order",
        "account_trigger",
        "strategy",
        "tool_call",
    ];
    const KNOWN_SOURCES: &[&str] = &[
        "packet",
        "tool_result",
        "recent_episode",
        "recent_review",
        "linked_episode",
        "replay_ref",
    ];
    let mut out = Vec::with_capacity(arr.len());
    for (i, sel) in arr.iter().enumerate() {
        let kind = sel
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("invalid_input: selector[{i}] 缺 kind"))?;
        if !KNOWN_KINDS.contains(&kind) {
            return Err(format!("invalid_input: selector[{i}] 未知 kind `{kind}`"));
        }
        let id = sel
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("invalid_input: selector[{i}] 缺 id"))?
            .to_string();
        let source = sel
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("packet");
        if !KNOWN_SOURCES.contains(&source) {
            return Err(format!(
                "invalid_input: selector[{i}] 未知 source `{source}`"
            ));
        }

        // spec §2 EvidenceRef 校验矩阵：source × kind
        validate_source(app, run_id, source, kind, &id, i).await?;
        let snapshot = build_snapshot_for(app, kind, &id, source).await?;
        out.push(match kind {
            "news" => EvidenceRef::News { id, snapshot },
            "quote" => EvidenceRef::Quote { id, snapshot },
            "account_snapshot" => EvidenceRef::AccountSnapshot { id, snapshot },
            "position" => EvidenceRef::Position { id, snapshot },
            "order" => EvidenceRef::Order { id, snapshot },
            "account_trigger" => EvidenceRef::AccountTrigger { id, snapshot },
            "strategy" => EvidenceRef::Strategy { id, snapshot },
            "tool_call" => EvidenceRef::ToolCall { id, snapshot },
            _ => unreachable!(),
        });
    }
    Ok(out)
}

/// spec §2 EvidenceRef 校验：按 source 决定可接受的 kind / id 来源。
async fn validate_source(
    app: &AppHandle,
    run_id: &str,
    source: &str,
    kind: &str,
    id: &str,
    i: usize,
) -> Result<(), String> {
    use crate::pipeline::agent_runtime::run_scope;
    match source {
        "tool_result" => {
            // tool_call_id 必须在本 run 已注册
            let app_c = app.clone();
            let run_c = run_id.to_string();
            let id_c = id.to_string();
            let exists = tokio::task::spawn_blocking(move || {
                crate::infrastructure::agent::tool_calls_repo::exists_in_run(
                    &app_c, &run_c, &id_c,
                )
            })
            .await
            .map_err(|e| format!("hydrate tool_call 查询异常：{e}"))?
            .map_err(|e| format!("hydrate tool_call 查询失败：{e}"))?;
            if !exists {
                return Err(format!(
                    "invalid_input: selector[{i}] source=tool_result，但 id `{id}` 不属于当前 run"
                ));
            }
        }
        "packet" => {
            let scope = run_scope::get(run_id).unwrap_or_default();
            let ok = match kind {
                "news" => scope.packet_news_ids.contains(id),
                "quote" => scope.packet_ts_codes.contains(id),
                "position" => scope.packet_position_ids.contains(id),
                "strategy" => scope.packet_strategy_ids.contains(id),
                // spec §2：account_snapshot 用唯一占位 "current"（packet 注入的快照
                // 总是"当前时刻"快照，不接受任意 id 防伪造）
                "account_snapshot" => id == "current",
                // 真实校验：account_trigger 必须在 DB 中存在
                "account_trigger" => trigger_exists(app, id).await,
                // order 走 orders_repo 校验
                "order" => order_exists(app, id).await,
                // tool_call 不属于 packet
                "tool_call" => false,
                _ => false,
            };
            if !ok {
                return Err(format!(
                    "invalid_input: selector[{i}] source=packet 但 (kind={kind}, id={id}) 不在 packet 注入范围"
                ));
            }
        }
        "recent_episode" => {
            let scope = run_scope::get(run_id).unwrap_or_default();
            if !scope.recent_episode_ids.contains(id) {
                return Err(format!(
                    "invalid_input: selector[{i}] source=recent_episode 但 id `{id}` 不在 packet 注入的 recentEpisodes 范围"
                ));
            }
        }
        "recent_review" => {
            let scope = run_scope::get(run_id).unwrap_or_default();
            if !scope.recent_review_ids.contains(id) {
                return Err(format!(
                    "invalid_input: selector[{i}] source=recent_review 但 id `{id}` 不在 packet 注入的 recentReviews 范围"
                ));
            }
        }
        "linked_episode" => {
            // spec §2：linked_episode 只允许「Runtime 已建立显式因果链接」时使用：
            //   orderId → episodeId 反查命中，或 manual_replay 指定 ref。
            // 反查：在 agent_order_intent_index 里 episode_id == id 才允许。
            let app_c = app.clone();
            let id_c = id.to_string();
            let allowed = tokio::task::spawn_blocking(move || -> Result<bool, String> {
                use crate::infrastructure::db::{migrate, open_database};
                let c = open_database(&app_c)?;
                migrate(&c)?;
                let n: i64 = c
                    .query_row(
                        "select count(*) from agent_order_intent_index where episode_id = ?1",
                        rusqlite::params![id_c],
                        |r| r.get(0),
                    )
                    .map_err(|e| format!("查询 order_intent_index 失败：{e}"))?;
                Ok(n > 0)
            })
            .await
            .map_err(|e| format!("hydrate linked_episode 异常：{e}"))??;
            let scope = run_scope::get(run_id).unwrap_or_default();
            let in_replay = scope.replay_episode_ids.contains(id);
            if !allowed && !in_replay {
                return Err(format!(
                    "invalid_input: selector[{i}] source=linked_episode 但 id `{id}` 无 order→episode 反查链或 manual_replay 引用"
                ));
            }
        }
        "replay_ref" => {
            // spec §2：仅 manual_replay run 允许 replay_ref，且 id 须在 replay_episode_ids 中
            let scope = run_scope::get(run_id).unwrap_or_default();
            if scope.trigger_kind != "manual_replay" {
                return Err(format!(
                    "invalid_input: selector[{i}] source=replay_ref 只允许在 manual_replay run 使用"
                ));
            }
            if !scope.replay_episode_ids.contains(id) {
                return Err(format!(
                    "invalid_input: selector[{i}] source=replay_ref id `{id}` 不在 manual_replay 指定范围"
                ));
            }
        }
        _ => unreachable!("source 已在外层 KNOWN_SOURCES 校验"),
    }
    Ok(())
}

async fn trigger_exists(app: &AppHandle, trigger_id: &str) -> bool {
    let app_c = app.clone();
    let id_c = trigger_id.to_string();
    tokio::task::spawn_blocking(move || -> bool {
        use rusqlite::params;
        use crate::infrastructure::db::{migrate, open_database};
        let Ok(c) = open_database(&app_c) else { return false };
        if migrate(&c).is_err() {
            return false;
        }
        let n: i64 = c
            .query_row(
                "select count(*) from account_triggers where trigger_id = ?1",
                params![id_c],
                |r| r.get(0),
            )
            .unwrap_or(0);
        n > 0
    })
    .await
    .unwrap_or(false)
}

async fn order_exists(app: &AppHandle, order_id: &str) -> bool {
    let app_c = app.clone();
    let id_c = order_id.to_string();
    tokio::task::spawn_blocking(move || -> bool {
        crate::infrastructure::account::orders_repo::get(&app_c, &id_c)
            .map(|o| o.is_some())
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

async fn build_snapshot_for(
    app: &AppHandle,
    kind: &str,
    id: &str,
    source: &str,
) -> Result<Value, String> {
    let base = json!({
        "schemaVersion": 1,
        "capturedAt": now_iso(),
        "selectorSource": source,
    });
    let mut snap = base;
    match kind {
        "news" => {
            let app_c = app.clone();
            let ids = vec![id.to_string()];
            let rows = tokio::task::spawn_blocking(move || {
                crate::infrastructure::news::repository::get_news_items_by_ids(app_c, ids)
            })
            .await
            .map_err(|e| format!("hydrate news 异常：{e}"))?
            .map_err(|e| format!("hydrate news 失败：{e}"))?;
            if let Some(item) = rows.into_iter().next() {
                let obj = snap.as_object_mut().unwrap();
                obj.insert("newsId".into(), Value::String(item.id));
                obj.insert("title".into(), Value::String(item.title));
                if let Some(s) = item.summary {
                    obj.insert("summary".into(), Value::String(s));
                }
                if let Some(u) = item.link {
                    obj.insert("url".into(), Value::String(u));
                }
                if let Some(p) = item.published {
                    obj.insert("publishedAt".into(), Value::String(p));
                }
            } else if source != "packet" {
                return Err(format!(
                    "invalid_input: news evidence id `{id}` 不存在于本地"
                ));
            } else {
                snap.as_object_mut()
                    .unwrap()
                    .insert("note".into(), Value::String("news_missing".into()));
            }
        }
        "quote" => {
            // id 视为 tsCode；从 market_snapshot 取
            if let Some(q) = crate::infrastructure::quotes::snapshot::market_snapshot::get(id) {
                let obj = snap.as_object_mut().unwrap();
                obj.insert("tsCode".into(), Value::String(q.code.as_str().to_string()));
                obj.insert("name".into(), Value::String(q.name.clone()));
                if let Some(p) = q.price.as_ref().map(|p| p.value()) {
                    obj.insert("price".into(), serde_json::json!(p));
                }
                if let Some(cp) = q.change_percent {
                    obj.insert("changePercent".into(), serde_json::json!(cp));
                }
                if let Some(v) = q.day_volume.as_ref().map(|v| v.value()) {
                    obj.insert("volume".into(), serde_json::json!(v));
                }
                if let Some(a) = q.day_amount.as_ref().map(|a| a.value()) {
                    obj.insert("amount".into(), serde_json::json!(a));
                }
                obj.insert(
                    "freshness".into(),
                    serde_json::to_value(&q.freshness).unwrap_or(Value::Null),
                );
            } else {
                return Err(format!(
                    "invalid_input: quote evidence tsCode `{id}` 在 MARKET_SNAPSHOT 中不存在"
                ));
            }
        }
        "account_snapshot" => {
            let svc = crate::pipeline::account::AccountService::new(app.clone());
            if let Ok(s) = svc.snapshot() {
                let pending = crate::infrastructure::account::orders_repo::list_active(app, 500, 0)
                    .map(|v| v.len())
                    .unwrap_or(0);
                let obj = snap.as_object_mut().unwrap();
                obj.insert("cash".into(), serde_json::json!(s.cash.value()));
                obj.insert("totalAssets".into(), serde_json::json!(s.total_assets.value()));
                obj.insert("marketValue".into(), serde_json::json!(s.market_value.value()));
                obj.insert("totalPnl".into(), serde_json::json!(s.total_pnl.value()));
                obj.insert(
                    "openPositionCount".into(),
                    serde_json::json!(s.open_positions.len()),
                );
                obj.insert("pendingOrderCount".into(), serde_json::json!(pending));
            }
        }
        "position" => {
            let svc = crate::pipeline::account::AccountService::new(app.clone());
            if let Ok(s) = svc.snapshot() {
                if let Some(p) = s.open_positions.iter().find(|p| p.id.as_str() == id) {
                    let obj = snap.as_object_mut().unwrap();
                    obj.insert("positionId".into(), Value::String(p.id.as_str().to_string()));
                    obj.insert(
                        "tsCode".into(),
                        Value::String(p.code.as_str().to_string()),
                    );
                    obj.insert(
                        "quantity".into(),
                        serde_json::json!(p.current_shares.value()),
                    );
                    // spec EvidencePositionSnapshot：sellableQuantity / avgCost / marketPrice? /
                    // unrealizedPnl? / protection?。当前 domain Position 没有
                    // sellableQuantity 显式字段——退化为 current_shares（T+1 详细由
                    // PositionLot 模型派生，未引入前用 shares）。
                    obj.insert(
                        "sellableQuantity".into(),
                        serde_json::json!(p.current_shares.value()),
                    );
                    obj.insert(
                        "avgCost".into(),
                        serde_json::json!(p.avg_entry_price.value()),
                    );
                    // marketPrice / unrealizedPnl：从 MARKET_SNAPSHOT
                    if let Some(q) = crate::infrastructure::quotes::snapshot::market_snapshot::get(
                        p.code.as_str(),
                    ) {
                        if let Some(price) = q.price {
                            let mp = price.value();
                            obj.insert("marketPrice".into(), serde_json::json!(mp));
                            let unrealized = (mp - p.avg_entry_price.value())
                                * (p.current_shares.value() as f64);
                            obj.insert("unrealizedPnl".into(), serde_json::json!(unrealized));
                        }
                    }
                    // protection：spec PositionProtection 字段集
                    let mut protection = serde_json::json!({
                        "enabled": p.stop_loss.is_some() || p.take_profit.is_some()
                            || p.time_stop_at.is_some(),
                        "revision": 1,
                    });
                    if let Some(sl) = p.stop_loss {
                        protection
                            .as_object_mut()
                            .unwrap()
                            .insert("stopLoss".into(), serde_json::json!(sl.value()));
                    }
                    if let Some(tp) = p.take_profit {
                        protection
                            .as_object_mut()
                            .unwrap()
                            .insert("takeProfit".into(), serde_json::json!(tp.value()));
                    }
                    if let Some(ts) = p.time_stop_at {
                        if let Some(dt) =
                            chrono::DateTime::from_timestamp_millis(ts.value())
                        {
                            protection.as_object_mut().unwrap().insert(
                                "timeStopAt".into(),
                                Value::String(dt.to_rfc3339()),
                            );
                        }
                    }
                    obj.insert("protection".into(), protection);
                } else {
                    return Err(format!(
                        "invalid_input: position evidence id `{id}` 不在 open positions"
                    ));
                }
            }
        }
        "account_trigger" => {
            // 真实查 account_triggers 表 fetch trigger 详情
            use crate::infrastructure::db::{migrate, open_database};
            if let Ok(c) = open_database(app) {
                if migrate(&c).is_ok() {
                    if let Ok((tt, pos_id, order_id, ts_code, occurred_at)) = c.query_row(
                        "select trigger_type, position_id, order_id, ts_code, occurred_at
                         from account_triggers where trigger_id = ?1",
                        rusqlite::params![id],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, Option<String>>(1)?,
                                r.get::<_, Option<String>>(2)?,
                                r.get::<_, Option<String>>(3)?,
                                r.get::<_, String>(4)?,
                            ))
                        },
                    ) {
                        let obj = snap.as_object_mut().unwrap();
                        obj.insert("triggerId".into(), Value::String(id.to_string()));
                        obj.insert("triggerType".into(), Value::String(tt));
                        if let Some(p) = pos_id {
                            obj.insert("positionId".into(), Value::String(p));
                        }
                        if let Some(o) = order_id {
                            obj.insert("orderId".into(), Value::String(o));
                        }
                        if let Some(ts) = ts_code {
                            obj.insert("tsCode".into(), Value::String(ts));
                        }
                        obj.insert("occurredAt".into(), Value::String(occurred_at));
                    } else {
                        return Err(format!(
                            "invalid_input: account_trigger evidence id `{id}` 不存在"
                        ));
                    }
                }
            }
        }
        "order" => {
            // 真实查 orders_repo fetch order 详情（spec EvidenceOrderSnapshot 完整字段）
            if let Ok(Some(o)) =
                crate::infrastructure::account::orders_repo::get(app, id)
            {
                let obj = snap.as_object_mut().unwrap();
                obj.insert("orderId".into(), Value::String(o.order_id));
                obj.insert("tsCode".into(), Value::String(o.ts_code.as_str().to_string()));
                obj.insert(
                    "side".into(),
                    Value::String(match o.side {
                        crate::domain::account::order::OrderSide::Buy => "buy",
                        crate::domain::account::order::OrderSide::Sell => "sell",
                    }.into()),
                );
                obj.insert(
                    "orderType".into(),
                    Value::String(match o.order_type {
                        crate::domain::account::order::OrderType::Market => "market",
                        crate::domain::account::order::OrderType::Limit => "limit",
                    }.into()),
                );
                if let Some(lp) = o.limit_price.as_ref() {
                    obj.insert("limitPrice".into(), serde_json::json!(lp.value()));
                }
                obj.insert("quantity".into(), serde_json::json!(o.quantity.value()));
                obj.insert(
                    "filledQuantity".into(),
                    serde_json::json!(o.filled_quantity.value()),
                );
                obj.insert("status".into(), Value::String(o.status.as_str().to_string()));
            } else {
                return Err(format!(
                    "invalid_input: order evidence id `{id}` 不存在"
                ));
            }
        }
        "strategy" => {
            let cards =
                crate::infrastructure::agent_runtime::strategy_cards_repo::list(app, None)
                    .unwrap_or_default();
            if let Some(c) = cards.iter().find(|c| c.strategy_id == id) {
                let obj = snap.as_object_mut().unwrap();
                obj.insert("strategyId".into(), Value::String(c.strategy_id.clone()));
                obj.insert("version".into(), serde_json::json!(c.version));
                obj.insert("name".into(), Value::String(c.name.clone()));
                obj.insert("description".into(), Value::String(c.description.clone()));
                obj.insert(
                    "status".into(),
                    Value::String(
                        match c.status {
                            crate::domain::agent_runtime::decisions::StrategyStatus::Active => {
                                "active"
                            }
                            crate::domain::agent_runtime::decisions::StrategyStatus::Paused => {
                                "paused"
                            }
                        }
                        .into(),
                    ),
                );
                // spec §2 EvidenceStrategySnapshot: configSummary required
                // 合并 entry / exit / risk rules 头部条目作为摘要
                let mut summary_parts: Vec<String> = Vec::new();
                if !c.config.entry_rules.is_empty() {
                    summary_parts.push(format!("entry: {}", c.config.entry_rules.join("; ")));
                }
                if !c.config.exit_rules.is_empty() {
                    summary_parts.push(format!("exit: {}", c.config.exit_rules.join("; ")));
                }
                if !c.config.risk_rules.is_empty() {
                    summary_parts.push(format!("risk: {}", c.config.risk_rules.join("; ")));
                }
                let mut config_summary = summary_parts.join(" | ");
                if config_summary.is_empty() {
                    config_summary = c.description.clone();
                }
                obj.insert("configSummary".into(), Value::String(config_summary));
            } else {
                return Err(format!(
                    "invalid_input: strategy evidence id `{id}` 不存在"
                ));
            }
        }
        "tool_call" => {
            // spec §2 ToolCallEvidenceSnapshot: name / inputSummary / isError 必填
            // 通过 agent_tool_calls 查真实 row；失败 fail-closed（不允许 placeholder）。
            use crate::infrastructure::db::{migrate, open_database};
            let c = open_database(app).map_err(|e| format!("db_error: {e}"))?;
            migrate(&c).map_err(|e| format!("db_error: {e}"))?;
            let row: Option<(String, Option<String>, Option<String>, i64)> = c
                .query_row(
                    "select name, input_summary_json, output_summary_json, is_error
                     from agent_tool_calls where tool_call_id = ?1",
                    rusqlite::params![id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    },
                )
                .ok();
            let (name, input_summary, output_summary, is_error) = match row {
                Some(r) => r,
                None => {
                    return Err(format!(
                        "invalid_input: tool_call evidence id `{id}` 不存在"
                    ));
                }
            };
            let obj = snap.as_object_mut().unwrap();
            obj.insert("toolCallId".into(), Value::String(id.to_string()));
            obj.insert("name".into(), Value::String(name));
            obj.insert(
                "inputSummary".into(),
                input_summary
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .unwrap_or(Value::Null),
            );
            obj.insert(
                "outputSummary".into(),
                output_summary
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .unwrap_or(Value::Null),
            );
            obj.insert("isError".into(), serde_json::json!(is_error != 0));
        }
        _ => {}
    }
    Ok(snap)
}

// ============ RecordDecisionEpisodeTool =================================

pub struct RecordDecisionEpisodeTool {
    app: AppHandle,
}

impl RecordDecisionEpisodeTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for RecordDecisionEpisodeTool {
    fn name(&self) -> &'static str {
        "record_decision_episode"
    }
    fn side_effect(&self) -> crate::pipeline::agent::tools::SideEffect {
        crate::pipeline::agent::tools::SideEffect::NonTradingWrite
    }

    fn description(&self) -> &'static str {
        "记录一次可复盘的投资判断（DecisionEpisode）。无论 no_action / add_watchlist / \
         交易意图 / blocked，只要形成判断都必须落 episode。evidence_selectors 必须显式声明。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "symbols":      {"type": "array", "items": {"type": "string"}},
                "thesis":       {"type": "string"},
                "action":       {"type": "string"},
                "actionStatus": {"type": "string"},
                "blockedReason":{"type": "string"},
                "confidence":   {"type": "number"},
                "riskPlan":     {"type": "object"},
                "strategyIds":  {"type": "array", "items": {"type": "string"}},
                "evidenceSelectors": {"type": "array", "items": {"type": "object"}}
            },
            "required": ["thesis", "action", "actionStatus", "strategyIds", "evidenceSelectors"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let thesis = match input.get("thesis").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => return err_text("invalid_input: thesis 不能为空"),
        };
        let action_str = match input.get("action").and_then(Value::as_str) {
            Some(s) => s,
            None => return err_text("invalid_input: 缺 action"),
        };
        let Some(action) = parse_action(action_str) else {
            return err_text(format!("invalid_input: 未知 action `{action_str}`"));
        };
        // spec §2 「record_decision_episode 必须显式带 actionStatus」（schema required）；
        // 缺字段直接 fail 而不是兜底，避免契约暧昧。
        let status_str = match input.get("actionStatus").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s,
            _ => return err_text("invalid_input: actionStatus 必填"),
        };
        let Some(action_status) = parse_action_status(status_str) else {
            return err_text(format!("invalid_input: 未知 actionStatus `{status_str}`"));
        };
        // spec §2: action="no_action" ⇔ actionStatus="no_action"
        let no_action = matches!(action, EpisodeAction::NoAction);
        let no_status = matches!(action_status, EpisodeActionStatus::NoAction);
        if no_action != no_status {
            return err_text(
                "invalid_input: action=no_action 与 actionStatus=no_action 必须同时出现或同时不出现",
            );
        }
        let symbols: Vec<String> = input
            .get("symbols")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let strategy_ids: Vec<String> = input
            .get("strategyIds")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let evidence_refs = match hydrate_evidence(
            &self.app,
            &ctx.run_id,
            input
                .get("evidenceSelectors")
                .unwrap_or(&Value::Array(vec![])),
        )
        .await
        {
            Ok(v) => v,
            Err(msg) => return err_text(msg),
        };
        let risk_plan: Option<RiskPlan> = input
            .get("riskPlan")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        // 从当前 AgentRun row 取 trigger_kind；查不到时降级为 "user_chat"
        // （chat 入口当前阶段不写 agent_runs，详见 Phase 续）。
        let trigger_kind = resolve_run_trigger_kind(&self.app, &ctx.run_id)
            .unwrap_or_else(|| "user_chat".to_string());

        let episode = DecisionEpisode {
            episode_id: new_id(),
            run_id: ctx.run_id.clone(),
            trigger_kind,
            symbols,
            thesis,
            action,
            action_status,
            blocked_reason: input
                .get("blockedReason")
                .and_then(Value::as_str)
                .map(String::from),
            confidence: input.get("confidence").and_then(Value::as_f64),
            risk_plan,
            strategy_ids,
            evidence_refs,
            created_at: now_iso(),
        };
        match episodes_repo::insert(&self.app, &episode) {
            Ok(()) => (
                ok_json(json!({
                    "accepted": true,
                    "episodeId": episode.episode_id,
                })),
                false,
            ),
            Err(msg) => err_text(format!("db_error: {msg}")),
        }
    }
}

// ============ RecordDecisionReviewTool ==================================

pub struct RecordDecisionReviewTool {
    app: AppHandle,
}

impl RecordDecisionReviewTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for RecordDecisionReviewTool {
    fn name(&self) -> &'static str {
        "record_decision_review"
    }
    fn side_effect(&self) -> crate::pipeline::agent::tools::SideEffect {
        crate::pipeline::agent::tools::SideEffect::NonTradingWrite
    }

    fn description(&self) -> &'static str {
        "记录一次 DecisionEpisode 的复盘。trigger 必须是订单终态 / 保护条件命中 / \
         定时巡检 / 手动复盘等明确事实。evidence_selectors 必须显式声明。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "episodeId":   {"type": "string"},
                "trigger":     {"type": "string"},
                "result":      {"type": "object"},
                "conclusion":  {"type": "string"},
                "suggestedChange": {"type": "object"},
                "warnings":    {"type": "array", "items": {"type": "string"}},
                "evidenceSelectors": {"type": "array", "items": {"type": "object"}}
            },
            "required": ["episodeId", "trigger", "conclusion", "evidenceSelectors"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let episode_id = match input.get("episodeId").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return err_text("invalid_input: episodeId 必填"),
        };
        let conclusion = match input.get("conclusion").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s.to_string(),
            _ => return err_text("invalid_input: conclusion 不能为空"),
        };
        let trigger_str = input.get("trigger").and_then(Value::as_str).unwrap_or("");
        let Some(trigger) = parse_review_trigger(trigger_str) else {
            return err_text(format!("invalid_input: 未知 trigger `{trigger_str}`"));
        };
        let result: Option<DecisionReviewResult> = input
            .get("result")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        let suggested_change = input.get("suggestedChange").cloned();
        let warnings: Vec<String> = input
            .get("warnings")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let evidence_refs = match hydrate_evidence(
            &self.app,
            &ctx.run_id,
            input
                .get("evidenceSelectors")
                .unwrap_or(&Value::Array(vec![])),
        )
        .await
        {
            Ok(v) => v,
            Err(msg) => return err_text(msg),
        };
        let review = DecisionReview {
            review_id: new_id(),
            episode_id,
            trigger,
            result,
            conclusion,
            suggested_change,
            evidence_refs,
            warnings,
            created_at: now_iso(),
        };
        match reviews_repo::insert(&self.app, &review) {
            Ok(()) => (
                ok_json(json!({
                    "accepted": true,
                    "reviewId": review.review_id,
                })),
                false,
            ),
            Err(msg) => err_text(format!("db_error: {msg}")),
        }
    }
}
