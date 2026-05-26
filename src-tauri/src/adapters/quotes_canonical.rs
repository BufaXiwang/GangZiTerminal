//! Quotes canonical Tauri commands —— spec `quotes-module.md §4`。
//!
//! Spec 要求 Quotes 对外只暴露 3 类读取 command：`list_market` / `fetch_data` /
//! `scan_market`。
//! - `list_market` —— `adapters/market_commands.rs::list_market`
//! - `scan_market` —— `adapters/quotes_commands.rs::scan_market` / `scan_market_query`
//! - `fetch_data` —— 本文件，按 spec 接 `tsCodes` + `include`；走共享
//!   `freshness_rules::resolve_quote_view`（与 fetch_quotes tool 一致），返回严格
//!   freshness / tradeStatus / 1h 硬过期 / tradeDate eligibility 派生。

#![allow(dead_code)] // 请求 DTO 持有 spec 全字段（include.klines/minuteKlines/indicators/dailyBasic/events 等待后端实装）

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::AppHandle;

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FetchDataInclude {
    #[serde(default)]
    pub quote: bool,
    #[serde(default)]
    pub intraday: bool,
    #[serde(default)]
    pub klines: Option<Vec<String>>,
    #[serde(default)]
    pub minute_klines: Option<Vec<String>>,
    #[serde(default)]
    pub indicators: Option<Value>,
    #[serde(default)]
    pub profile: bool,
    #[serde(default)]
    pub daily_basic: bool,
    #[serde(default)]
    pub events: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchDataRequest {
    pub ts_codes: Option<Vec<String>>,
    #[serde(default)]
    pub include: Option<FetchDataInclude>,
    #[serde(default)]
    pub limit: Option<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseError {
    /// spec quotes-module.md §4 errors[].code: ErrorCode（闭集合）
    pub code: crate::domain::shared::ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_code: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchDataResponse {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ResponseError>,
    pub items: Vec<FetchDataItem>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchDataItem {
    pub ts_code: String,
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_freshness: Option<Value>,
    /// spec quotes-module.md §4：item warnings 严格 WarningCode 闭集合。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::domain::shared::WarningCode>,
}

const MAX_TS_CODES: usize = 200;

/// `fetch_data` —— spec `quotes-module.md §4`。
///
/// 当前实装 `tsCodes` 路径 + `include.quote` 完整字段（spec §2 quote 有效性规则：
/// freshness / tradeStatus / 1h 硬过期 / tradeDate eligibility）。其它 include
/// 字段（kline / minuteKlines / intraday / indicators / dailyBasic / events）尚未接入
/// query facade，响应中以 item-level `data_partial` warning 表达覆盖不完整。
#[tauri::command]
pub async fn fetch_data(
    app: AppHandle,
    request: FetchDataRequest,
) -> Result<FetchDataResponse, String> {
    use crate::infrastructure::quotes::snapshot::market_snapshot;

    let ts_codes = match request.ts_codes {
        Some(v) if !v.is_empty() => v,
        _ => {
            return Ok(FetchDataResponse {
                errors: vec![ResponseError {
                    code: crate::domain::shared::ErrorCode::InvalidInput,
                    message: Some("tsCodes 必填且非空".into()),
                    field: Some("tsCodes".into()),
                    ts_code: None,
                }],
                items: Vec::new(),
            });
        }
    };
    if ts_codes.len() > MAX_TS_CODES {
        return Ok(FetchDataResponse {
            errors: vec![ResponseError {
                code: crate::domain::shared::ErrorCode::InvalidInput,
                message: Some(format!("tsCodes 数量超过上限 {MAX_TS_CODES}")),
                field: Some("tsCodes".into()),
                ts_code: None,
            }],
            items: Vec::new(),
        });
    }

    // 去重保留首次出现顺序
    let mut seen = std::collections::HashSet::new();
    let dedup: Vec<String> = ts_codes
        .into_iter()
        .filter(|c| seen.insert(c.clone()))
        .collect();

    let include = request.include.unwrap_or_default();
    let want_quote = include.quote || (!include.profile && !include.daily_basic);
    let mut items = Vec::with_capacity(dedup.len());

    for ts in dedup {
        use crate::domain::shared::WarningCode;
        let mut warnings: Vec<WarningCode> = Vec::new();
        let mut category = "stock".to_string();
        let mut name = None;
        let mut quote_value: Option<Value> = None;

        // 解析 category（用 ts_code 后缀粗判，详情靠 universe）
        if ts.ends_with(".SH") || ts.ends_with(".SZ") || ts.ends_with(".BJ") {
            // 默认 stock；指数 / 基金的 category 由 universe 表更精准识别 
        }

        let mut quote_freshness_value: Option<Value> = None;
        if want_quote {
            // spec quotes-module.md §2 quote 有效性规则（共用 helper，与 fetch_quotes 一致）
            use crate::domain::quotes::freshness_rules::resolve_quote_view;
            use crate::domain::shared::market_time::resolve_market_time;
            use crate::domain::shared::OccurredAt as OA;
            let now_ms = chrono::Utc::now().timestamp_millis();
            let mkt = resolve_market_time(OA::new(now_ms));
            let snap = market_snapshot::get(&ts);
            let resolved = resolve_quote_view(snap.as_ref(), &mkt, now_ms);
            quote_freshness_value =
                Some(serde_json::to_value(&resolved.freshness).unwrap_or(Value::Null));
            match resolved.quote {
                Some(q) => {
                    name = Some(q.name.clone());
                    let mut v = serde_json::to_value(q).unwrap_or(Value::Null);
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert(
                            "tradeStatus".into(),
                            Value::String(q.trade_status.as_str().to_string()),
                        );
                    }
                    quote_value = Some(v);
                }
                None => {
                    let w = resolved.warning.unwrap_or(WarningCode::QuoteMissing);
                    warnings.push(w);
                }
            }
        }
        if include.intraday
            || include.klines.is_some()
            || include.minute_klines.is_some()
            || include.indicators.is_some()
            || include.daily_basic
            || include.events
        {
            warnings.push(WarningCode::DataPartial);
        }
        // category 改判 —— 用 universe 表后缀判（轻量）
        if let Ok(rows) = crate::infrastructure::quotes::repository::list_indexes(&app) {
            if rows.iter().any(|r| r.ts_code == ts) {
                category = "index".into();
            }
        }
        if let Ok(rows) = crate::infrastructure::quotes::repository::list_listed_funds(&app) {
            if rows.iter().any(|r| r.ts_code == ts) {
                category = "fund".into();
            }
        }

        items.push(FetchDataItem {
            ts_code: ts,
            category,
            name,
            quote: quote_value,
            quote_freshness: quote_freshness_value,
            warnings,
        });
    }

    Ok(FetchDataResponse {
        errors: Vec::new(),
        items,
    })
}

// ============ list_market canonical ===================================
//
// spec `quotes-module.md §4 list_market`：query 排序优先级：
//   exact tsCode → exact name → name prefix → name substring → status=listed
//   → category → tsCode。

#[derive(Debug, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListMarketRequest {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub include_quote: bool,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMarketItem {
    pub ts_code: String,
    pub code: String,
    pub name: String,
    pub category: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_freshness: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::domain::shared::WarningCode>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMarketPage {
    pub limit: i64,
    pub offset: i64,
    pub has_more: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMarketResponse {
    pub items: Vec<ListMarketItem>,
    pub page: ListMarketPage,
}

const LIST_MARKET_MAX_LIMIT: i64 = 500;
const LIST_MARKET_DEFAULT_LIMIT: i64 = 100;

/// spec `quotes-module.md §4 list_market`。
#[tauri::command]
pub async fn list_market(
    app: AppHandle,
    request: Option<ListMarketRequest>,
) -> Result<ListMarketResponse, String> {
    let req = request.unwrap_or_default();
    let limit = req
        .limit
        .unwrap_or(LIST_MARKET_DEFAULT_LIMIT)
        .clamp(1, LIST_MARKET_MAX_LIMIT);
    let offset = req.offset.unwrap_or(0).max(0);
    let want_quote = req.include_quote;

    let q_raw = req.query.unwrap_or_default();
    // spec §4：query trim + 折叠连续空白
    let query = q_raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let query_lower = query.to_lowercase();

    let mut all: Vec<RawItem> = Vec::with_capacity(7000);
    // stocks
    if matches!(req.category.as_deref(), None | Some("stock")) {
        if let Ok(rows) = crate::infrastructure::quotes::repository::list_stocks(&app) {
            for r in rows {
                let suffix = match r.market.as_str() {
                    "sh" => "SH",
                    "sz" => "SZ",
                    "bj" => "BJ",
                    _ => continue,
                };
                all.push(RawItem {
                    ts_code: format!("{}.{}", r.code, suffix),
                    code: r.code,
                    name: r.name,
                    category: "stock",
                    sector: r.sector,
                    is_listed: true,
                });
            }
        }
    }
    // indexes
    if matches!(req.category.as_deref(), None | Some("index")) {
        if let Ok(rows) = crate::infrastructure::quotes::repository::list_indexes(&app) {
            for r in rows {
                all.push(RawItem {
                    ts_code: r.ts_code,
                    code: r.code,
                    name: r.name,
                    category: "index",
                    sector: None,
                    is_listed: true,
                });
            }
        }
    }
    // funds
    if matches!(req.category.as_deref(), None | Some("fund")) {
        if let Ok(rows) = crate::infrastructure::quotes::repository::list_listed_funds(&app) {
            for r in rows {
                all.push(RawItem {
                    ts_code: r.ts_code,
                    code: r.code,
                    name: r.name,
                    category: "fund",
                    sector: r.fund_type,
                    is_listed: true,
                });
            }
        }
    }

    // 过滤 + 评分排序
    let filtered: Vec<(RawItem, i32)> = if query.is_empty() {
        all.into_iter().map(|it| (it, 0)).collect()
    } else {
        all.into_iter()
            .filter_map(|it| match query_score(&it, &query_lower) {
                0 => None,
                s => Some((it, s)),
            })
            .collect()
    };

    // 稳定排序：score desc, listed desc, category asc, ts_code asc
    let mut sorted = filtered;
    sorted.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.0.is_listed.cmp(&a.0.is_listed))
            .then_with(|| category_order(a.0.category).cmp(&category_order(b.0.category)))
            .then_with(|| a.0.ts_code.cmp(&b.0.ts_code))
    });

    let total = sorted.len();
    let start = (offset as usize).min(total);
    let end = (start + limit as usize).min(total);
    let has_more = end < total;
    let page_items: Vec<RawItem> = sorted.into_iter().skip(start).take(end - start).map(|x| x.0).collect();

    let items: Vec<ListMarketItem> = page_items
        .into_iter()
        .map(|it| build_list_market_item(&app, it, want_quote))
        .collect();

    Ok(ListMarketResponse {
        items,
        page: ListMarketPage {
            limit,
            offset,
            has_more,
        },
    })
}

struct RawItem {
    ts_code: String,
    code: String,
    name: String,
    category: &'static str,
    sector: Option<String>,
    is_listed: bool,
}

fn category_order(c: &str) -> u8 {
    match c {
        "stock" => 0,
        "index" => 1,
        "fund" => 2,
        _ => 3,
    }
}

/// spec §4 query 排序优先级：exact tsCode(7) → exact name(6) → name prefix(5)
/// → name substring(4) → ts_code substring(3) → 不命中(0)。
fn query_score(it: &RawItem, q_lower: &str) -> i32 {
    let ts_lower = it.ts_code.to_lowercase();
    let name_lower = it.name.to_lowercase();
    if ts_lower == q_lower {
        return 7;
    }
    if name_lower == q_lower {
        return 6;
    }
    if name_lower.starts_with(q_lower) {
        return 5;
    }
    if name_lower.contains(q_lower) {
        return 4;
    }
    if ts_lower.contains(q_lower) {
        return 3;
    }
    0
}

fn build_list_market_item(app: &AppHandle, it: RawItem, want_quote: bool) -> ListMarketItem {
    use crate::infrastructure::quotes::snapshot::market_snapshot;
    let mut warnings = Vec::new();
    let mut quote_value: Option<Value> = None;
    let mut quote_freshness_value: Option<Value> = None;
    if want_quote {
        use crate::domain::quotes::freshness_rules::resolve_quote_view;
        use crate::domain::shared::market_time::resolve_market_time;
        use crate::domain::shared::OccurredAt as OA;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mkt = resolve_market_time(OA::new(now_ms));
        let snap = market_snapshot::get(&it.ts_code);
        let resolved = resolve_quote_view(snap.as_ref(), &mkt, now_ms);
        quote_freshness_value =
            Some(serde_json::to_value(&resolved.freshness).unwrap_or(Value::Null));
        match resolved.quote {
            Some(q) => {
                // spec §4 list_market.quote 摘要（不含五档盘口）
                quote_value = Some(serde_json::json!({
                    "tradeDate": q.trade_date,
                    "price": q.price.as_ref().map(|p| p.value()),
                    "change": q.change.as_ref().map(|c| c.value()),
                    "changePercent": q.change_percent,
                    "open": q.open.as_ref().map(|p| p.value()),
                    "high": q.high.as_ref().map(|p| p.value()),
                    "low": q.low.as_ref().map(|p| p.value()),
                    "previousClose": q.previous_close.as_ref().map(|p| p.value()),
                    "volume": q.day_volume.as_ref().map(|v| v.value()),
                    "amount": q.day_amount.as_ref().map(|v| v.value()),
                }));
            }
            None => {
                warnings.push(
                    resolved
                        .warning
                        .unwrap_or(crate::domain::shared::WarningCode::QuoteMissing),
                );
            }
        }
    }
    let _ = app; // future-proof：当前不用 app 但保留参数以便未来扩展（如 daily_basic）
    ListMarketItem {
        ts_code: it.ts_code,
        code: it.code,
        name: it.name,
        category: it.category,
        sector: it.sector,
        quote: quote_value,
        quote_freshness: quote_freshness_value,
        warnings,
    }
}
