//! `fetch_quotes` —— Quotes 模块 canonical 读取工具。
//!
//! 对齐 docs/design/agent-runtime-module.md §4 `FetchQuotesToolInput`：
//! - `tsCodes` 与 `scan` 二选一；同时出现 / 同时缺失返回 `invalid_input`
//! - `tsCodes` 路径走 `MARKET_SNAPSHOT` 直读 + universe category 判定，
//!   返回 spec `PacketQuotes` 形态（snapshotAt/freshness/items[]/scan? 略）
//! - `scan` 路径走 `infrastructure::quotes::scanner::scan_market_query`，
//!   只填 `PacketQuotes.scan`，不隐式追加详情
//!
//! 不做远端 refresh；本地 snapshot 缺数据时通过 item / response warning 表达。

use crate::domain::agent::types::ToolResultContent;
use crate::domain::quotes::{
    InstrumentCategory, ScanCondition as DomainScanCondition, ScanOp as DomainScanOp,
    ScanResult as DomainScanResult, ScanSort as DomainScanSort, StockQuote,
};
use crate::domain::shared::WarningCode;
use crate::infrastructure::quotes::repository as qrepo;
use crate::infrastructure::quotes::scanner;
use crate::infrastructure::quotes::snapshot::market_snapshot;
use crate::pipeline::agent::tools::{err_text, ok_json, Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{json, Value};
use tauri::AppHandle;

const MAX_TS_CODES: usize = 200;

pub struct FetchQuotesTool {
    app: AppHandle,
}

impl FetchQuotesTool {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

#[async_trait]
impl Tool for FetchQuotesTool {
    fn name(&self) -> &'static str {
        "fetch_quotes"
    }

    fn description(&self) -> &'static str {
        "读取行情 / K 线 / 分时 / 技术指标 / 基本面 / 扫描结果。\
         tsCodes 与 scan 二选一；本地 snapshot 缺数据时返回 warning，不触发远端。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tsCodes": {"type": "array", "items": {"type": "string"}},
                "scan": {
                    "type": "object",
                    "properties": {
                        "filter": {"type": "string", "enum": [
                            "limit_up","limit_down","top_gain","top_loss",
                            "top_amount","top_volume"
                        ]},
                        "conditions": {"type": "array"},
                        "sortBy": {"type": "string"},
                        "limit": {"type": "integer"},
                        "category": {"type": "string", "enum": ["stock","index","fund"]}
                    }
                },
                "include": {
                    "type": "object",
                    "properties": {
                        "quote":    {"type": "boolean"},
                        "intraday": {"type": "boolean"},
                        "klines":   {"type": "array", "items": {"type": "string"}},
                        "minuteKlines": {"type": "array", "items": {"type": "string"}},
                        "indicators": {},
                        "profile":  {"type": "boolean"},
                        "dailyBasic": {"type": "boolean"},
                        "events":   {"type": "boolean"}
                    }
                }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> (Vec<ToolResultContent>, bool) {
        let has_ts_codes = input
            .get("tsCodes")
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        let has_scan = input.get("scan").map(Value::is_object).unwrap_or(false);
        if has_ts_codes == has_scan {
            return err_text("invalid_input: tsCodes 和 scan 必须二选一");
        }
        if has_scan && input.get("include").is_some() {
            return err_text("invalid_input: scan 路径不接受 include；如需详情请再调一次 fetch_quotes({tsCodes})");
        }

        if has_ts_codes {
            return exec_ts_codes_path(&self.app, &input).await;
        }
        exec_scan_path(&self.app, &input).await
    }
}

async fn exec_ts_codes_path(app: &AppHandle, input: &Value) -> (Vec<ToolResultContent>, bool) {
    let raw: Vec<String> = input
        .get("tsCodes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_uppercase()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if raw.len() > MAX_TS_CODES {
        return err_text(format!(
            "invalid_input: tsCodes 数量超过上限 {MAX_TS_CODES}"
        ));
    }
    // dedup 保留首次出现顺序
    let mut seen = std::collections::HashSet::new();
    let codes: Vec<String> = raw.into_iter().filter(|c| seen.insert(c.clone())).collect();
    // 格式校验：6 位数字 + .SH/.SZ/.BJ
    let mut errors: Vec<Value> = Vec::new();
    for c in &codes {
        if !is_valid_ts_code(c) {
            errors.push(json!({
                "code": "invalid_input",
                "tsCode": c,
                "message": "TsCode 格式应为 6 位数字 + .SH/.SZ/.BJ"
            }));
        }
    }
    if !errors.is_empty() {
        return (
            ok_json(json!({
                "snapshotAt": chrono::Utc::now().to_rfc3339(),
                "freshness": {"status": "missing"},
                "items": [],
                "errors": errors,
            })),
            true,
        );
    }

    // category 判定一次性建表（避免 per-code N+1 调用）
    let index_codes: std::collections::HashSet<String> =
        qrepo::list_indexes(app)
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.ts_code)
            .collect();
    let fund_codes: std::collections::HashSet<String> =
        qrepo::list_listed_funds(app)
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.ts_code)
            .collect();

    let mut items: Vec<Value> = Vec::with_capacity(codes.len());
    let mut worst_status = "fresh"; // fresh > stale > missing
    let mut any_response_warning: Vec<&'static str> = Vec::new();

    // spec quotes-module.md §2 quote 有效性规则共用 helper
    use crate::domain::quotes::freshness_rules::resolve_quote_view;
    use crate::domain::shared::market_time::resolve_market_time;
    use crate::domain::shared::OccurredAt as OA;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mkt = resolve_market_time(OA::new(now_ms));

    for ts in &codes {
        let snapshot = market_snapshot::get(ts);
        let category = if index_codes.contains(ts) {
            InstrumentCategory::Index
        } else if fund_codes.contains(ts) {
            InstrumentCategory::Fund
        } else {
            InstrumentCategory::Stock
        };
        let resolved = resolve_quote_view(snapshot.as_ref(), &mkt, now_ms);
        match resolved.quote {
            Some(q) => {
                let mut item_warnings = derive_item_warnings(q);
                if let Some(w) = resolved.warning {
                    let s = warning_str(w);
                    if !item_warnings.contains(&s) {
                        item_warnings.push(s);
                    }
                }
                match resolved.freshness.status {
                    crate::domain::shared::FreshnessStatus::Stale if worst_status == "fresh" => {
                        worst_status = "stale";
                    }
                    crate::domain::shared::FreshnessStatus::Missing => worst_status = "missing",
                    _ => {}
                }
                items.push(quote_to_packet_item(q, category, item_warnings));
            }
            None => {
                worst_status = "missing";
                let w = resolved
                    .warning
                    .map(warning_str)
                    .unwrap_or("quote_missing");
                any_response_warning.push(w);
                items.push(json!({
                    "tsCode": ts,
                    "name": "",
                    "category": category_str(category),
                    "source": "local",
                    "freshness": {"status": "missing", "warning": w},
                    "warnings": [w]
                }));
            }
        }
    }

    let mut response = json!({
        "snapshotAt": chrono::Utc::now().to_rfc3339(),
        "freshness": {"status": worst_status},
        "items": items,
    });
    if !any_response_warning.is_empty() {
        if let Some(obj) = response.as_object_mut() {
            obj.insert(
                "warnings".into(),
                Value::Array(any_response_warning.iter().map(|s| json!(s)).collect()),
            );
        }
    }
    (ok_json(response), false)
}

fn is_valid_ts_code(c: &str) -> bool {
    let bytes = c.as_bytes();
    if bytes.len() != 9 {
        return false;
    }
    bytes[..6].iter().all(|b| b.is_ascii_digit())
        && bytes[6] == b'.'
        && matches!(&bytes[7..9], b"SH" | b"SZ" | b"BJ")
}

fn category_str(c: InstrumentCategory) -> &'static str {
    match c {
        InstrumentCategory::Stock => "stock",
        InstrumentCategory::Index => "index",
        InstrumentCategory::Fund => "fund",
    }
}

fn derive_item_warnings(q: &StockQuote) -> Vec<&'static str> {
    let mut ws: Vec<&'static str> = Vec::new();
    for w in &q.warnings {
        ws.push(warning_str(*w));
    }
    if q.bid_levels.is_empty() || q.ask_levels.is_empty() {
        if !ws.contains(&"depth_missing") {
            ws.push("depth_missing");
        }
    }
    if q.price.is_none() {
        if !ws.contains(&"quote_price_missing") {
            ws.push("quote_price_missing");
        }
    }
    ws
}

fn warning_str(w: WarningCode) -> &'static str {
    use WarningCode::*;
    match w {
        QuoteMissing => "quote_missing",
        QuoteStale => "quote_stale",
        SnapshotExpired => "snapshot_expired",
        QuotePriceMissing => "quote_price_missing",
        DepthMissing => "depth_missing",
        InstrumentMissing => "instrument_missing",
        ProviderPartialFailure => "provider_partial_failure",
        ArticleMissing => "article_missing",
        QfqMissing => "qfq_missing",
        UsingUnadjustedKline => "using_unadjusted_kline",
        DailyBasicMissing => "daily_basic_missing",
        EventsMissing => "events_missing",
        StrategyOmitted => "strategy_omitted",
        MappingMissing => "mapping_missing",
        DataPartial => "data_partial",
    }
}

fn quote_to_packet_item(
    q: &StockQuote,
    category: InstrumentCategory,
    warnings: Vec<&'static str>,
) -> Value {
    let bid: Vec<Value> = q
        .bid_levels
        .iter()
        .map(|l| {
            json!({
                "price": l.price.as_ref().map(|p| p.value()),
                "volume": l.volume.as_ref().map(|v| v.value()),
            })
        })
        .collect();
    let ask: Vec<Value> = q
        .ask_levels
        .iter()
        .map(|l| {
            json!({
                "price": l.price.as_ref().map(|p| p.value()),
                "volume": l.volume.as_ref().map(|v| v.value()),
            })
        })
        .collect();
    json!({
        "tsCode": q.code.as_str(),
        "name": q.name,
        "category": category_str(category),
        "tradeDate": q.trade_date.to_compact(),
        "price": q.price.as_ref().map(|p| p.value()),
        "change": q.change.as_ref().map(|p| p.value()),
        "changePercent": q.change_percent,
        "open": q.open.as_ref().map(|p| p.value()),
        "high": q.high.as_ref().map(|p| p.value()),
        "low": q.low.as_ref().map(|p| p.value()),
        "previousClose": q.previous_close.as_ref().map(|p| p.value()),
        "limitUp": q.limit_up.as_ref().map(|p| p.value()),
        "limitDown": q.limit_down.as_ref().map(|p| p.value()),
        "volume": q.day_volume.as_ref().map(|v| v.value()),
        "amount": q.day_amount.as_ref().map(|v| v.value()),
        "turnoverRate": q.turnover_rate,
        "volumeRatio": q.volume_ratio,
        "tradeStatus": q.trade_status.as_str(),
        "source": q.source.as_str(),
        "capturedAt": q.captured_at.value(),
        "exchangeTime": q.exchange_time.as_ref().map(|t| t.value()),
        "freshness": {
            "status": match q.freshness.status {
                crate::domain::shared::FreshnessStatus::Fresh => "fresh",
                crate::domain::shared::FreshnessStatus::Stale => "stale",
                crate::domain::shared::FreshnessStatus::Missing => "missing",
            },
            "capturedAt": q.freshness.captured_at.as_ref().map(|t| t.value()),
            "ageMs": q.freshness.age_ms,
            "source": q.freshness.source.clone(),
            "warning": q.freshness.warning.map(|w| warning_str(w)),
        },
        "bid": bid,
        "ask": ask,
        "warnings": warnings,
    })
}

async fn exec_scan_path(app: &AppHandle, input: &Value) -> (Vec<ToolResultContent>, bool) {
    let scan = input.get("scan").cloned().unwrap_or(Value::Null);
    let filter_str = scan.get("filter").and_then(Value::as_str);
    let sort_str = scan.get("sortBy").and_then(Value::as_str);
    let limit = scan
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .min(500) as usize;

    // 构造 conditions（spec ScanCondition 集合；这里支持核心字段）
    let mut conditions: Vec<DomainScanCondition> = Vec::new();
    if let Some(filter) = filter_str {
        match filter {
            "limit_up" => conditions.push(DomainScanCondition::LimitUpHit),
            "limit_down" => conditions.push(DomainScanCondition::LimitDownHit),
            "top_gain" | "top_loss" | "top_amount" | "top_volume" => {}
            other => {
                return err_text(format!("invalid_input: scan.filter 未知 `{other}`"));
            }
        }
    }
    if let Some(arr) = scan.get("conditions").and_then(Value::as_array) {
        for c in arr {
            let field = c.get("field").and_then(Value::as_str).unwrap_or("");
            let op = c.get("op").and_then(Value::as_str).unwrap_or("");
            let value = c.get("value");
            let cond = parse_user_condition(field, op, value);
            match cond {
                Ok(cond) => conditions.push(cond),
                Err(e) => return err_text(e),
            }
        }
    }
    let sort_by = match sort_str {
        Some("change_pct_desc") => DomainScanSort::ChangePctDesc,
        Some("change_pct_asc") => DomainScanSort::ChangePctAsc,
        Some("amount_desc") => DomainScanSort::AmountDesc,
        Some("volume_desc") => DomainScanSort::VolumeDesc,
        Some("turnover_rate_desc") => DomainScanSort::TurnoverRateDesc,
        None => match filter_str {
            Some("limit_up") | Some("limit_down") | Some("top_amount") => {
                DomainScanSort::AmountDesc
            }
            Some("top_loss") => DomainScanSort::ChangePctAsc,
            Some("top_volume") => DomainScanSort::VolumeDesc,
            _ => DomainScanSort::ChangePctDesc,
        },
        Some(other) => {
            return err_text(format!("invalid_input: scan.sortBy 未知 `{other}`"));
        }
    };

    match scanner::scan_market_query(app, conditions, sort_by, limit).await {
        Ok(result) => (ok_json(scan_result_to_packet(result, filter_str, sort_str, limit)), false),
        Err(e) => err_text(format!("scan_market 错误：{e}")),
    }
}

fn scan_result_to_packet(
    result: DomainScanResult,
    filter_str: Option<&str>,
    sort_str: Option<&str>,
    limit: usize,
) -> Value {
    let items: Vec<Value> = result
        .items
        .iter()
        .map(|it| {
            json!({
                "rank": it.rank,
                "tsCode": it.code.as_str(),
                "name": it.name,
                "category": "stock",
                "price": it.price.as_ref().map(|p| p.value()),
                "changePercent": it.change_pct,
                "amount": it.amount.as_ref().map(|p| p.value()),
                "volume": it.volume.as_ref().map(|v| v.value()),
                "turnoverRate": it.turnover_rate,
                "volumeRatio": it.volume_ratio,
                "peTtm": it.pe,
                "pb": it.pb,
                "totalMv": it.total_mv.as_ref().map(|p| p.value()),
            })
        })
        .collect();
    json!({
        "snapshotAt": chrono::Utc::now().to_rfc3339(),
        "freshness": {"status": "fresh"},
        "scan": {
            "generatedAt": chrono::Utc::now().to_rfc3339(),
            "tradeDate": result.trade_date.to_compact(),
            "criteria": {
                "filter": filter_str,
                "sortBy": sort_str,
                "limit": limit,
            },
            "items": items,
        },
    })
}

fn parse_user_condition(
    field: &str,
    op: &str,
    value: Option<&Value>,
) -> Result<DomainScanCondition, String> {
    let scan_op = parse_scan_op(op, value)?;
    use crate::domain::quotes::ScanCondition as C;
    Ok(match field {
        "changePercent" | "change_pct" => C::ChangePct(scan_op),
        "amount" => C::Amount(scan_op),
        "volume" => C::Volume(scan_op),
        "turnoverRate" | "turnover_rate" => C::TurnoverRate(scan_op),
        "volumeRatio" | "volume_ratio" => C::VolumeRatio(scan_op),
        "peTtm" | "pe_ttm" => C::PeTtm(scan_op),
        "pb" => C::Pb(scan_op),
        "totalMv" | "total_mv" => C::TotalMv(scan_op),
        "circMv" | "circ_mv" => C::CircMv(scan_op),
        other => return Err(format!("invalid_input: 未知 field `{other}`")),
    })
}

fn parse_scan_op(op: &str, value: Option<&Value>) -> Result<DomainScanOp, String> {
    let single = || {
        value
            .and_then(Value::as_f64)
            .ok_or_else(|| "invalid_input: value 必须是数字".to_string())
    };
    let range = || {
        value
            .and_then(|v| v.as_array())
            .and_then(|a| {
                if a.len() == 2 {
                    Some((a[0].as_f64()?, a[1].as_f64()?))
                } else {
                    None
                }
            })
            .ok_or_else(|| "invalid_input: between 需要 [a,b]".to_string())
    };
    Ok(match op {
        "gt" => DomainScanOp::Gt(single()?),
        "gte" => DomainScanOp::Gte(single()?),
        "lt" => DomainScanOp::Lt(single()?),
        "lte" => DomainScanOp::Lte(single()?),
        "eq" => DomainScanOp::Eq(single()?),
        "between" => {
            let (a, b) = range()?;
            DomainScanOp::Between(a, b)
        }
        other => return Err(format!("invalid_input: 未知 op `{other}`")),
    })
}
