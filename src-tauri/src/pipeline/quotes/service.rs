//! Quotes 应用 use case — list_market / fetch_data / scan_market / refresh_*。
//!
//! Spec: docs/design/quotes-module.md §3 / §4 / §5

use crate::domain::quotes::{
    apply_band_helper, compute_indicators, compute_limit_band, core_indexes, derive_freshness,
    eligible_trade_date, Adjust as AdjEnum, CompanyEvent, DailyBasic, FreshnessIntent,
    IndicatorBasis, IndicatorName, IndicatorSnapshot, IntradaySeries, KlinePeriod, KlineSeries,
    MarketInstrument, MarketQuotesRefreshedPayload, MinuteKlinePeriod, MinuteKlineSeries,
    RefreshPurpose, RefreshScope, ScanCondition, ScanConditionField, ScanConditionValue,
    ScanCriteria, ScanFilter, ScanItem, ScanOp, ScanResult, ScanSortBy, ScanUniverse,
    StockProfile, StockQuote, TradeStatus,
};
use crate::domain::shared::{
    resolve_market_time, ErrorCode, Freshness, FreshnessStatus, InstrumentCategory,
    InstrumentStatus, MarketTimeContext, TradeDate, TsCode, WarningCode,
};
use crate::infrastructure::db::AppDb;
use crate::infrastructure::quotes::{
    CachedSnapshot, EastmoneyProvider, QuotesConfig, QuotesRepository, SinaProvider, SnapshotCache,
    TencentProvider, TradeCalendarRepo, TushareClient,
};
use chrono::Utc;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::sync::Arc;

/// Pipeline 内部错误 / response 错误条目。
///
/// Spec: docs/design/quotes-module.md §4 `ResponseError`。
/// adapters 层在边界把它映射为 `ResponseError`。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ResponseError {
    pub code: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_code: Option<TsCode>,
}

impl ResponseError {
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            message: None,
            field: None,
            ts_code: None,
        }
    }
    pub fn with_message(code: ErrorCode, msg: impl Into<String>) -> Self {
        Self {
            code,
            message: Some(msg.into()),
            field: None,
            ts_code: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuotesServiceConfig {
    pub config: QuotesConfig,
}

/// Quotes BC 对外能力。Adapter / scheduler 共用 `Arc<QuotesService>`。
pub struct QuotesService {
    pub(crate) db: AppDb,
    pub(crate) cache: Arc<SnapshotCache>,
    pub(crate) eastmoney: EastmoneyProvider,
    pub(crate) sina: SinaProvider,
    pub(crate) tencent: TencentProvider,
    pub(crate) tushare: TushareClient,
    pub(crate) calendar: Arc<TradeCalendarRepo>,
    #[allow(dead_code)]
    pub(crate) config: QuotesConfig,
    pub(crate) event_sink: std::sync::RwLock<Option<RefreshEventSink>>,
}

pub type RefreshEventSink = Arc<dyn Fn(MarketQuotesRefreshedPayload) + Send + Sync + 'static>;

impl QuotesService {
    pub fn new(db: AppDb, config: QuotesConfig) -> reqwest::Result<Self> {
        let cache = Arc::new(SnapshotCache::new());
        let eastmoney = EastmoneyProvider::new()?;
        let sina = SinaProvider::new()?;
        let tencent = TencentProvider::new()?;
        let tushare = TushareClient::new(config.tushare_token.clone())?;
        let calendar = Arc::new(TradeCalendarRepo::new(db.clone()));
        Ok(Self {
            db,
            cache,
            eastmoney,
            sina,
            tencent,
            tushare,
            calendar,
            config,
            event_sink: std::sync::RwLock::new(None),
        })
    }

    pub fn db(&self) -> &AppDb {
        &self.db
    }

    pub fn cache(&self) -> &Arc<SnapshotCache> {
        &self.cache
    }

    pub fn calendar(&self) -> &Arc<TradeCalendarRepo> {
        &self.calendar
    }

    pub fn tushare(&self) -> &TushareClient {
        &self.tushare
    }

    pub fn eastmoney(&self) -> &EastmoneyProvider {
        &self.eastmoney
    }

    pub fn set_event_sink(&self, sink: RefreshEventSink) {
        if let Ok(mut g) = self.event_sink.write() {
            *g = Some(sink);
        }
    }

    pub(crate) fn emit_refreshed(&self, payload: MarketQuotesRefreshedPayload) {
        if let Ok(g) = self.event_sink.read() {
            if let Some(sink) = g.as_ref() {
                sink(payload);
            }
        }
    }

    fn repo(&self) -> QuotesRepository<'_> {
        QuotesRepository::new(&self.db)
    }

    // ====================================================================== list_market

    pub fn list_market(&self, req: ListMarketRequest) -> ListMarketResponse {
        let limit = clamp(req.limit, 100, 500);
        let offset = req.offset.unwrap_or(0);
        let now = Utc::now();
        let ctx = resolve_market_time(now);
        let repo = self.repo();
        let (instruments, _total) = repo
            .list_instruments(req.category, req.query.as_deref(), limit, offset)
            .unwrap_or_default();

        let has_more = (instruments.len() as u32) == limit;
        let include_quote = req.include_quote.unwrap_or(false);

        let items: Vec<ListMarketItem> = instruments
            .into_iter()
            .map(|inst| {
                let mut item = ListMarketItem {
                    instrument: inst.clone(),
                    quote: None,
                    quote_freshness: None,
                    warnings: Vec::new(),
                };
                if include_quote {
                    let (q, f, w) =
                        self.resolve_quote_summary(&inst, &ctx, FreshnessIntent::Universe);
                    item.quote = q;
                    item.quote_freshness = f;
                    if let Some(code) = w {
                        item.warnings.push(code);
                    }
                }
                item
            })
            .collect();

        ListMarketResponse {
            items,
            page: ListMarketPage {
                limit,
                offset,
                has_more,
            },
        }
    }

    fn resolve_quote_summary(
        &self,
        inst: &MarketInstrument,
        ctx: &MarketTimeContext,
        intent: FreshnessIntent,
    ) -> (
        Option<ListMarketQuoteSummary>,
        Option<Freshness>,
        Option<WarningCode>,
    ) {
        let cached = self.cache.get(&inst.ts_code);
        let eligible = eligible_trade_date(ctx);
        let (snap, source) = match cached {
            Some(c) => (Some(c.quote.clone()), Some(c.source.clone())),
            None => {
                // 非交易时段：尝试 load close snapshot for latestCompletedTradeDate.
                if !eligible.is_intraday {
                    if let Ok(Some(q)) = self
                        .repo()
                        .load_close_snapshot(&inst.ts_code, eligible.trade_date)
                    {
                        let src = q.source.as_str().to_string();
                        (Some(q), Some(src))
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
                }
            }
        };
        let Some(quote) = snap else {
            let f = Freshness {
                status: FreshnessStatus::Missing,
                captured_at: None,
                exchange_time: None,
                age_ms: None,
                source: None,
                warning: Some(WarningCode::QuoteMissing),
            };
            return (None, Some(f), Some(WarningCode::QuoteMissing));
        };
        let source = source.unwrap_or_else(|| quote.source.as_str().to_string());
        let (freshness, eligibility) =
            derive_freshness(ctx, intent, quote.trade_date, quote.captured_at, &source);
        if eligibility.is_some() {
            // missing: 不返回行情字段
            return (
                None,
                Some(freshness),
                Some(eligibility.unwrap()),
            );
        }
        let summary = ListMarketQuoteSummary {
            trade_date: Some(quote.trade_date),
            price: quote.price.map(|p| dec_to_f64(p.0)),
            change: quote.change.map(|p| dec_to_f64(p.0)),
            change_percent: quote.change_percent,
            open: quote.open.map(|p| dec_to_f64(p.0)),
            high: quote.high.map(|p| dec_to_f64(p.0)),
            low: quote.low.map(|p| dec_to_f64(p.0)),
            previous_close: quote.previous_close.map(|p| dec_to_f64(p.0)),
            volume: quote.volume.map(|v| v.0),
            amount: quote.amount.map(|a| dec_to_f64(a.0)),
        };
        let warning = if freshness.status == FreshnessStatus::Stale {
            Some(WarningCode::QuoteStale)
        } else {
            None
        };
        (Some(summary), Some(freshness), warning)
    }

    // ====================================================================== fetch_data

    pub fn fetch_data(&self, req: FetchDataRequest) -> FetchDataResponse {
        let mut errors: Vec<ResponseError> = Vec::new();
        let mut items: Vec<FetchDataItem> = Vec::new();

        // 输入校验
        let raw_codes = req.ts_codes.unwrap_or_default();
        if raw_codes.is_empty() {
            errors.push(ResponseError::with_message(
                ErrorCode::InvalidInput,
                "tsCodes is required",
            ));
            return FetchDataResponse { errors, items };
        }
        if raw_codes.len() > 200 {
            errors.push(ResponseError::with_message(
                ErrorCode::InvalidInput,
                "tsCodes exceeds max 200",
            ));
            return FetchDataResponse { errors, items };
        }

        // 解析 + 去重
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut ts_codes: Vec<TsCode> = Vec::new();
        for raw in &raw_codes {
            match TsCode::parse(raw) {
                Ok(c) => {
                    if seen.insert(c.as_str().to_string()) {
                        ts_codes.push(c);
                    }
                }
                Err(e) => {
                    errors.push(ResponseError::with_message(
                        ErrorCode::InvalidInput,
                        e.to_string(),
                    ));
                    return FetchDataResponse { errors, items };
                }
            }
        }

        // include 默认（spec §4）
        let include = req.include.unwrap_or(FetchInclude {
            profile: Some(true),
            quote: Some(true),
            ..Default::default()
        });

        // indicators 校验
        let indicator_set = match &include.indicators {
            Some(FetchIndicators::All(true)) => Some(IndicatorName::all().to_vec()),
            Some(FetchIndicators::Subset(list)) => Some(list.clone()),
            Some(FetchIndicators::All(false)) | None => None,
        };

        let limit = req.limit.unwrap_or_default();
        let kline_limit = limit.kline.unwrap_or(120);
        let minute_limit = limit.minute_kline.unwrap_or(120);
        let events_days = limit.events_days_ahead.unwrap_or(180);

        let now = Utc::now();
        let ctx = resolve_market_time(now);
        let repo = self.repo();

        for ts_code in &ts_codes {
            let inst = match repo.get_instrument(ts_code) {
                Ok(Some(i)) => i,
                Ok(None) => {
                    items.push(FetchDataItem::missing(ts_code.clone()));
                    continue;
                }
                Err(e) => {
                    errors.push(ResponseError::with_message(ErrorCode::DbError, e.to_string()));
                    items.push(FetchDataItem::missing(ts_code.clone()));
                    continue;
                }
            };
            let mut item = FetchDataItem::new(ts_code.clone(), inst.category, Some(inst.name.clone()));
            if include.profile.unwrap_or(false) {
                item.profile = Some(StockProfile::from(&inst));
            }
            if include.quote.unwrap_or(false) {
                let (q, fresh) = self.build_full_quote(&inst, &ctx);
                if let Some(code) = fresh.warning {
                    item.warnings.push(code);
                }
                item.quote_freshness = Some(fresh);
                item.quote = q;
            }
            if include.intraday.unwrap_or(false) {
                let target_date = if ctx.is_trading_time {
                    ctx.current_trade_date.unwrap_or(ctx.latest_completed_trade_date)
                } else {
                    ctx.latest_completed_trade_date
                };
                if let Ok(Some(series)) = repo.load_intraday(ts_code, target_date) {
                    item.intraday = Some(series);
                }
            }
            if let Some(periods) = include.klines.as_ref() {
                let mut klines = std::collections::BTreeMap::new();
                for p in periods {
                    // 优先 qfq；缺则 fallback none + warning
                    let (series, used_none) = match repo.load_kline_series(ts_code, *p, AdjEnum::Qfq, kline_limit) {
                        Ok(Some(s)) => (Some(s), false),
                        _ => match repo.load_kline_series(ts_code, *p, AdjEnum::None, kline_limit) {
                            Ok(Some(s)) => (Some(s), true),
                            _ => (None, false),
                        },
                    };
                    if let Some(mut s) = series {
                        if used_none {
                            s.warnings.push(WarningCode::UsingUnadjustedKline);
                            item.warnings.push(WarningCode::UsingUnadjustedKline);
                        }
                        klines.insert(period_key(*p), s);
                    }
                }
                if !klines.is_empty() {
                    item.klines = Some(klines);
                }
            }
            if let Some(periods) = include.minute_klines.as_ref() {
                let mut minute = std::collections::BTreeMap::new();
                for p in periods {
                    if let Ok(Some(s)) = repo.load_minute_series(ts_code, *p, minute_limit) {
                        minute.insert(minute_period_key(*p), s);
                    }
                }
                if !minute.is_empty() {
                    item.minute_klines = Some(minute);
                }
            }
            if let Some(names) = indicator_set.as_ref() {
                // 用 qfq day K 现算
                if let Ok(Some(series)) = repo.load_kline_series(ts_code, KlinePeriod::Day, AdjEnum::Qfq, 200) {
                    let snap = compute_indicators(
                        ts_code.clone(),
                        IndicatorBasis {
                            period: KlinePeriod::Day,
                            adjust: AdjEnum::Qfq,
                            fetched_at: series.freshness.captured_at.unwrap_or(now),
                        },
                        &series.points,
                        names,
                        Vec::new(),
                    );
                    item.indicators = Some(snap);
                } else if let Ok(Some(series)) =
                    repo.load_kline_series(ts_code, KlinePeriod::Day, AdjEnum::None, 200)
                {
                    let mut warns = vec![WarningCode::UsingUnadjustedKline];
                    let snap = compute_indicators(
                        ts_code.clone(),
                        IndicatorBasis {
                            period: KlinePeriod::Day,
                            adjust: AdjEnum::None,
                            fetched_at: series.freshness.captured_at.unwrap_or(now),
                        },
                        &series.points,
                        names,
                        warns.clone(),
                    );
                    item.indicators = Some(snap);
                    item.warnings.append(&mut warns);
                }
            }
            if include.daily_basic.unwrap_or(false) {
                let eligible = eligible_trade_date(&ctx);
                if let Ok(Some(db)) = repo.latest_daily_basic(ts_code, eligible.trade_date) {
                    item.daily_basic = Some(db);
                } else {
                    item.warnings.push(WarningCode::DailyBasicMissing);
                }
            }
            if include.events.unwrap_or(false) {
                if let Ok(events) = repo.list_company_events(ts_code, events_days as i64) {
                    if events.is_empty() {
                        item.warnings.push(WarningCode::EventsMissing);
                    }
                    item.events = Some(events);
                }
            }
            items.push(item);
        }
        FetchDataResponse { errors, items }
    }

    fn build_full_quote(
        &self,
        inst: &MarketInstrument,
        ctx: &MarketTimeContext,
    ) -> (Option<StockQuote>, Freshness) {
        let eligible = eligible_trade_date(ctx);
        let mut cached = self.cache.get(&inst.ts_code).map(|c| c.quote);
        if cached.is_none() && !eligible.is_intraday {
            if let Ok(Some(q)) = self.repo().load_close_snapshot(&inst.ts_code, eligible.trade_date) {
                cached = Some(q);
            }
        }
        let Some(mut quote) = cached else {
            return (
                None,
                Freshness {
                    status: FreshnessStatus::Missing,
                    captured_at: None,
                    exchange_time: None,
                    age_ms: None,
                    source: None,
                    warning: Some(WarningCode::QuoteMissing),
                },
            );
        };
        let source = quote.source.as_str().to_string();
        let (freshness, eligibility) =
            derive_freshness(ctx, FreshnessIntent::Detail, quote.trade_date, quote.captured_at, &source);
        if eligibility.is_some() {
            return (None, freshness);
        }
        // 派生 limitUp / limitDown
        match (
            quote.previous_close,
            compute_limit_band(&inst.ts_code, inst.category, inst.board.as_deref(), inst.is_st.unwrap_or(false)),
        ) {
            (Some(pc), Some(band)) => {
                if let Some((up, down)) = apply_band_helper(pc, band) {
                    quote.limit_up = up;
                    quote.limit_down = down;
                }
            }
            _ => {}
        }
        // 派生 tradeStatus
        quote.trade_status = derive_trade_status(inst, ctx);
        // 五档盘口缺失 / 不完整时返回 depth_missing warning
        if quote.bid.is_empty() || quote.ask.is_empty() {
            quote.warnings.push(WarningCode::DepthMissing);
        } else if quote.bid.iter().take(1).any(|l| l.price.is_none())
            || quote.ask.iter().take(1).any(|l| l.price.is_none())
        {
            quote.warnings.push(WarningCode::DepthMissing);
        }
        // 关键价格缺失
        if quote.price.is_none() || quote.previous_close.is_none() {
            quote.warnings.push(WarningCode::QuotePriceMissing);
        }
        if matches!(freshness.status, FreshnessStatus::Stale) {
            quote.warnings.push(WarningCode::QuoteStale);
        }
        quote.freshness = freshness.clone();
        (Some(quote), freshness)
    }

    // ====================================================================== scan_market

    pub fn scan_market(&self, req: ScanMarketRequest) -> ScanMarketResponse {
        let limit = clamp(req.limit, 50, 500);
        let now = Utc::now();
        let ctx = resolve_market_time(now);
        let eligible = eligible_trade_date(&ctx);
        let repo = self.repo();
        let category = req.category;
        let (instruments, total) = repo
            .list_instruments(category, None, 100_000, 0)
            .unwrap_or_default();

        let mut valid_count: u32 = 0;
        let mut excluded_missing: u32 = 0;
        let mut excluded_expired: u32 = 0;
        let mut entries: Vec<(MarketInstrument, StockQuote, Option<DailyBasic>, Vec<WarningCode>)> =
            Vec::new();
        let mut response_warnings: Vec<WarningCode> = Vec::new();

        for inst in instruments.iter() {
            let (quote_opt, freshness) = self.build_full_quote(inst, &ctx);
            match quote_opt {
                Some(q) => {
                    if matches!(freshness.status, FreshnessStatus::Missing) {
                        if freshness.warning == Some(WarningCode::SnapshotExpired) {
                            excluded_expired += 1;
                        } else {
                            excluded_missing += 1;
                        }
                        continue;
                    }
                    valid_count += 1;
                    let db = repo.latest_daily_basic(&inst.ts_code, eligible.trade_date).ok().flatten();
                    let mut item_warns: Vec<WarningCode> = Vec::new();
                    if matches!(freshness.status, FreshnessStatus::Stale) {
                        item_warns.push(WarningCode::QuoteStale);
                    }
                    entries.push((inst.clone(), q, db, item_warns));
                }
                None => {
                    if freshness.warning == Some(WarningCode::SnapshotExpired) {
                        excluded_expired += 1;
                    } else {
                        excluded_missing += 1;
                    }
                }
            }
        }

        // 应用 filter
        let mut filtered: Vec<_> = entries
            .into_iter()
            .filter(|(_, q, _, _)| filter_passes(req.filter, q))
            .collect();

        // 应用 conditions
        let mut any_missing_condition_input = false;
        if let Some(conds) = req.conditions.as_ref() {
            filtered.retain(|(_, q, db, _)| {
                let r = conds_pass(conds, q, db.as_ref());
                if !r.matched && r.missing_input {
                    any_missing_condition_input = true;
                }
                r.matched
            });
        }
        if any_missing_condition_input {
            response_warnings.push(WarningCode::DataPartial);
        }

        // 应用 sortBy
        sort_items(req.sort_by, req.filter, &mut filtered);

        let matched_total = filtered.len() as u32;
        filtered.truncate(limit as usize);

        let items: Vec<ScanItem> = filtered
            .into_iter()
            .enumerate()
            .map(|(idx, (inst, q, db, warns))| ScanItem {
                rank: (idx + 1) as u32,
                ts_code: inst.ts_code.clone(),
                name: Some(inst.name.clone()),
                category: inst.category,
                quote: Some(q),
                daily_basic: db,
                warnings: warns,
            })
            .collect();

        ScanMarketResponse {
            result: ScanResult {
                generated_at: now,
                universe: ScanUniverse {
                    category,
                    total: total,
                    valid_quote_count: Some(valid_count),
                    excluded_missing_quote_count: Some(excluded_missing),
                    excluded_expired_quote_count: Some(excluded_expired),
                    matched: matched_total,
                },
                criteria: ScanCriteria {
                    filter: req.filter.map(filter_str),
                    conditions: req.conditions.clone(),
                    sort_by: req.sort_by.map(sort_str),
                    limit,
                },
                items,
                warnings: response_warnings,
            },
            errors: Vec::new(),
        }
    }

    // ====================================================================== refresh hooks
    //
    // 本节是 spec §4 的内部 Rust API。adapters / scheduler 调用。
    // 实现保持轻量：当前阶段聚焦在结构 + DB 写路径；远端 provider 真实拉取由 scheduler tick 调用。

    pub async fn refresh_market_instruments(&self) -> Result<(), ResponseError> {
        // tushare token 缺失时跳过
        if !self.tushare.has_token() {
            tracing::info!(target: "quotes.refresh", "tushare token missing; skip universe enrich");
            return Ok(());
        }
        let mut all_items: Vec<MarketInstrument> = Vec::new();
        match self.tushare.fetch_stock_basic().await {
            Ok(mut v) => all_items.append(&mut v),
            Err(e) => tracing::warn!(target: "quotes.refresh", error = %e, "stock_basic failed"),
        }
        for mkt in ["SSE", "SZSE"] {
            match self.tushare.fetch_index_basic(mkt).await {
                Ok(mut v) => all_items.append(&mut v),
                Err(e) => tracing::warn!(target: "quotes.refresh", market = mkt, error = %e, "index_basic failed"),
            }
        }
        match self.tushare.fetch_fund_basic().await {
            Ok(mut v) => all_items.append(&mut v),
            Err(e) => tracing::warn!(target: "quotes.refresh", error = %e, "fund_basic failed"),
        }
        if !all_items.is_empty() {
            self.repo()
                .upsert_instruments(&all_items)
                .map_err(|e| ResponseError::with_message(ErrorCode::DbError, e.to_string()))?;
        }
        Ok(())
    }

    /// 刷新指定 scope 的 quotes 到 `MARKET_SNAPSHOT`。
    pub async fn refresh_market_quotes(
        &self,
        req: RefreshMarketQuotesRequest,
    ) -> Result<MarketQuotesRefreshedPayload, ResponseError> {
        let now = Utc::now();
        let ctx = resolve_market_time(now);
        let eligible = eligible_trade_date(&ctx);
        let trade_date = req.trade_date.unwrap_or(eligible.trade_date);

        let targets: Vec<TsCode> = match &req.scope {
            RefreshScope::Subscribed => {
                let codes = req.ts_codes.clone().unwrap_or_default();
                codes
            }
            RefreshScope::Manual => {
                let codes = req.ts_codes.clone().unwrap_or_default();
                if codes.is_empty() {
                    return Err(ResponseError::with_message(
                        ErrorCode::InvalidInput,
                        "manual scope requires tsCodes",
                    ));
                }
                codes
            }
            RefreshScope::Universe => {
                let (insts, _) = self
                    .repo()
                    .list_instruments(None, None, 100_000, 0)
                    .unwrap_or_default();
                insts.into_iter().map(|i| i.ts_code).collect()
            }
        };
        let total = targets.len() as u32;
        let mut success: u32 = 0;
        let mut failed_batches: u32 = 0;
        let mut affected: Vec<TsCode> = Vec::new();

        for ts in &targets {
            let inst = match self.repo().get_instrument(ts).ok().flatten() {
                Some(i) => i,
                None => continue,
            };
            // Provider 路由：BJ → EM；SH/SZ → 先 EM 简化（TDX 协议层异步 client 完善后再切回 TDX 主源）。
            // spec §5：当前实现降级使用 Eastmoney 作主源；TDX 协议层完整接线属 follow-up。
            let outcome = self
                .eastmoney
                .fetch_quote(ts, inst.category, trade_date, now)
                .await;
            match outcome {
                Ok(q) => {
                    let captured_at = q.captured_at;
                    let snap = CachedSnapshot {
                        quote: q.clone(),
                        captured_at,
                        trade_date,
                        source: q.source.as_str().to_string(),
                    };
                    self.cache.put(snap);
                    if matches!(req.purpose, RefreshPurpose::Close) {
                        let _ = self.repo().upsert_close_snapshot(ts, trade_date, &q);
                    }
                    success += 1;
                    affected.push(ts.clone());
                }
                Err(_) => {
                    failed_batches += 1;
                    // fallback Tencent → Sina
                    if let Ok(q) = self
                        .tencent
                        .fetch_quote(ts, inst.category, trade_date, now)
                        .await
                    {
                        let snap = CachedSnapshot {
                            quote: q.clone(),
                            captured_at: q.captured_at,
                            trade_date,
                            source: "tencent".to_string(),
                        };
                        self.cache.put(snap);
                        success += 1;
                        affected.push(ts.clone());
                        continue;
                    }
                    if let Ok(q) = self
                        .sina
                        .fetch_quote(ts, inst.category, trade_date, now)
                        .await
                    {
                        let snap = CachedSnapshot {
                            quote: q.clone(),
                            captured_at: q.captured_at,
                            trade_date,
                            source: "sina".to_string(),
                        };
                        self.cache.put(snap);
                        success += 1;
                        affected.push(ts.clone());
                    }
                }
            }
        }

        if matches!(req.purpose, RefreshPurpose::Close) {
            let _ = self.repo().record_refresh_state(
                "close",
                trade_date,
                total,
                success,
                total.saturating_sub(success),
                now,
            );
        }

        let payload = MarketQuotesRefreshedPayload {
            scope: req.scope,
            purpose: req.purpose,
            trade_date: Some(trade_date),
            affected_ts_codes: if matches!(req.scope, RefreshScope::Universe) {
                None
            } else {
                Some(affected)
            },
            total,
            success,
            failed_batches,
            captured_at: now,
        };
        self.emit_refreshed(payload.clone());
        Ok(payload)
    }

    pub fn core_indexes(&self) -> Vec<TsCode> {
        core_indexes()
    }

    /// 刷新交易日历到本地（TuShare `trade_cal`）。
    pub async fn refresh_trade_calendar(
        &self,
        start_date: &str,
        end_date: &str,
    ) -> Result<u32, ResponseError> {
        if !self.tushare.has_token() {
            return Ok(0);
        }
        let entries = self
            .tushare
            .fetch_trade_cal(start_date, end_date)
            .await
            .map_err(|e| ResponseError::with_message(ErrorCode::ProviderUnavailable, e.to_string()))?;
        let rows: Vec<_> = entries
            .iter()
            .map(|e| (e.cal_date, e.is_open, e.pretrade_date))
            .collect();
        self.calendar
            .upsert_batch(&rows, "tushare", Utc::now())
            .map_err(|e| ResponseError::with_message(ErrorCode::DbError, e.to_string()))?;
        Ok(rows.len() as u32)
    }
}

// ============================================================================= helpers

fn clamp(v: Option<u32>, default: u32, max: u32) -> u32 {
    match v {
        None => default,
        Some(0) => default,
        Some(x) => x.min(max),
    }
}

fn dec_to_f64(d: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    d.to_f64().unwrap_or(f64::NAN)
}

fn period_key(p: KlinePeriod) -> String {
    p.as_str().to_string()
}

fn minute_period_key(p: MinuteKlinePeriod) -> String {
    p.as_str().to_string()
}

fn derive_trade_status(inst: &MarketInstrument, ctx: &MarketTimeContext) -> TradeStatus {
    if matches!(inst.status, Some(InstrumentStatus::Suspended)) {
        return TradeStatus::Halted;
    }
    if matches!(inst.status, Some(InstrumentStatus::Delisted)) {
        return TradeStatus::Closed;
    }
    if ctx.is_trading_time {
        TradeStatus::Trading
    } else {
        TradeStatus::Closed
    }
}

// ---------------------------------------------------------------- filter / sort

fn filter_passes(f: Option<ScanFilter>, q: &StockQuote) -> bool {
    let Some(f) = f else { return true };
    match f {
        ScanFilter::LimitUp => match (q.price, q.limit_up) {
            (Some(p), Some(up)) => p.0 == up.0,
            _ => false,
        },
        ScanFilter::LimitDown => match (q.price, q.limit_down) {
            (Some(p), Some(down)) => p.0 == down.0,
            _ => false,
        },
        ScanFilter::TopGain | ScanFilter::TopLoss => q.change_percent.is_some(),
        ScanFilter::TopAmount => q.amount.is_some(),
        ScanFilter::TopVolume => q.volume.is_some(),
    }
}

struct CondMatch {
    matched: bool,
    missing_input: bool,
}

fn conds_pass(conds: &[ScanCondition], q: &StockQuote, db: Option<&DailyBasic>) -> CondMatch {
    let mut missing_input = false;
    for c in conds {
        let v = pick_field(c.field, q, db);
        match v {
            None => {
                missing_input = true;
                return CondMatch {
                    matched: false,
                    missing_input,
                };
            }
            Some(x) => {
                if !op_matches(c.op, x, &c.value) {
                    return CondMatch {
                        matched: false,
                        missing_input,
                    };
                }
            }
        }
    }
    CondMatch {
        matched: true,
        missing_input,
    }
}

fn pick_field(f: ScanConditionField, q: &StockQuote, db: Option<&DailyBasic>) -> Option<f64> {
    match f {
        ScanConditionField::ChangePercent => q.change_percent,
        ScanConditionField::Amount => q.amount.map(|a| dec_to_f64(a.0)),
        ScanConditionField::Volume => q.volume.map(|v| v.0 as f64),
        ScanConditionField::TurnoverRate => q.turnover_rate,
        ScanConditionField::VolumeRatio => q.volume_ratio,
        ScanConditionField::PeTtm => db.and_then(|d| d.pe_ttm),
        ScanConditionField::Pb => db.and_then(|d| d.pb),
        ScanConditionField::TotalMv => db.and_then(|d| d.total_mv.map(|m| dec_to_f64(m.0))),
        ScanConditionField::CircMv => db.and_then(|d| d.circ_mv.map(|m| dec_to_f64(m.0))),
    }
}

fn op_matches(op: ScanOp, x: f64, val: &ScanConditionValue) -> bool {
    match (op, val) {
        (ScanOp::Gt, ScanConditionValue::Single(v)) => x > *v,
        (ScanOp::Gte, ScanConditionValue::Single(v)) => x >= *v,
        (ScanOp::Lt, ScanConditionValue::Single(v)) => x < *v,
        (ScanOp::Lte, ScanConditionValue::Single(v)) => x <= *v,
        (ScanOp::Eq, ScanConditionValue::Single(v)) => (x - v).abs() < 1e-9,
        (ScanOp::Between, ScanConditionValue::Range([lo, hi])) => x >= *lo && x <= *hi,
        _ => false,
    }
}

fn sort_items(
    sort_by: Option<ScanSortBy>,
    filter: Option<ScanFilter>,
    items: &mut Vec<(MarketInstrument, StockQuote, Option<DailyBasic>, Vec<WarningCode>)>,
) {
    let default_sort = filter.map(default_sort_for_filter).unwrap_or(ScanSortBy::AmountDesc);
    let sb = sort_by.unwrap_or(default_sort);
    items.sort_by(|a, b| {
        let cmp = match sb {
            ScanSortBy::ChangePctDesc => b
                .1
                .change_percent
                .unwrap_or(f64::NEG_INFINITY)
                .partial_cmp(&a.1.change_percent.unwrap_or(f64::NEG_INFINITY))
                .unwrap_or(std::cmp::Ordering::Equal),
            ScanSortBy::ChangePctAsc => a
                .1
                .change_percent
                .unwrap_or(f64::INFINITY)
                .partial_cmp(&b.1.change_percent.unwrap_or(f64::INFINITY))
                .unwrap_or(std::cmp::Ordering::Equal),
            ScanSortBy::AmountDesc => b
                .1
                .amount
                .map(|a| a.0)
                .unwrap_or(Decimal::MIN)
                .cmp(&a.1.amount.map(|a| a.0).unwrap_or(Decimal::MIN)),
            ScanSortBy::VolumeDesc => b
                .1
                .volume
                .map(|v| v.0)
                .unwrap_or(i64::MIN)
                .cmp(&a.1.volume.map(|v| v.0).unwrap_or(i64::MIN)),
            ScanSortBy::TurnoverRateDesc => b
                .1
                .turnover_rate
                .unwrap_or(f64::NEG_INFINITY)
                .partial_cmp(&a.1.turnover_rate.unwrap_or(f64::NEG_INFINITY))
                .unwrap_or(std::cmp::Ordering::Equal),
        };
        if cmp.is_eq() {
            a.0.ts_code.as_str().cmp(b.0.ts_code.as_str())
        } else {
            cmp
        }
    });
}

fn default_sort_for_filter(f: ScanFilter) -> ScanSortBy {
    match f {
        ScanFilter::LimitUp | ScanFilter::LimitDown => ScanSortBy::AmountDesc,
        ScanFilter::TopGain => ScanSortBy::ChangePctDesc,
        ScanFilter::TopLoss => ScanSortBy::ChangePctAsc,
        ScanFilter::TopAmount => ScanSortBy::AmountDesc,
        ScanFilter::TopVolume => ScanSortBy::VolumeDesc,
    }
}

fn filter_str(f: ScanFilter) -> String {
    match f {
        ScanFilter::LimitUp => "limit_up",
        ScanFilter::LimitDown => "limit_down",
        ScanFilter::TopGain => "top_gain",
        ScanFilter::TopLoss => "top_loss",
        ScanFilter::TopAmount => "top_amount",
        ScanFilter::TopVolume => "top_volume",
    }
    .to_string()
}

fn sort_str(s: ScanSortBy) -> String {
    match s {
        ScanSortBy::ChangePctDesc => "change_pct_desc",
        ScanSortBy::ChangePctAsc => "change_pct_asc",
        ScanSortBy::AmountDesc => "amount_desc",
        ScanSortBy::VolumeDesc => "volume_desc",
        ScanSortBy::TurnoverRateDesc => "turnover_rate_desc",
    }
    .to_string()
}

// ============================================================================= DTOs

#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ListMarketRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<InstrumentCategory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_quote: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ListMarketQuoteSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trade_date: Option<TradeDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_close: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ListMarketItem {
    #[serde(flatten)]
    pub instrument: MarketInstrument,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<ListMarketQuoteSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_freshness: Option<Freshness>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ListMarketPage {
    pub limit: u32,
    pub offset: u32,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ListMarketResponse {
    pub items: Vec<ListMarketItem>,
    pub page: ListMarketPage,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchDataRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_codes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<FetchInclude>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<FetchLimits>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchInclude {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intraday: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub klines: Option<Vec<KlinePeriod>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minute_klines: Option<Vec<MinuteKlinePeriod>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indicators: Option<FetchIndicators>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_basic: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(untagged)]
pub enum FetchIndicators {
    All(bool),
    Subset(Vec<IndicatorName>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchLimits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kline: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minute_kline: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events_days_ahead: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchDataItem {
    pub ts_code: TsCode,
    pub category: InstrumentCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<StockQuote>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_freshness: Option<Freshness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intraday: Option<IntradaySeries>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub klines: Option<std::collections::BTreeMap<String, KlineSeries>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minute_klines: Option<std::collections::BTreeMap<String, MinuteKlineSeries>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indicators: Option<IndicatorSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<StockProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_basic: Option<DailyBasic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<CompanyEvent>>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
}

impl FetchDataItem {
    fn new(ts_code: TsCode, category: InstrumentCategory, name: Option<String>) -> Self {
        Self {
            ts_code,
            category,
            name,
            quote: None,
            quote_freshness: None,
            intraday: None,
            klines: None,
            minute_klines: None,
            indicators: None,
            profile: None,
            daily_basic: None,
            events: None,
            warnings: Vec::new(),
        }
    }
    fn missing(ts_code: TsCode) -> Self {
        Self {
            ts_code,
            category: InstrumentCategory::Stock,
            name: None,
            quote: None,
            quote_freshness: None,
            intraday: None,
            klines: None,
            minute_klines: None,
            indicators: None,
            profile: None,
            daily_basic: None,
            events: None,
            warnings: vec![WarningCode::InstrumentMissing],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FetchDataResponse {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub errors: Vec<ResponseError>,
    pub items: Vec<FetchDataItem>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ScanMarketRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<InstrumentCategory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<ScanFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<ScanCondition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_by: Option<ScanSortBy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ScanMarketResponse {
    #[serde(flatten)]
    pub result: ScanResult,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub errors: Vec<ResponseError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RefreshMarketQuotesRequest {
    pub scope: RefreshScope,
    pub purpose: RefreshPurpose,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts_codes: Option<Vec<TsCode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trade_date: Option<TradeDate>,
}
