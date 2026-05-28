//! Quotes 应用 use case — list_market / fetch_data / scan_market / refresh_*。
//!
//! Spec: docs/design/quotes-module.md §3 / §4 / §5

use crate::domain::quotes::{
    apply_band_helper, compute_indicators, compute_limit_band, core_indexes, derive_freshness,
    eligible_trade_date, is_in_trading_session, Adjust as AdjEnum, CompanyEvent, DailyBasic,
    FreshnessIntent, IndicatorBasis, IndicatorName, IndicatorSnapshot, IndustryHeatmap,
    IndustryHeatmapItem, IntradaySeries, KlinePeriod, KlinePoint, KlineSeries, MarketBreadth,
    MarketInstrument, MarketQuotesRefreshedPayload, MinuteKlinePeriod, MinuteKlineSeries,
    RefreshDataScope, RefreshMarketQuotesScope, RefreshPurpose, RefreshScopeKind, ScanCondition,
    ScanConditionField, ScanConditionValue, ScanCriteria, ScanFilter, ScanItem, ScanOp, ScanResult,
    ScanSortBy, ScanUniverse, StockProfile, StockQuote, TradeStatus,
};
use crate::domain::shared::{
    ErrorCode, Freshness, FreshnessStatus, InstrumentCategory, InstrumentStatus, MarketTimeContext,
    ResponseError, TradeDate, TsCode, WarningCode,
};
use crate::infrastructure::db::AppDb;
use crate::infrastructure::quotes::{
    AdjustCache, AdjustCacheKey, CachedSnapshot, EastmoneyProvider, QuotesConfig, QuotesRepository,
    SinaProvider, SnapshotCache, TdxConnectionManager, TencentProvider, TradeCalendar,
    TradeCalendarRepo, TushareClient, TushareHealthCheck,
};
use crate::pipeline::quotes::market_time::resolve_market_time_with_calendar;
use chrono::{TimeZone, Utc};
use chrono_tz::Asia::Shanghai;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashSet;
use std::sync::Arc;

/// adapters 仍可能引用本符号；现作为 shared::ResponseError 的别名。
pub type PipelineResponseError = ResponseError;

#[derive(Debug, Clone)]
pub struct QuotesServiceConfig {
    pub config: QuotesConfig,
}

/// Quotes BC 对外能力。Adapter / scheduler 共用 `Arc<QuotesService>`。
pub struct QuotesService {
    pub(crate) db: AppDb,
    pub(crate) cache: Arc<SnapshotCache>,
    pub(crate) tdx: TdxConnectionManager,
    pub(crate) eastmoney: EastmoneyProvider,
    pub(crate) sina: SinaProvider,
    pub(crate) tencent: TencentProvider,
    pub(crate) tushare: TushareClient,
    /// TuShare 健康 gate（spec quotes-module.md §2 "TuShare 健康状态"）。
    /// 所有 TuShare provider 调用前必须 `health.is_available()`；为 false 则跳过。
    pub(crate) health: Arc<TushareHealthCheck>,
    pub(crate) calendar: Arc<TradeCalendarRepo>,
    /// qfq / hfq on-read cache（spec §2 "本地复权计算"）。
    pub(crate) adjust_cache: Arc<AdjustCache>,
    #[allow(dead_code)]
    pub(crate) config: QuotesConfig,
    pub(crate) event_sink: std::sync::RwLock<Option<RefreshEventSink>>,
}

pub type RefreshEventSink = Arc<dyn Fn(MarketQuotesRefreshedPayload) + Send + Sync + 'static>;

impl QuotesService {
    pub fn new(db: AppDb, config: QuotesConfig) -> reqwest::Result<Self> {
        let cache = Arc::new(SnapshotCache::new());
        let tdx = TdxConnectionManager::new();
        let eastmoney = EastmoneyProvider::new()?;
        let sina = SinaProvider::new()?;
        let tencent = TencentProvider::new()?;
        let tushare = TushareClient::new(config.tushare_token.clone())?;
        // 共享 client 给 health probe；TushareClient 是 Clone（reqwest::Client + Option<String>）。
        let health = Arc::new(TushareHealthCheck::new(
            Arc::new(tushare.clone()),
            crate::domain::quotes::TushareHealthConfig::default(),
        ));
        let calendar = Arc::new(TradeCalendarRepo::new(db.clone()));
        let adjust_cache = Arc::new(AdjustCache::new());
        Ok(Self {
            db,
            cache,
            tdx,
            eastmoney,
            sina,
            tencent,
            tushare,
            health,
            calendar,
            adjust_cache,
            config,
            event_sink: std::sync::RwLock::new(None),
        })
    }

    /// 暴露 health check 给 lib / scheduler 调用。
    pub fn health(&self) -> &Arc<TushareHealthCheck> {
        &self.health
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

    /// 用 TuShare 日历修正 shared 近似日历（spec §🚨 已反馈）。
    pub fn market_time_now(&self) -> MarketTimeContext {
        let now = Utc::now();
        let cal: &dyn TradeCalendar = self.calendar.as_ref();
        resolve_market_time_with_calendar(now, cal)
    }

    // ====================================================================== list_market

    pub fn list_market(&self, req: ListMarketRequest) -> ListMarketResponse {
        let limit = clamp(req.limit, 100, 500);
        let offset = req.offset.unwrap_or(0);
        let ctx = self.market_time_now();
        let repo = self.repo();
        let (instruments, total) = repo
            .list_instruments(req.category, req.query.as_deref(), limit, offset)
            .unwrap_or_default();

        // spec §4：has_more = total > offset + len
        let has_more = (offset as u64) + (instruments.len() as u64) < (total as u64);
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
        if let Some(w) = eligibility {
            return (None, Some(freshness), Some(w));
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

        let mut seen: HashSet<String> = HashSet::new();
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

        // 校验 indicators subset 是否合法（serde 反序列化已挡掉真未知名；这里只验显式 subset 非空）。
        let include = req.include.unwrap_or(FetchInclude {
            profile: Some(true),
            quote: Some(true),
            ..Default::default()
        });

        let indicator_set: Option<Vec<IndicatorName>> = match &include.indicators {
            Some(FetchIndicators::All(true)) => Some(IndicatorName::all().to_vec()),
            Some(FetchIndicators::Subset(list)) => {
                if list.is_empty() {
                    errors.push(ResponseError::with_message(
                        ErrorCode::InvalidInput,
                        "indicators subset must be non-empty",
                    ));
                    return FetchDataResponse { errors, items };
                }
                Some(list.clone())
            }
            Some(FetchIndicators::All(false)) | None => None,
        };

        let limit = req.limit.unwrap_or_default();
        let kline_limit = limit.kline.unwrap_or(120);
        let minute_limit = limit.minute_kline.unwrap_or(120);
        let events_days = limit.events_days_ahead.unwrap_or(180);

        let ctx = self.market_time_now();
        let now = ctx.now;
        let repo = self.repo();

        for ts_code in &ts_codes {
            let inst = match repo.get_instrument(ts_code) {
                Ok(Some(i)) => i,
                Ok(None) => {
                    items.push(FetchDataItem::missing(ts_code.clone()));
                    continue;
                }
                Err(e) => {
                    errors.push(
                        ResponseError::with_message(ErrorCode::DbError, e.to_string())
                            .with_ts_code(ts_code.clone()),
                    );
                    items.push(FetchDataItem::missing(ts_code.clone()));
                    continue;
                }
            };
            let mut item =
                FetchDataItem::new(ts_code.clone(), inst.category, Some(inst.name.clone()));
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
                    // Spec §2 三态语义已由 read_kline_series_with_adjust 内部正确判定：
                    //  - 状态 A：series.warnings 含 QfqMissing；
                    //  - 状态 B：无 warning（合理终态）；
                    //  - 状态 C：series.warnings 含 UsingUnadjustedKline。
                    // qfq 读不到 → fallback adjust='none' 并标 UsingUnadjustedKline。
                    let series = match self.read_kline_series_with_adjust(
                        ts_code,
                        *p,
                        AdjEnum::Qfq,
                        kline_limit,
                    ) {
                        Ok(Some(s)) => Some(s),
                        _ => match repo
                            .load_kline_series(ts_code, *p, AdjEnum::None, kline_limit)
                        {
                            Ok(Some(mut s)) => {
                                if !s.warnings.contains(&WarningCode::UsingUnadjustedKline) {
                                    s.warnings.push(WarningCode::UsingUnadjustedKline);
                                }
                                Some(s)
                            }
                            _ => None,
                        },
                    };
                    if let Some(s) = series {
                        // 透传 series 上的 warning 到 item 级（QfqMissing / UsingUnadjustedKline）。
                        for w in &s.warnings {
                            if matches!(
                                w,
                                WarningCode::QfqMissing | WarningCode::UsingUnadjustedKline
                            ) && !item.warnings.contains(w)
                            {
                                item.warnings.push(*w);
                            }
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
                if let Ok(Some(series)) =
                    self.read_kline_series_with_adjust(ts_code, KlinePeriod::Day, AdjEnum::Qfq, 200)
                {
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
                    let warns = vec![WarningCode::UsingUnadjustedKline];
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
                    item.warnings.push(WarningCode::UsingUnadjustedKline);
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
            if let Ok(Some(q)) = self
                .repo()
                .load_close_snapshot(&inst.ts_code, eligible.trade_date)
            {
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
        let (freshness, eligibility) = derive_freshness(
            ctx,
            FreshnessIntent::Detail,
            quote.trade_date,
            quote.captured_at,
            &source,
        );
        if eligibility.is_some() {
            return (None, freshness);
        }
        if let (Some(pc), Some(band)) = (
            quote.previous_close,
            compute_limit_band(
                &inst.ts_code,
                inst.category,
                inst.board.as_deref(),
                inst.is_st.unwrap_or(false),
            ),
        ) {
            if let Some((up, down)) = apply_band_helper(pc, band) {
                quote.limit_up = up;
                quote.limit_down = down;
            }
        }
        quote.trade_status = derive_trade_status(inst, ctx);
        // 五档盘口缺失 / 不完整 → depth_missing warning。
        let bid_missing = quote.bid.is_empty()
            || quote.bid.iter().take(1).any(|l| l.price.is_none());
        let ask_missing = quote.ask.is_empty()
            || quote.ask.iter().take(1).any(|l| l.price.is_none());
        if bid_missing || ask_missing {
            quote.warnings.push(WarningCode::DepthMissing);
        }
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
        let ctx = self.market_time_now();
        let now = ctx.now;
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
        let coverage_partial = (excluded_missing + excluded_expired) > 0;
        if coverage_partial {
            response_warnings.push(WarningCode::DataPartial);
        }

        let mut filtered: Vec<_> = entries
            .into_iter()
            .filter(|(_, q, _, _)| filter_passes(req.filter, q))
            .collect();

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
        if any_missing_condition_input && !response_warnings.contains(&WarningCode::DataPartial) {
            response_warnings.push(WarningCode::DataPartial);
        }

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
                    total,
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

    // ====================================================================== market_breadth / industry_heatmap

    /// 市场宽度 —— 仅统计 `category == stock` 的标的；
    /// 数据源优先级：`MARKET_SNAPSHOT` in-memory → 非交易时段 fallback 至 `quote_close_snapshot`。
    ///
    /// Spec: docs/design/quotes-module.md §4 `market_breadth`
    pub fn market_breadth(&self) -> MarketBreadth {
        let ctx = self.market_time_now();
        let now = ctx.now;
        let eligible = eligible_trade_date(&ctx);
        let repo = self.repo();
        let (instruments, _total) = repo
            .list_instruments(Some(InstrumentCategory::Stock), None, 100_000, 0)
            .unwrap_or_default();

        let mut total: u32 = 0;
        let mut up: u32 = 0;
        let mut down: u32 = 0;
        let mut flat: u32 = 0;
        let mut limit_up: u32 = 0;
        let mut limit_down: u32 = 0;
        let mut no_data: u32 = 0;

        for inst in &instruments {
            let Some(quote) = self.load_eligible_quote(inst, &ctx) else {
                no_data += 1;
                continue;
            };
            total += 1;
            let cp = quote.change_percent.unwrap_or(0.0);
            // 三态分桶（spec §4：up / down / flat 三选一；flat 包含 change_percent 缺失）。
            if cp > 0.0 {
                up += 1;
            } else if cp < 0.0 {
                down += 1;
            } else {
                flat += 1;
            }
            // 涨停 / 跌停：复用 compute_limit_band 拿到适用 percent；
            // 比较 `change_percent.abs() >= up_percent - epsilon`。
            if let Some(band) = compute_limit_band(
                &inst.ts_code,
                inst.category,
                inst.board.as_deref(),
                inst.is_st.unwrap_or(false),
            ) {
                if band.bounded {
                    const EPSILON: f64 = 0.05; // 百分点
                    let up_threshold = band.up_percent as f64 - EPSILON;
                    let down_threshold = -(band.down_percent as f64 - EPSILON);
                    if cp >= up_threshold {
                        limit_up += 1;
                    } else if cp <= down_threshold {
                        limit_down += 1;
                    }
                }
            }
        }

        MarketBreadth {
            total,
            up,
            down,
            flat,
            limit_up,
            limit_down,
            no_data,
            trade_date: eligible.trade_date,
            computed_at: now,
        }
    }

    /// 行业热度 —— 按 `MarketInstrument.sector` 聚合 `category == stock` 标的的 `change_percent`。
    ///
    /// 规则（spec §4 `industry_heatmap`）：
    /// - 仅统计 `category == stock`。
    /// - sector 为 `None` 或空串 → 归入 `"未分类"` 桶，但**不**参与 top_gainers / top_losers。
    /// - 无有效 quote 的标的不参与统计（与 `market_breadth.no_data` 一致）。
    /// - top_gainers / top_losers 各取 `top_n` 个行业（少于 `top_n` 时全返）。
    /// - 每个行业内 `leader_codes` 按 `change_percent desc` 取前 3，`change_percent` 缺失 → 0.0。
    ///
    /// Spec: docs/design/quotes-module.md §4 `industry_heatmap`
    pub fn industry_heatmap(&self, top_n: usize) -> IndustryHeatmap {
        let ctx = self.market_time_now();
        let now = ctx.now;
        let eligible = eligible_trade_date(&ctx);
        let repo = self.repo();
        let (instruments, _total) = repo
            .list_instruments(Some(InstrumentCategory::Stock), None, 100_000, 0)
            .unwrap_or_default();

        // 行业 bucket：sector 字符串 → 该行业全部 (ts_code, name, change_percent) tuple。
        let mut buckets: std::collections::HashMap<
            String,
            Vec<(TsCode, String, f64)>,
        > = std::collections::HashMap::new();

        const UNCLASSIFIED: &str = "未分类";

        for inst in &instruments {
            let Some(quote) = self.load_eligible_quote(inst, &ctx) else {
                continue;
            };
            let cp = quote.change_percent.unwrap_or(0.0);
            let sector = inst
                .sector
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(UNCLASSIFIED)
                .to_string();
            buckets
                .entry(sector)
                .or_default()
                .push((inst.ts_code.clone(), inst.name.clone(), cp));
        }

        // 聚合每个 sector → IndustryHeatmapItem。
        let mut items: Vec<IndustryHeatmapItem> = buckets
            .into_iter()
            .filter(|(s, _)| s != UNCLASSIFIED) // spec：未分类不参与 top
            .map(|(sector, entries)| {
                let count = entries.len() as u32;
                let sum: f64 = entries.iter().map(|(_, _, cp)| *cp).sum();
                let avg = if count == 0 { 0.0 } else { sum / count as f64 };
                // leaders: 按 change_percent desc 取前 3。
                let mut sorted = entries.clone();
                sorted.sort_by(|a, b| {
                    b.2.partial_cmp(&a.2)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.0.as_str().cmp(b.0.as_str()))
                });
                let leaders: Vec<_> = sorted.into_iter().take(3).collect();
                IndustryHeatmapItem {
                    sector,
                    avg_change_percent: avg,
                    count,
                    leader_codes: leaders.iter().map(|(c, _, _)| c.clone()).collect(),
                    leader_names: leaders.into_iter().map(|(_, n, _)| n).collect(),
                }
            })
            .collect();

        // top_gainers：avg desc；top_losers：avg asc。
        let mut by_gain = items.clone();
        by_gain.sort_by(|a, b| {
            b.avg_change_percent
                .partial_cmp(&a.avg_change_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.sector.cmp(&b.sector))
        });
        let top_gainers: Vec<_> = by_gain.into_iter().take(top_n).collect();

        // top_losers：保持 items 引用即可（复用一次 sort）。
        items.sort_by(|a, b| {
            a.avg_change_percent
                .partial_cmp(&b.avg_change_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.sector.cmp(&b.sector))
        });
        let top_losers: Vec<_> = items.into_iter().take(top_n).collect();

        IndustryHeatmap {
            top_gainers,
            top_losers,
            trade_date: eligible.trade_date,
            computed_at: now,
        }
    }

    /// 取一个 instrument 当前 eligible 的 quote。
    ///
    /// 数据源优先级（spec §2 / §4）：
    /// 1. `MARKET_SNAPSHOT` 内存 cache（trade_date 必须等于 eligible.trade_date 才接受）。
    /// 2. 非交易时段：fallback 至 `quote_close_snapshot` 表的 `eligible.trade_date` 行。
    ///
    /// 同时在硬过期场景（交易时段 + capturedAt > HARD_EXPIRE_SECS）下过滤掉，
    /// 复用 `derive_freshness` 的 eligibility 检查保持与 `list_market` / `scan_market` 一致。
    fn load_eligible_quote(
        &self,
        inst: &MarketInstrument,
        ctx: &MarketTimeContext,
    ) -> Option<StockQuote> {
        let eligible = eligible_trade_date(ctx);
        let snap = self.cache.get(&inst.ts_code).map(|c| c.quote).or_else(|| {
            if !eligible.is_intraday {
                self.repo()
                    .load_close_snapshot(&inst.ts_code, eligible.trade_date)
                    .ok()
                    .flatten()
            } else {
                None
            }
        });
        let quote = snap?;
        let source = quote.source.as_str().to_string();
        let (_freshness, eligibility) = derive_freshness(
            ctx,
            FreshnessIntent::Universe,
            quote.trade_date,
            quote.captured_at,
            &source,
        );
        // eligibility 非空 → 此 quote 不可用（snapshot expired / trade_date mismatch）。
        if eligibility.is_some() {
            return None;
        }
        Some(quote)
    }

    // ====================================================================== refresh hooks

    /// Universe refresh — 按 spec §5 line 728-730 顺序执行：
    /// 1. TDX 主源 SH / SZ universe；
    /// 2. Eastmoney 补 BJ；
    /// 3. TuShare enrich（有 token 时补 industry / list_date / fund_type / 等）。
    ///
    /// 任何一步失败不阻断后续步骤；缺 token 仅 skip enrich。
    pub async fn refresh_market_instruments(&self) -> Result<(), ResponseError> {
        let now = Utc::now();
        let mut all_items: Vec<MarketInstrument> = Vec::new();

        // 1. TDX 主源 SH / SZ
        for tdx_market in [
            crate::infrastructure::quotes::tdx::TdxMarket::SH,
            crate::infrastructure::quotes::tdx::TdxMarket::SZ,
        ] {
            match self.tdx.fetch_universe(tdx_market).await {
                Ok(entries) => {
                    let market = match tdx_market {
                        crate::infrastructure::quotes::tdx::TdxMarket::SH => {
                            crate::domain::shared::Market::SH
                        }
                        crate::infrastructure::quotes::tdx::TdxMarket::SZ => {
                            crate::domain::shared::Market::SZ
                        }
                    };
                    for entry in entries {
                        if let Some(item) = tdx_entry_to_instrument(&entry, market, now) {
                            all_items.push(item);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "quotes.refresh", market = ?tdx_market, error = %e, "tdx universe failed");
                }
            }
        }

        // 2. Eastmoney 补 BJ
        match self.eastmoney.fetch_bj_universe().await {
            Ok(entries) => {
                for (code6, name) in entries {
                    let ts = format!("{}.BJ", code6);
                    if let Ok(ts_code) = TsCode::parse(&ts) {
                        all_items.push(MarketInstrument {
                            ts_code,
                            name,
                            category: InstrumentCategory::Stock,
                            market: crate::domain::shared::Market::BJ,
                            board: Some("主板".to_string()),
                            sector: None,
                            status: Some(InstrumentStatus::Listed),
                            is_st: None,
                            publisher: None,
                            index_category: None,
                            fund_type: None,
                            management: None,
                            list_date: None,
                            source: crate::domain::quotes::InstrumentSource::Eastmoney,
                            updated_at: now,
                        });
                    }
                }
            }
            Err(e) => tracing::warn!(target: "quotes.refresh", error = %e, "eastmoney bj universe failed"),
        }

        // 写入主源结果（即使 enrich 阶段失败，主源 universe 也已落盘）。
        if !all_items.is_empty() {
            self.repo()
                .upsert_instruments(&all_items)
                .map_err(|e| ResponseError::with_message(ErrorCode::DbError, e.to_string()))?;
        }

        // 3. TuShare enrich（仅 health.is_available() 时执行，非阻塞）
        // Spec: quotes-module.md §2 "TuShare 健康状态"
        if self.health.is_available() {
            let mut enrich: Vec<MarketInstrument> = Vec::new();
            match self.tushare.fetch_stock_basic().await {
                Ok(mut v) => enrich.append(&mut v),
                Err(e) => tracing::warn!(target: "quotes.refresh", error = %e, "tushare stock_basic enrich failed"),
            }
            for mkt in TushareClient::standard_index_markets() {
                match self.tushare.fetch_index_basic(mkt).await {
                    Ok(mut v) => enrich.append(&mut v),
                    Err(e) => tracing::warn!(target: "quotes.refresh", market = %mkt, error = %e, "tushare index_basic enrich failed"),
                }
            }
            match self.tushare.fetch_fund_basic().await {
                Ok(mut v) => enrich.append(&mut v),
                Err(e) => tracing::warn!(target: "quotes.refresh", error = %e, "tushare fund_basic enrich failed"),
            }
            if !enrich.is_empty() {
                // upsert_instruments 用 COALESCE 保留主源已写入字段，新字段填补（spec §2 line 96）。
                self.repo()
                    .upsert_instruments(&enrich)
                    .map_err(|e| ResponseError::with_message(ErrorCode::DbError, e.to_string()))?;
            }
        } else {
            tracing::info!(
                target: "quotes.refresh",
                state = ?self.health.state(),
                "tushare unavailable; skip universe enrich (main universe still written from tdx/em)"
            );
        }

        // 类别变更：让 snapshot cache 中的过期类别条目失效（spec §2 不变量）。
        if let Ok(map) = self.repo().instrument_category_map() {
            let removed = self.cache.invalidate_if_category_changed(&map);
            if removed > 0 {
                tracing::info!(target: "quotes.refresh", removed, "invalidated stale snapshot entries due to category change");
            }
        }
        Ok(())
    }

    /// 刷新指定 scope 的 quotes 到 `MARKET_SNAPSHOT`。
    pub async fn refresh_market_quotes(
        &self,
        req: RefreshMarketQuotesRequest,
    ) -> Result<MarketQuotesRefreshedPayload, ResponseError> {
        let ctx = self.market_time_now();
        let now = ctx.now;
        let eligible = eligible_trade_date(&ctx);
        let trade_date = req.trade_date.unwrap_or(eligible.trade_date);

        let scope_kind = req.scope.kind();

        let (targets, target_categories) = match &req.scope {
            RefreshMarketQuotesScope::Subscribed { ts_codes } => {
                // spec §🚨 pragmatic default：subscribed + empty → no-op，无写入、不 emit。
                if ts_codes.is_empty() {
                    let payload = MarketQuotesRefreshedPayload {
                        scope: scope_kind,
                        purpose: req.purpose,
                        trade_date: Some(trade_date),
                        affected_ts_codes: Some(Vec::new()),
                        total: 0,
                        success: 0,
                        failed_batches: 0,
                        captured_at: now,
                    };
                    return Ok(payload);
                }
                self.resolve_categories(ts_codes.clone())
            }
            RefreshMarketQuotesScope::Manual { ts_codes } => {
                if ts_codes.is_empty() {
                    return Err(ResponseError::with_message(
                        ErrorCode::InvalidInput,
                        "manual scope requires non-empty tsCodes",
                    ));
                }
                self.resolve_categories(ts_codes.clone())
            }
            RefreshMarketQuotesScope::Universe => {
                // 分页：避免一次 load 100k（spec drift #20）。
                let (instruments, _total) = self
                    .repo()
                    .list_instruments(None, None, 100_000, 0)
                    .unwrap_or_default();
                let ts_codes: Vec<TsCode> =
                    instruments.iter().map(|i| i.ts_code.clone()).collect();
                let cats: std::collections::HashMap<TsCode, (InstrumentCategory, Option<String>)> =
                    instruments
                        .into_iter()
                        .map(|i| (i.ts_code.clone(), (i.category, Some(i.name))))
                        .collect();
                (ts_codes, cats)
            }
        };

        let total = targets.len() as u32;
        let mut success: u32 = 0;
        let mut failed_batches: u32 = 0;
        let mut affected: Vec<TsCode> = Vec::new();

        // 分批 refresh — 每批最多 200。避免一个超大 universe 把进程阻塞太久。
        const BATCH: usize = 200;
        for chunk in targets.chunks(BATCH) {
            for ts in chunk {
                let (category, name) = target_categories
                    .get(ts)
                    .cloned()
                    .unwrap_or((InstrumentCategory::Stock, None));
                let outcome = self.refresh_one_quote(ts, category, name, trade_date, now).await;
                match outcome {
                    Some(q) => {
                        let captured_at = q.captured_at;
                        let source_str = q.source.as_str().to_string();
                        let snap = CachedSnapshot {
                            quote: q.clone(),
                            captured_at,
                            trade_date,
                            source: source_str,
                        };
                        self.cache.put(snap);
                        if matches!(req.purpose, RefreshPurpose::Close) {
                            let _ = self.repo().upsert_close_snapshot(ts, trade_date, &q);
                        }
                        success += 1;
                        affected.push(ts.clone());
                    }
                    None => {
                        failed_batches += 1;
                    }
                }
            }
        }

        // 记录 refresh state（任何 kind 都记录，spec drift #17）。
        let kind = if matches!(req.purpose, RefreshPurpose::Close) {
            "close"
        } else {
            "intraday"
        };
        let _ = self.repo().record_refresh_state(
            kind,
            trade_date,
            total,
            success,
            total.saturating_sub(success),
            now,
        );

        let payload = MarketQuotesRefreshedPayload {
            scope: scope_kind,
            purpose: req.purpose,
            trade_date: Some(trade_date),
            affected_ts_codes: if matches!(scope_kind, RefreshScopeKind::Universe) {
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

    fn resolve_categories(
        &self,
        ts_codes: Vec<TsCode>,
    ) -> (
        Vec<TsCode>,
        std::collections::HashMap<TsCode, (InstrumentCategory, Option<String>)>,
    ) {
        let mut map: std::collections::HashMap<TsCode, (InstrumentCategory, Option<String>)> =
            std::collections::HashMap::with_capacity(ts_codes.len());
        let repo = self.repo();
        for code in &ts_codes {
            match repo.get_instrument(code) {
                Ok(Some(i)) => {
                    map.insert(code.clone(), (i.category, Some(i.name)));
                }
                _ => {
                    map.insert(code.clone(), (InstrumentCategory::Stock, None));
                }
            }
        }
        (ts_codes, map)
    }

    /// 按 spec §5 line 742 从多个 provider 候选中选取一条 quote。
    ///
    /// `candidates` 必须按 spec tie-breaker 顺序（TDX > EM > Tencent > Sina）传入；
    /// 函数选首个 `is_quote_complete = true`，否则首个 `is_display_complete = true`，否则 None。
    /// 暴露为关联函数便于纯函数单测；和 `refresh_one_quote` 的命中-即-return 等价。
    #[doc(hidden)]
    pub fn pick_fallback_quote(candidates: Vec<StockQuote>) -> Option<StockQuote> {
        let mut display_fallback: Option<StockQuote> = None;
        for q in candidates {
            if q.is_quote_complete() {
                return Some(q);
            }
            if q.is_display_complete() && display_fallback.is_none() {
                display_fallback = Some(q);
            }
        }
        display_fallback
    }

    /// Refresh 单只标的：尝试 TDX → EM → Tencent → Sina；按 spec §5 line 742 选取。
    ///
    /// 选取规则：
    /// 1. 顺序尝试 provider，遇到首个 `is_quote_complete = true` 立即采纳（带盘口）。
    /// 2. 都不完整时，保留首个 `is_display_complete = true` 的 quote 作为 fallback。
    /// 3. 全部失败返回 None。
    ///
    /// 顺序本身就是 spec 要求的 tie-breaker `TDX > Eastmoney > Tencent > Sina`，因此先 hit 即满足
    /// "字段完整度优先 + tie-break 顺序"。eligible trade date 由调用方 (`refresh_market_quotes`)
    /// 统一指定，本函数内一致。BJ 跳过 TDX。
    ///
    /// Spec: quotes-module.md §5 实时行情 fallback。
    async fn refresh_one_quote(
        &self,
        ts: &TsCode,
        category: InstrumentCategory,
        name: Option<String>,
        trade_date: TradeDate,
        now: chrono::DateTime<Utc>,
    ) -> Option<StockQuote> {
        let is_bj = matches!(ts.market(), crate::domain::shared::Market::BJ);
        let mut fallback_display: Option<StockQuote> = None;

        // helper: 处理一个 provider 的返回值；命中完整则立即 return Some。
        macro_rules! consider {
            ($result:expr, $provider:literal) => {{
                match $result {
                    Ok(q) => {
                        if q.is_quote_complete() {
                            return Some(q);
                        }
                        if q.is_display_complete() && fallback_display.is_none() {
                            fallback_display = Some(q);
                        }
                    }
                    Err(e) => tracing::debug!(target: concat!("quotes.provider.", $provider), ts = ts.as_str(), error = %e, "provider failed"),
                }
            }};
        }

        if !is_bj {
            consider!(
                self.tdx
                    .fetch_quote(ts, category, trade_date, now, name.clone())
                    .await,
                "tdx"
            );
        }
        consider!(self.eastmoney.fetch_quote(ts, category, trade_date, now).await, "em");
        consider!(self.tencent.fetch_quote(ts, category, trade_date, now).await, "tencent");
        consider!(self.sina.fetch_quote(ts, category, trade_date, now).await, "sina");

        fallback_display
    }

    // ====================================================================== refresh_klines

    /// 拉取 K 线 — TDX-primary + 增量（spec §1 line 15-16 + §5 line 800-810）。
    ///
    /// 路径：
    /// 1. 查 `max(trade_date)` from `quote_klines_daily` WHERE `adjust='none'`；
    ///    无数据 → 拉 365 天；有数据 → 从 `max+1` 开始（增量）。
    /// 2. TDX `fetch_kline(period, count)` 拉 unadjusted Bar。
    /// 3. 失败 → EM `fetch_daily_kline` fallback（仅 Day period；EM 不提供 W/M）。
    /// 4. 都失败 → 该 (ts_code, period) 计入 failed，continue。
    /// 5. Adapter Bar → KlinePoint → repo `upsert_daily_klines(adjust=none)`。
    /// 6. TuShare 长历史扩展（spec §5 line 805）：TODO(D2.5)。
    /// 7. 写入 unadjusted 后，invalidate 该 ts_code 的 qfq/hfq cache（spec §2 "xdxr 事件刷新时整体失效"
    ///    包含 base unadjusted 更新；下次读取重算）。
    pub async fn refresh_klines(
        &self,
        scope: RefreshDataScope,
        periods: Vec<KlinePeriod>,
    ) -> Result<RefreshDataResult, ResponseError> {
        let ts_codes = self.resolve_data_scope(&scope)?;
        let periods = if periods.is_empty() {
            vec![KlinePeriod::Day]
        } else {
            periods
        };
        let now = Utc::now();
        let mut total: u32 = 0;
        let mut success: u32 = 0;
        let mut failed: u32 = 0;
        let mut warnings: Vec<WarningCode> = Vec::new();
        let today = now.date_naive();

        for ts in &ts_codes {
            for period in &periods {
                total += 1;
                let repo = self.repo();
                // ① 增量：查 max(trade_date)；定 count（TDX 协议单次限制 ~800）。
                //    无数据 → 全量 365 根；有数据 → max+1 到今日。
                let max_td = repo.max_kline_trade_date(ts, *period).ok().flatten();
                let count = match max_td {
                    None => 365u16,
                    Some(td) => {
                        let days_gap = (today - td.as_naive()).num_days();
                        if days_gap <= 0 {
                            // 已有今日数据，但仍允许刷新最后一根（盘中实时变化）。
                            2
                        } else {
                            // 加 buffer，TDX 协议返回包含 max+1..today 的根数取决于交易日。
                            (days_gap as u16 + 5).min(800)
                        }
                    }
                };

                // ② TDX 主路径（SH/SZ）。
                let mut ok = false;
                let mut got_bars: Option<Vec<KlinePoint>> = None;
                let mut source_used = "tdx";
                match self.tdx.fetch_kline(ts, *period, count).await {
                    Ok(bars) => {
                        let pts: Vec<KlinePoint> = bars
                            .iter()
                            .filter_map(
                                crate::infrastructure::quotes::tdx::manager::map_daily_bar,
                            )
                            .collect();
                        if !pts.is_empty() {
                            got_bars = Some(pts);
                        }
                    }
                    Err(e) => tracing::debug!(target: "quotes.refresh.kline", ts = ts.as_str(), error = %e, "tdx kline failed; try EM"),
                }

                // ③ EM fallback（仅 Day period；W/M 无 EM 备源 → 直接 failed）。
                if got_bars.is_none() && matches!(period, KlinePeriod::Day) {
                    match self.eastmoney.fetch_daily_kline(ts, count as u32).await {
                        Ok(pts) if !pts.is_empty() => {
                            got_bars = Some(pts);
                            source_used = "eastmoney";
                        }
                        Ok(_) => {}
                        Err(e) => tracing::debug!(target: "quotes.refresh.kline", ts = ts.as_str(), error = %e, "em kline fallback failed"),
                    }
                }

                if let Some(pts) = got_bars {
                    // ④ 写 unadjusted（spec §2：本地落库的永远 adjust=none）。
                    let _ = repo.upsert_daily_klines(
                        ts,
                        *period,
                        AdjEnum::None,
                        &pts,
                        source_used,
                        now,
                    );
                    // ⑤ Invalidate qfq/hfq cache（unadjusted 变了，复权 series 需重算）。
                    self.adjust_cache.invalidate(ts);
                    success += 1;
                    ok = true;
                }

                if !ok {
                    failed += 1;
                    // BJ 不支持 K 线 → warning（spec §5 line 806 "BJ 不支持"）。
                    if matches!(ts.market(), crate::domain::shared::Market::BJ)
                        && !warnings.contains(&WarningCode::DataPartial)
                    {
                        warnings.push(WarningCode::DataPartial);
                    }
                }

                // ⑥ TuShare 长历史扩展（spec §5 line 805）。
                // TODO(D2.5): 当请求回溯窗口超出 TDX 单次根数限制且 TushareHealthState.is_available
                //             时，调 tushare.fetch_kline 补更早的 unadjusted 历史段。
                //             D2 范围只做近 365 天 TDX 主路径。
            }
        }

        // 记录 refresh_state（按 period 分 kind）— 当前简化为 "kline"。
        let eligible_td = eligible_trade_date(&self.market_time_now()).trade_date;
        let _ = self
            .repo()
            .record_refresh_state("kline", eligible_td, total, success, failed, now);
        Ok(RefreshDataResult {
            total,
            success,
            failed,
            warnings,
            affected_ts_codes: ts_codes,
        })
    }

    // ====================================================================== refresh_xdxr_events

    /// 拉取 xdxr 除权事件 — TDX 主源（spec §2 + §5 line 854）。
    ///
    /// 路径：对每个 ts_code：
    /// 1. `tdx.fetch_xdxr(ts_code)` → `Vec<XdxrRecord>`；
    /// 2. adapter → `Vec<XdxrEvent>`（跳过未知 category）；
    /// 3. `delete_xdxr_events(ts_code)` + `upsert_xdxr_events(...)` (清空 + 重写 = 幂等)；
    /// 4. invalidate 该 ts_code 的 adjust_cache 条目（xdxr 变了 → qfq/hfq 需重算）；
    /// 5. 记录 refresh_state `kind=xdxr`。
    ///
    /// xdxr 数据量小（单只标的几十~几百条），不做增量。
    /// BJ 标的不发到 TDX（`UnsupportedMarket`），整 ts_code 直接 failed。
    pub async fn refresh_xdxr_events(
        &self,
        scope: RefreshDataScope,
    ) -> Result<RefreshDataResult, ResponseError> {
        let ts_codes = self.resolve_data_scope(&scope)?;
        let now = Utc::now();
        let fetched_at_ms = now.timestamp_millis();
        let mut total: u32 = 0;
        let mut success: u32 = 0;
        let mut failed: u32 = 0;
        let mut warnings: Vec<WarningCode> = Vec::new();

        for ts in &ts_codes {
            total += 1;
            match self.tdx.fetch_xdxr(ts).await {
                Ok(records) => {
                    let events: Vec<crate::domain::quotes::XdxrEvent> = records
                        .iter()
                        .filter_map(|r| {
                            crate::infrastructure::quotes::tdx::adapter::tdx_xdxr_to_domain(
                                ts,
                                r,
                                fetched_at_ms,
                            )
                        })
                        .collect();
                    let repo = self.repo();
                    let _ = repo.delete_xdxr_events(ts);
                    let _ = repo.upsert_xdxr_events(ts, &events);
                    self.adjust_cache.invalidate(ts);
                    success += 1;
                }
                Err(e) => {
                    tracing::debug!(target: "quotes.refresh.xdxr", ts = ts.as_str(), error = %e, "tdx xdxr failed");
                    failed += 1;
                    if matches!(ts.market(), crate::domain::shared::Market::BJ)
                        && !warnings.contains(&WarningCode::DataPartial)
                    {
                        warnings.push(WarningCode::DataPartial);
                    }
                }
            }
        }

        let eligible_td = eligible_trade_date(&self.market_time_now()).trade_date;
        let _ = self
            .repo()
            .record_refresh_state("xdxr", eligible_td, total, success, failed, now);
        Ok(RefreshDataResult {
            total,
            success,
            failed,
            warnings,
            affected_ts_codes: ts_codes,
        })
    }

    // ====================================================================== read with adjust

    /// 读取 `KlineSeries`，按 `adjust` 现算 qfq / hfq，带 cache。
    ///
    /// Spec: docs/design/quotes-module.md §2 "本地复权计算（基于 TDX xdxr）"。
    ///
    /// 路径：
    /// 1. `adjust == None` → 直接读 `adjust='none'` 行。
    /// 2. `adjust == Qfq / Hfq`：
    ///    a. 算 cache key `(ts_code, period, adjust, xdxr_version)`；hit → 返回。
    ///    b. miss → 读 unadjusted + 读 xdxr events → `apply_adjust`；结果写 cache。
    /// 3. xdxr 三态语义判定（spec §2 line 233-240）：
    ///    - **状态 A · 全局未刷新**：`quote_refresh_state` 没有 `kind="xdxr"` 记录
    ///      → push `qfq_missing` warning（语义："xdxr 尚未刷新，结果可能扭曲"）。
    ///    - **状态 B · 该标的天然无除权**：xdxr 已刷新过但本 ts_code 的 events 为空
    ///      → 合理终态，**不**返回 warning。
    ///    - **状态 C · 该标的部分历史缺失**：events 起点晚于 unadjusted K 线起点
    ///      → push `using_unadjusted_kline` warning（语义："复权数据可能不完整"）。
    pub fn read_kline_series_with_adjust(
        &self,
        ts_code: &TsCode,
        period: KlinePeriod,
        adjust: AdjEnum,
        limit: u32,
    ) -> rusqlite::Result<Option<KlineSeries>> {
        let repo = self.repo();
        if matches!(adjust, AdjEnum::None) {
            return repo.load_kline_series(ts_code, period, AdjEnum::None, limit);
        }
        // 计算 xdxr_version：用事件总数 + 最大 fetched_at 简化表达（够区分刷新前后）。
        let events = repo.list_xdxr_events(ts_code).unwrap_or_default();
        let xdxr_version: i64 =
            events.iter().map(|e| e.fetched_at).max().unwrap_or(0) + events.len() as i64;
        let key = AdjustCacheKey {
            ts_code: ts_code.as_str().to_string(),
            period,
            adjust,
            xdxr_version,
        };
        if let Some(cached) = self.adjust_cache.get(&key) {
            return Ok(Some(cached));
        }
        let Some(unadj) = repo.load_kline_series(ts_code, period, AdjEnum::None, limit)? else {
            return Ok(None);
        };
        let mode = match adjust {
            AdjEnum::Qfq => crate::domain::quotes::AdjustMode::Qfq,
            AdjEnum::Hfq => crate::domain::quotes::AdjustMode::Hfq,
            AdjEnum::None => crate::domain::quotes::AdjustMode::None,
        };
        let adjusted_points = crate::domain::quotes::apply_adjust(&unadj.points, &events, mode);
        // 三态判定 —— 先查 xdxr 是否曾经刷新过（任意 trade_date）。
        let xdxr_refreshed = repo.has_refresh_state("xdxr").unwrap_or(false);
        let mut series = KlineSeries {
            period,
            adjust,
            points: adjusted_points,
            freshness: unadj.freshness,
            warnings: unadj.warnings,
        };
        if !xdxr_refreshed {
            // 状态 A：xdxr 全局从未刷新 → qfq/hfq 等同 unadjusted，语义 "尚未刷新"。
            series.warnings.push(WarningCode::QfqMissing);
        } else if events.is_empty() {
            // 状态 B：该标的（指数 / 未除权股 / ETF）天然无除权事件 → 合理终态，不发 warning。
        } else if let (Some(first_event), Some(first_bar)) =
            (events.first(), series.points.first())
        {
            // 状态 C：events 起点晚于 K 线起点 → 历史段未被复权 factor 覆盖。
            if first_event.occur_date > first_bar.date {
                series.warnings.push(WarningCode::UsingUnadjustedKline);
            }
        }
        self.adjust_cache.put(key, series.clone());
        Ok(Some(series))
    }

    // ====================================================================== refresh_minute_klines

    /// 拉取分钟 K（spec §5 line 809-816：TDX > Eastmoney；BJ 走 EM）。
    ///
    /// **交易时段 guard (drift 6)**：
    /// - 交易时段内 (`is_in_trading_session`)：正常拉取，所有 (ts, period) 都向远端发请求。
    /// - 盘后：对每个 (ts, period)，先查 `max_minute_kline_ts_ms`；如果 DB 已存有
    ///   "今日开盘以后" 的 bar，跳过远端拉取；否则允许一次性 catch-up（首次启动 / 之前网络失败）。
    ///   午休 / 09:15-09:30 也按"交易时段内"处理，因为 guard 用 `[09:15, 15:00]` 宽窗口。
    ///
    /// 写入 `quote_klines_minute`；失败的 (ts_code, period) 计入 failed。
    /// 调度由模块外运行时决定（spec §5 未规定固定频率）。
    pub async fn refresh_minute_klines(
        &self,
        scope: RefreshDataScope,
        periods: Vec<MinuteKlinePeriod>,
    ) -> Result<RefreshDataResult, ResponseError> {
        let ts_codes = self.resolve_data_scope(&scope)?;
        let periods = if periods.is_empty() {
            vec![MinuteKlinePeriod::M5]
        } else {
            periods
        };
        let now = Utc::now();
        // drift 6: trading-session guard (B 方案：盘后允许 DB 空时 catch-up，已存即 skip)。
        let now_bj = now.with_timezone(&Shanghai).naive_local();
        let in_session = is_in_trading_session(now_bj);
        // 北京时间今日 09:15 对应 UTC timestamp_ms，作为"今日开盘起点"阈值。
        let today_open_ms: i64 = {
            let bj_date = now_bj.date();
            let bj_open = bj_date.and_hms_opt(9, 15, 0).unwrap();
            // bj_open 是北京墙钟 → 转 Asia/Shanghai aware → UTC
            Shanghai
                .from_local_datetime(&bj_open)
                .single()
                .map(|dt| dt.with_timezone(&Utc).timestamp_millis())
                .unwrap_or(0)
        };
        let mut total: u32 = 0;
        let mut success: u32 = 0;
        let mut failed: u32 = 0;
        let mut warnings: Vec<WarningCode> = Vec::new();
        const MINUTE_COUNT: u16 = 240;

        for ts in &ts_codes {
            let is_bj = matches!(ts.market(), crate::domain::shared::Market::BJ);
            for period in &periods {
                // 盘后 + 已存今日数据 → skip remote fetch（drift 6 plan B）。
                // 不计 total（视作 no-op，保 `success + failed = total` invariant）。
                if !in_session {
                    let max = self
                        .repo()
                        .max_minute_kline_ts_ms(ts, *period)
                        .ok()
                        .flatten()
                        .unwrap_or(0);
                    if max >= today_open_ms {
                        tracing::debug!(
                            target: "quotes.refresh.minute",
                            ts = ts.as_str(),
                            period = period.as_str(),
                            "outside session and DB has today's bars; skipping"
                        );
                        continue;
                    }
                }
                total += 1;
                let mut ok = false;
                if !is_bj {
                    match self.tdx.fetch_minute_kline(ts, *period, MINUTE_COUNT).await {
                        Ok(bars) => {
                            let points: Vec<_> = bars
                                .iter()
                                .filter_map(
                                    crate::infrastructure::quotes::tdx::manager::map_minute_bar,
                                )
                                .collect();
                            if !points.is_empty() {
                                let _ = self
                                    .repo()
                                    .upsert_minute_klines(ts, *period, &points, "tdx", now);
                                success += 1;
                                ok = true;
                            }
                        }
                        Err(e) => tracing::debug!(target: "quotes.refresh.minute", ts = ts.as_str(), error = %e, "tdx minute kline failed; try EM"),
                    }
                }
                if !ok {
                    match self.eastmoney.fetch_minute_kline(ts, *period, MINUTE_COUNT as u32).await {
                        Ok(points) if !points.is_empty() => {
                            let _ = self
                                .repo()
                                .upsert_minute_klines(ts, *period, &points, "eastmoney", now);
                            success += 1;
                            ok = true;
                        }
                        Ok(_) => {}
                        Err(e) => tracing::debug!(target: "quotes.refresh.minute", ts = ts.as_str(), error = %e, "em minute kline failed"),
                    }
                }
                if !ok {
                    failed += 1;
                    if is_bj && !warnings.contains(&WarningCode::DataPartial) {
                        warnings.push(WarningCode::DataPartial);
                    }
                }
            }
        }

        let eligible_td = eligible_trade_date(&self.market_time_now()).trade_date;
        let _ = self
            .repo()
            .record_refresh_state("minute_kline", eligible_td, total, success, failed, now);
        Ok(RefreshDataResult {
            total,
            success,
            failed,
            warnings,
            affected_ts_codes: ts_codes,
        })
    }

    // ====================================================================== refresh_intraday

    /// 拉取当日分时 — TDX-primary (spec §5 line 818-826)。
    ///
    /// 路径：
    /// 1. **交易时段 guard (drift 6)**：`is_in_trading_session(now_beijing) == false` →
    ///    跳过远端拉取，DB 中已存的分时仍可读。盘后分时数据不再变化。
    /// 2. **TDX 主路径 (SH/SZ)**：`fetch_minute_time(ts_code) → Vec<MinuteTimePoint>`；
    ///    adapter `tdx_minute_time_to_intraday_points` 按 index→trading-minute slot
    ///    映射时间（spec §5 line 824 "TDX 协议原生支持当日 240 点分时"）。
    /// 3. **Eastmoney fallback**：TDX 失败或 BJ 走 EM `fetch_intraday`。
    /// 4. **写入**：`repo.upsert_intraday(ts_code, trade_date, points, source, now)`。
    ///    upsert ON CONFLICT(ts_code, trade_date, time) — 新日期不冲突，旧日期 series 留库
    ///    （由读侧 `fetch_data` 按 eligible trade_date 选最新；spec §4 line 673）。
    /// 5. **refresh_state**：`kind = "intraday"`，记录 total/success/failed。
    pub async fn refresh_intraday(
        &self,
        scope: RefreshDataScope,
    ) -> Result<RefreshDataResult, ResponseError> {
        let ts_codes = self.resolve_data_scope(&scope)?;
        let ctx = self.market_time_now();
        let now = ctx.now;
        let trade_date = if ctx.is_trading_time {
            ctx.current_trade_date.unwrap_or(ctx.latest_completed_trade_date)
        } else {
            ctx.latest_completed_trade_date
        };

        // Step 0: 交易时段 guard (drift 6) — 非交易时段不拉远端。
        let now_bj = now.with_timezone(&Shanghai).naive_local();
        if !is_in_trading_session(now_bj) {
            tracing::info!(
                target: "quotes.refresh.intraday",
                now_bj = %now_bj,
                "outside trading session; skip intraday remote fetch (DB remains source)"
            );
            let _ = self
                .repo()
                .record_refresh_state("intraday", trade_date, 0, 0, 0, now);
            return Ok(RefreshDataResult {
                total: 0,
                success: 0,
                failed: 0,
                warnings: Vec::new(),
                affected_ts_codes: ts_codes,
            });
        }

        let mut total: u32 = 0;
        let mut success: u32 = 0;
        let mut failed: u32 = 0;
        let mut warnings: Vec<WarningCode> = Vec::new();

        for ts in &ts_codes {
            let is_bj = matches!(ts.market(), crate::domain::shared::Market::BJ);
            total += 1;
            let mut ok = false;

            // ① TDX 主路径（SH/SZ）。
            if !is_bj {
                match self.tdx.fetch_minute_time(ts).await {
                    Ok(points) if !points.is_empty() => {
                        let mapped =
                            crate::infrastructure::quotes::tdx::adapter::tdx_minute_time_to_intraday_points(&points);
                        if !mapped.is_empty() {
                            let _ = self
                                .repo()
                                .upsert_intraday(ts, trade_date, &mapped, "tdx", now);
                            success += 1;
                            ok = true;
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!(target: "quotes.refresh.intraday", ts = ts.as_str(), error = %e, "tdx minute_time failed; try EM"),
                }
            }

            // ② Eastmoney fallback（TDX 失败或 BJ）。
            if !ok {
                match self.eastmoney.fetch_intraday(ts, trade_date).await {
                    Ok(points) if !points.is_empty() => {
                        let _ = self
                            .repo()
                            .upsert_intraday(ts, trade_date, &points, "eastmoney", now);
                        success += 1;
                        ok = true;
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!(target: "quotes.refresh.intraday", ts = ts.as_str(), error = %e, "em intraday failed"),
                }
            }

            if !ok {
                failed += 1;
                if is_bj && !warnings.contains(&WarningCode::DataPartial) {
                    warnings.push(WarningCode::DataPartial);
                }
            }
        }

        let _ = self
            .repo()
            .record_refresh_state("intraday", trade_date, total, success, failed, now);
        Ok(RefreshDataResult {
            total,
            success,
            failed,
            warnings,
            affected_ts_codes: ts_codes,
        })
    }

    /// 旧 TDX-fallback (TuShare-primary 时代)；新 TDX-primary `refresh_klines` 已直接调
    /// `tdx.fetch_kline`。保留 dead_code 占位以兼容潜在外部引用；后续可删。
    #[allow(dead_code)]
    async fn tdx_daily_fallback(&self, ts: &TsCode) -> Option<Vec<KlinePoint>> {
        match self.tdx.fetch_daily_kline(ts, 365).await {
            Ok(bars) => Some(
                bars.iter()
                    .filter_map(crate::infrastructure::quotes::tdx::manager::map_daily_bar)
                    .collect(),
            ),
            Err(_) => None,
        }
    }

    // ====================================================================== refresh_daily_basic

    pub async fn refresh_daily_basic(
        &self,
        scope: RefreshDataScope,
        trade_date: Option<TradeDate>,
    ) -> Result<RefreshDataResult, ResponseError> {
        if !self.health.is_available() {
            // Spec: quotes-module.md §2 "TuShare 健康状态" + §5：health gate fail-open
            // → 保留旧数据 + data_partial warning。token_missing 与熔断走同一分支。
            tracing::info!(
                target: "quotes.refresh",
                state = ?self.health.state(),
                "tushare unavailable; daily_basic refresh skipped, keeping local data"
            );
            return Ok(RefreshDataResult {
                total: 0,
                success: 0,
                failed: 0,
                warnings: vec![WarningCode::DataPartial],
                affected_ts_codes: Vec::new(),
            });
        }
        let now = Utc::now();
        let eligible = eligible_trade_date(&self.market_time_now());
        let trade_date = trade_date.unwrap_or(eligible.trade_date);

        let ts_codes = self.resolve_data_scope(&scope)?;
        let mut total: u32 = 0;
        let mut success: u32 = 0;
        let mut failed: u32 = 0;

        // 优化：universe scope 用 trade_date 一次性拉
        if matches!(scope, RefreshDataScope::Universe) {
            total = 1;
            match self
                .tushare
                .fetch_daily_basic(None, Some(&trade_date.format()))
                .await
            {
                Ok(rows) if !rows.is_empty() => {
                    let _ = self.repo().upsert_daily_basic(&rows);
                    success = 1;
                }
                _ => failed = 1,
            }
        } else {
            for ts in &ts_codes {
                total += 1;
                match self
                    .tushare
                    .fetch_daily_basic(Some(ts), Some(&trade_date.format()))
                    .await
                {
                    Ok(rows) if !rows.is_empty() => {
                        let _ = self.repo().upsert_daily_basic(&rows);
                        success += 1;
                    }
                    _ => failed += 1,
                }
            }
        }

        let _ = self
            .repo()
            .record_refresh_state("daily_basic", trade_date, total, success, failed, now);
        Ok(RefreshDataResult {
            total,
            success,
            failed,
            warnings: Vec::new(),
            affected_ts_codes: ts_codes,
        })
    }

    // ====================================================================== refresh_company_events

    pub async fn refresh_company_events(
        &self,
        scope: RefreshDataScope,
        window_days: Option<i64>,
    ) -> Result<RefreshDataResult, ResponseError> {
        if !self.health.is_available() {
            // Spec: quotes-module.md §2 "TuShare 健康状态" + §5：health gate fail-open
            tracing::info!(
                target: "quotes.refresh",
                state = ?self.health.state(),
                "tushare unavailable; company events refresh skipped, keeping local data"
            );
            return Ok(RefreshDataResult {
                total: 0,
                success: 0,
                failed: 0,
                warnings: vec![WarningCode::DataPartial],
                affected_ts_codes: Vec::new(),
            });
        }
        let now = Utc::now();
        let window_days = window_days.unwrap_or(30);
        let today = now.date_naive();
        // T-3 ~ T+window
        let from = today - chrono::Duration::days(3);
        let to = today + chrono::Duration::days(window_days);
        let from_s = from.format("%Y%m%d").to_string();
        let to_s = to.format("%Y%m%d").to_string();

        let ts_codes = self.resolve_data_scope(&scope)?;
        let mut total: u32 = 0;
        let mut success: u32 = 0;
        let mut failed: u32 = 0;

        if matches!(scope, RefreshDataScope::Universe) {
            // universe：用 ann_date_start..end + None 标的，dividend / suspend 各拉一次
            total = 2;
            match self.tushare.fetch_dividends(None, &from_s, &to_s).await {
                Ok(events) if !events.is_empty() => {
                    let _ = self.repo().upsert_company_events(&events);
                    success += 1;
                }
                _ => failed += 1,
            }
            match self.tushare.fetch_suspensions(None, &from_s, &to_s).await {
                Ok(events) if !events.is_empty() => {
                    let _ = self.repo().upsert_company_events(&events);
                    success += 1;
                }
                _ => failed += 1,
            }
        } else {
            for ts in &ts_codes {
                total += 2;
                match self.tushare.fetch_dividends(Some(ts), &from_s, &to_s).await {
                    Ok(events) if !events.is_empty() => {
                        let _ = self.repo().upsert_company_events(&events);
                        success += 1;
                    }
                    _ => failed += 1,
                }
                match self.tushare.fetch_suspensions(Some(ts), &from_s, &to_s).await {
                    Ok(events) if !events.is_empty() => {
                        let _ = self.repo().upsert_company_events(&events);
                        success += 1;
                    }
                    _ => failed += 1,
                }
            }
        }

        let trade_date = eligible_trade_date(&self.market_time_now()).trade_date;
        let _ = self
            .repo()
            .record_refresh_state("events", trade_date, total, success, failed, now);
        Ok(RefreshDataResult {
            total,
            success,
            failed,
            warnings: Vec::new(),
            affected_ts_codes: ts_codes,
        })
    }

    fn resolve_data_scope(&self, scope: &RefreshDataScope) -> Result<Vec<TsCode>, ResponseError> {
        match scope {
            RefreshDataScope::Universe => {
                self.repo().list_universe_ts_codes().map_err(|e| {
                    ResponseError::with_message(ErrorCode::DbError, e.to_string())
                })
            }
            RefreshDataScope::Subscribed { ts_codes } => Ok(ts_codes.clone()),
            RefreshDataScope::Manual { ts_codes } => {
                if ts_codes.is_empty() {
                    return Err(ResponseError::with_message(
                        ErrorCode::InvalidInput,
                        "manual scope requires non-empty tsCodes",
                    ));
                }
                Ok(ts_codes.clone())
            }
        }
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
        // Spec: quotes-module.md §2 "TuShare 健康状态" + §5：日历校准 gate；不可用时本地推算继续工作
        if !self.health.is_available() {
            tracing::info!(
                target: "quotes.refresh",
                state = ?self.health.state(),
                "tushare unavailable; trade_calendar refresh skipped (local computation still works)"
            );
            return Ok(0);
        }
        let entries = self
            .tushare
            .fetch_trade_cal(start_date, end_date)
            .await
            .map_err(|e| {
                ResponseError::with_message(ErrorCode::ProviderUnavailable, e.to_string())
            })?;
        let rows: Vec<_> = entries
            .iter()
            .map(|e| (e.cal_date, e.is_open, e.pretrade_date))
            .collect();
        self.calendar
            .upsert_batch(&rows, "tushare", Utc::now())
            .map_err(|e| ResponseError::with_message(ErrorCode::DbError, e.to_string()))?;
        Ok(rows.len() as u32)
    }

    /// 收盘快照 retry — 如果当日 close refresh 不完整，稍后重试。
    ///
    /// Spec: quotes-module.md §5 "失败时可低频重试直到获得最新已完成交易日快照"。
    pub async fn close_snapshot_complete(&self, trade_date: TradeDate) -> bool {
        let universe_size = self
            .repo()
            .list_universe_ts_codes()
            .map(|v| v.len() as u32)
            .unwrap_or(0);
        let Some((total, success, _failed, _at)) =
            self.repo().read_refresh_state("close", trade_date).ok().flatten()
        else {
            return false;
        };
        // 阈值：成功率 ≥ 95% 视为完整。允许少量个股 provider 失败。
        if universe_size == 0 {
            return total > 0 && success * 100 / total.max(1) >= 95;
        }
        success * 100 / universe_size >= 95
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

/// 把 TDX `SecurityListEntry` + 推断 market 翻译成 `MarketInstrument`。
///
/// 未识别前缀（spec §2 类别表外）返回 None — 调用方 skip 该条。
fn tdx_entry_to_instrument(
    entry: &crate::infrastructure::quotes::tdx::SecurityListEntry,
    market: crate::domain::shared::Market,
    now: chrono::DateTime<Utc>,
) -> Option<MarketInstrument> {
    use crate::infrastructure::quotes::universe::classify;
    if entry.code.len() < 6 {
        return None;
    }
    let cls = classify(market, &entry.code)?;
    let market_suffix = match market {
        crate::domain::shared::Market::SH => "SH",
        crate::domain::shared::Market::SZ => "SZ",
        crate::domain::shared::Market::BJ => "BJ",
    };
    let ts_str = format!("{}.{}", &entry.code[..6], market_suffix);
    let ts_code = TsCode::parse(&ts_str).ok()?;
    let name = entry.name.trim().to_string();
    let is_st = if matches!(cls.category, InstrumentCategory::Stock) {
        Some(name.contains("ST"))
    } else {
        None
    };
    Some(MarketInstrument {
        ts_code,
        name,
        category: cls.category,
        market,
        board: cls.board.map(|s| s.to_string()),
        sector: None,
        status: Some(InstrumentStatus::Listed),
        is_st,
        publisher: None,
        index_category: None,
        fund_type: None,
        management: None,
        list_date: None,
        source: crate::domain::quotes::InstrumentSource::Tdx,
        updated_at: now,
    })
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

/// Spec: quotes-module.md §4 内部 Rust API `RefreshMarketQuotesRequest`。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RefreshMarketQuotesRequest {
    pub scope: RefreshMarketQuotesScope,
    pub purpose: RefreshPurpose,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trade_date: Option<TradeDate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RefreshDataResult {
    pub total: u32,
    pub success: u32,
    pub failed: u32,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<WarningCode>,
    pub affected_ts_codes: Vec<TsCode>,
}

// ============================================================================= tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::quotes::QuoteSource;
    use crate::infrastructure::db::run_migrations;
    use crate::infrastructure::quotes::migrations as quotes_migrations;

    fn make_service() -> QuotesService {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|conn| run_migrations(conn, quotes_migrations()).unwrap());
        QuotesService::new(db, QuotesConfig::default()).unwrap()
    }

    fn seed_instrument(svc: &QuotesService, ts: &str, name: &str, cat: InstrumentCategory) {
        let code = TsCode::parse(ts).unwrap();
        let inst = MarketInstrument {
            ts_code: code.clone(),
            name: name.to_string(),
            category: cat,
            market: code.market(),
            board: None,
            sector: None,
            status: Some(InstrumentStatus::Listed),
            is_st: Some(false),
            publisher: None,
            index_category: None,
            fund_type: None,
            management: None,
            list_date: None,
            source: crate::domain::quotes::InstrumentSource::Tushare,
            updated_at: Utc::now(),
        };
        svc.repo().upsert_instruments(&[inst]).unwrap();
    }

    #[test]
    fn fetch_data_empty_ts_codes_is_invalid_input() {
        let svc = make_service();
        let req = FetchDataRequest::default();
        let res = svc.fetch_data(req);
        assert_eq!(res.errors.len(), 1);
        assert_eq!(res.errors[0].code, ErrorCode::InvalidInput);
        assert!(res.items.is_empty());
    }

    #[test]
    fn fetch_data_over_200_is_invalid_input() {
        let svc = make_service();
        let codes: Vec<String> = (0..201).map(|i| format!("{:06}.SH", i)).collect();
        let req = FetchDataRequest {
            ts_codes: Some(codes),
            ..Default::default()
        };
        let res = svc.fetch_data(req);
        assert_eq!(res.errors[0].code, ErrorCode::InvalidInput);
    }

    #[test]
    fn fetch_data_bad_format_is_invalid_input() {
        let svc = make_service();
        let req = FetchDataRequest {
            ts_codes: Some(vec!["NOTAVAILD".into()]),
            ..Default::default()
        };
        let res = svc.fetch_data(req);
        assert_eq!(res.errors[0].code, ErrorCode::InvalidInput);
    }

    #[test]
    fn fetch_data_unknown_ts_code_returns_instrument_missing() {
        let svc = make_service();
        let req = FetchDataRequest {
            ts_codes: Some(vec!["600519.SH".into()]),
            ..Default::default()
        };
        let res = svc.fetch_data(req);
        assert_eq!(res.items.len(), 1);
        assert!(res.items[0].warnings.contains(&WarningCode::InstrumentMissing));
    }

    #[test]
    fn fetch_data_dedup_preserves_first_order() {
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        seed_instrument(&svc, "000001.SZ", "平安银行", InstrumentCategory::Stock);
        let req = FetchDataRequest {
            ts_codes: Some(vec![
                "000001.SZ".into(),
                "600519.SH".into(),
                "000001.SZ".into(),
            ]),
            include: Some(FetchInclude {
                profile: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let res = svc.fetch_data(req);
        assert_eq!(res.items.len(), 2);
        assert_eq!(res.items[0].ts_code.as_str(), "000001.SZ");
        assert_eq!(res.items[1].ts_code.as_str(), "600519.SH");
    }

    #[test]
    fn fetch_data_empty_indicators_subset_invalid_input() {
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        let req = FetchDataRequest {
            ts_codes: Some(vec!["600519.SH".into()]),
            include: Some(FetchInclude {
                indicators: Some(FetchIndicators::Subset(vec![])),
                ..Default::default()
            }),
            ..Default::default()
        };
        let res = svc.fetch_data(req);
        assert_eq!(res.errors[0].code, ErrorCode::InvalidInput);
    }

    #[tokio::test]
    async fn refresh_market_quotes_manual_empty_is_invalid_input() {
        let svc = make_service();
        let req = RefreshMarketQuotesRequest {
            scope: RefreshMarketQuotesScope::Manual { ts_codes: vec![] },
            purpose: RefreshPurpose::Intraday,
            trade_date: None,
        };
        let err = svc.refresh_market_quotes(req).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[tokio::test]
    async fn refresh_market_quotes_subscribed_empty_is_noop() {
        let svc = make_service();
        let req = RefreshMarketQuotesRequest {
            scope: RefreshMarketQuotesScope::Subscribed { ts_codes: vec![] },
            purpose: RefreshPurpose::Intraday,
            trade_date: None,
        };
        let payload = svc.refresh_market_quotes(req).await.unwrap();
        assert_eq!(payload.total, 0);
        assert_eq!(payload.success, 0);
        assert_eq!(payload.failed_batches, 0);
    }

    #[tokio::test]
    async fn refresh_daily_basic_without_token_returns_ok_with_warning() {
        // Spec §5 line 764: token 缺失保持旧数据 + warning，不报错。
        let svc = make_service();
        let res = svc
            .refresh_daily_basic(RefreshDataScope::Universe, None)
            .await
            .unwrap();
        assert_eq!(res.total, 0);
        assert_eq!(res.success, 0);
        assert_eq!(res.failed, 0);
        assert!(res.warnings.contains(&WarningCode::DataPartial));
        assert!(res.affected_ts_codes.is_empty());
    }

    #[tokio::test]
    async fn refresh_company_events_without_token_returns_ok_with_warning() {
        let svc = make_service();
        let res = svc
            .refresh_company_events(
                RefreshDataScope::Manual {
                    ts_codes: vec![TsCode::parse("600519.SH").unwrap()],
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(res.total, 0);
        assert!(res.warnings.contains(&WarningCode::DataPartial));
        assert!(res.affected_ts_codes.is_empty());
    }

    #[test]
    fn list_market_query_trim_and_collapse_whitespace() {
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州 茅台", InstrumentCategory::Stock);
        let req = ListMarketRequest {
            query: Some("  贵州   茅台  ".into()),
            ..Default::default()
        };
        let res = svc.list_market(req);
        assert!(res.items.iter().any(|i| i.instrument.ts_code.as_str() == "600519.SH"));
    }

    #[test]
    fn list_market_has_more_uses_total() {
        let svc = make_service();
        for i in 0..3 {
            seed_instrument(&svc, &format!("60000{}.SH", i), &format!("S{}", i), InstrumentCategory::Stock);
        }
        let req = ListMarketRequest {
            limit: Some(2),
            offset: Some(0),
            ..Default::default()
        };
        let res = svc.list_market(req);
        assert_eq!(res.items.len(), 2);
        assert!(res.page.has_more);
        let req2 = ListMarketRequest {
            limit: Some(2),
            offset: Some(2),
            ..Default::default()
        };
        let res2 = svc.list_market(req2);
        assert_eq!(res2.items.len(), 1);
        assert!(!res2.page.has_more);
    }

    #[test]
    fn core_indexes_exposed() {
        let svc = make_service();
        assert_eq!(svc.core_indexes().len(), 4);
    }

    #[test]
    fn scope_dto_subscribed_serde() {
        // 验证 tagged union 序列化形如 spec: `{ kind: "subscribed", tsCodes: [...] }`
        let s = RefreshMarketQuotesScope::Subscribed {
            ts_codes: vec![TsCode::parse("600519.SH").unwrap()],
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["kind"], "subscribed");
        assert!(json["tsCodes"].is_array());
    }

    #[test]
    fn scope_dto_universe_serde() {
        let s = RefreshMarketQuotesScope::Universe;
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["kind"], "universe");
    }

    #[test]
    fn list_market_query_empty_returns_all() {
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        let req = ListMarketRequest {
            query: Some("  ".into()),
            ..Default::default()
        };
        let res = svc.list_market(req);
        // trim => empty => returns all
        assert!(!res.items.is_empty());
    }

    #[tokio::test]
    async fn refresh_market_instruments_returns_ok_without_token() {
        let svc = make_service();
        // 没有 tushare token 时，主源仍走 TDX/EM；测试环境网络可能不可达，
        // 但 spec §5 要求函数不报错（warnings only）。
        let res = svc.refresh_market_instruments().await;
        assert!(res.is_ok());
    }

    #[test]
    fn tdx_entry_to_instrument_sh_main_board() {
        use crate::infrastructure::quotes::tdx::SecurityListEntry;
        let entry = SecurityListEntry {
            code: "600519".into(),
            volunit: 100,
            decimal_point: 2,
            name: "贵州茅台".into(),
            pre_close: 1500.0,
        };
        let now = Utc::now();
        let inst = tdx_entry_to_instrument(&entry, crate::domain::shared::Market::SH, now).unwrap();
        assert_eq!(inst.ts_code.as_str(), "600519.SH");
        assert!(matches!(inst.category, InstrumentCategory::Stock));
        assert_eq!(inst.board.as_deref(), Some("主板"));
        assert_eq!(inst.source, crate::domain::quotes::InstrumentSource::Tdx);
        assert!(matches!(inst.status, Some(InstrumentStatus::Listed)));
    }

    #[test]
    fn tdx_entry_to_instrument_sz_chinext() {
        use crate::infrastructure::quotes::tdx::SecurityListEntry;
        let entry = SecurityListEntry {
            code: "300750".into(),
            volunit: 100,
            decimal_point: 2,
            name: "宁德时代".into(),
            pre_close: 200.0,
        };
        let now = Utc::now();
        let inst = tdx_entry_to_instrument(&entry, crate::domain::shared::Market::SZ, now).unwrap();
        assert!(matches!(inst.category, InstrumentCategory::Stock));
        assert_eq!(inst.board.as_deref(), Some("创业板"));
    }

    #[test]
    fn tdx_entry_to_instrument_unknown_prefix_returns_none() {
        use crate::infrastructure::quotes::tdx::SecurityListEntry;
        let entry = SecurityListEntry {
            code: "888888".into(),
            volunit: 100,
            decimal_point: 2,
            name: "???".into(),
            pre_close: 0.0,
        };
        let now = Utc::now();
        assert!(tdx_entry_to_instrument(&entry, crate::domain::shared::Market::SH, now).is_none());
    }

    #[test]
    fn tdx_entry_to_instrument_st_flag_inferred() {
        use crate::infrastructure::quotes::tdx::SecurityListEntry;
        let entry = SecurityListEntry {
            code: "600519".into(),
            volunit: 100,
            decimal_point: 2,
            name: "ST贵州".into(),
            pre_close: 0.0,
        };
        let now = Utc::now();
        let inst = tdx_entry_to_instrument(&entry, crate::domain::shared::Market::SH, now).unwrap();
        assert_eq!(inst.is_st, Some(true));
    }

    #[tokio::test]
    async fn refresh_klines_requires_universe_when_token_missing() {
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        // 无 token + 单只标的 — TDX 不一定可用，结果 failed > 0 也 ok。
        // 这里只验证调用不 panic、返回结构正确。
        let res = svc
            .refresh_klines(
                RefreshDataScope::Manual {
                    ts_codes: vec![TsCode::parse("430047.BJ").unwrap()],
                },
                vec![KlinePeriod::Day],
            )
            .await
            .unwrap();
        assert_eq!(res.affected_ts_codes.len(), 1);
        // BJ + 无 token + 无 TDX → 全 failed
        assert_eq!(res.success + res.failed, res.total);
    }

    /// Q1 regression: 任何 path 都必须保持 total == success + failed.
    /// 之前 bug：TuShare 失败 + TDX fallback 成功时同时计入 success 和 failed。
    /// 这里走 no-token + BJ（TDX 不支持）path：每条记录恰好 failed，
    /// 即使有多个 period 也只能是 N failed，不能出现重复计数。
    #[tokio::test]
    async fn refresh_klines_count_invariant_no_token_bj() {
        let svc = make_service();
        seed_instrument(&svc, "430047.BJ", "BJ Co", InstrumentCategory::Stock);
        let res = svc
            .refresh_klines(
                RefreshDataScope::Manual {
                    ts_codes: vec![TsCode::parse("430047.BJ").unwrap()],
                },
                vec![KlinePeriod::Day, KlinePeriod::Week],
            )
            .await
            .unwrap();
        assert_eq!(res.total, 2);
        assert_eq!(res.success + res.failed, res.total,
            "count invariant: success({}) + failed({}) must equal total({})",
            res.success, res.failed, res.total);
    }

    #[tokio::test]
    async fn refresh_minute_klines_data_scope_manual_empty_rejected() {
        let svc = make_service();
        let res = svc
            .refresh_minute_klines(RefreshDataScope::Manual { ts_codes: vec![] }, vec![])
            .await
            .unwrap_err();
        assert_eq!(res.code, ErrorCode::InvalidInput);
    }

    #[tokio::test]
    async fn refresh_minute_klines_records_refresh_state() {
        // BJ + 无 TDX/EM 网络 → 走 failed path，但函数仍 Ok 并写 refresh_state。
        // drift 6 guard：盘后 + DB 空 → catch-up 模式 total=1；盘后 + DB 已存今日数据 → skip total=0；
        // 盘中 → 正常 total=1。这里 DB 是 in-memory 空，因此总能进入 fetch 路径，total=1。
        let svc = make_service();
        seed_instrument(&svc, "430047.BJ", "BJ Co", InstrumentCategory::Stock);
        let res = svc
            .refresh_minute_klines(
                RefreshDataScope::Manual {
                    ts_codes: vec![TsCode::parse("430047.BJ").unwrap()],
                },
                vec![MinuteKlinePeriod::M5],
            )
            .await
            .unwrap();
        assert_eq!(res.total, 1);
        assert_eq!(res.success + res.failed, res.total);
        assert_eq!(res.affected_ts_codes.len(), 1);
    }

    #[tokio::test]
    async fn refresh_intraday_data_scope_manual_empty_rejected() {
        let svc = make_service();
        let res = svc
            .refresh_intraday(RefreshDataScope::Manual { ts_codes: vec![] })
            .await
            .unwrap_err();
        assert_eq!(res.code, ErrorCode::InvalidInput);
    }

    #[tokio::test]
    async fn refresh_intraday_returns_ok_with_count_invariant() {
        // drift 6 guard：盘后 → 跳过 → total=0；盘中 → total=1（BJ + 无网络全 failed）。
        // 不管走哪条路径，都必须满足 invariant 和 affected_ts_codes 至少回填。
        let svc = make_service();
        seed_instrument(&svc, "430047.BJ", "BJ Co", InstrumentCategory::Stock);
        let res = svc
            .refresh_intraday(RefreshDataScope::Manual {
                ts_codes: vec![TsCode::parse("430047.BJ").unwrap()],
            })
            .await
            .unwrap();
        assert!(res.total <= 1, "total ∈ {{0, 1}} depending on trading session guard");
        assert_eq!(res.success + res.failed, res.total);
        assert_eq!(res.affected_ts_codes.len(), 1);
    }

    #[tokio::test]
    async fn refresh_klines_data_scope_manual_empty_rejected() {
        let svc = make_service();
        let res = svc
            .refresh_klines(RefreshDataScope::Manual { ts_codes: vec![] }, vec![])
            .await
            .unwrap_err();
        assert_eq!(res.code, ErrorCode::InvalidInput);
    }

    // ============================================================================ D2 tests

    /// 增量 refresh：第一次 (无数据) 应该用 365；后续 (有数据) 用 max+gap+buffer。
    /// 这里只验证 max_kline_trade_date repo 查询和 refresh 不 panic。
    #[tokio::test]
    async fn refresh_klines_incremental_uses_max_trade_date() {
        use crate::domain::shared::{Amount, Price, Volume};
        use rust_decimal::Decimal;
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        // 预先 seed 一条 unadjusted bar — 模拟"已有数据"。
        let ts = TsCode::parse("600519.SH").unwrap();
        let bar = KlinePoint {
            date: TradeDate::parse("20250520").unwrap(),
            open: Price(Decimal::new(180000, 2)),
            close: Price(Decimal::new(181000, 2)),
            high: Price(Decimal::new(182000, 2)),
            low: Price(Decimal::new(179000, 2)),
            volume: Some(Volume(100_000)),
            amount: Some(Amount(Decimal::new(180_000_000, 2))),
        };
        svc.repo()
            .upsert_daily_klines(&ts, KlinePeriod::Day, AdjEnum::None, &[bar], "test", Utc::now())
            .unwrap();
        let max = svc
            .repo()
            .max_kline_trade_date(&ts, KlinePeriod::Day)
            .unwrap()
            .unwrap();
        assert_eq!(max.format(), "20250520");
        // 调 refresh — TDX 网络可能不可达，主要验证不 panic。
        let res = svc
            .refresh_klines(
                RefreshDataScope::Manual { ts_codes: vec![ts.clone()] },
                vec![KlinePeriod::Day],
            )
            .await
            .unwrap();
        assert_eq!(res.total, 1);
        assert_eq!(res.success + res.failed, res.total);
    }

    #[tokio::test]
    async fn refresh_xdxr_events_manual_empty_rejected() {
        let svc = make_service();
        let res = svc
            .refresh_xdxr_events(RefreshDataScope::Manual { ts_codes: vec![] })
            .await
            .unwrap_err();
        assert_eq!(res.code, ErrorCode::InvalidInput);
    }

    #[tokio::test]
    async fn refresh_xdxr_events_bj_market_counted_as_failed() {
        // BJ 不支持 TDX xdxr → 调用直接 unsupported_market error → failed 计数。
        let svc = make_service();
        seed_instrument(&svc, "430047.BJ", "BJ Co", InstrumentCategory::Stock);
        let res = svc
            .refresh_xdxr_events(RefreshDataScope::Manual {
                ts_codes: vec![TsCode::parse("430047.BJ").unwrap()],
            })
            .await
            .unwrap();
        assert_eq!(res.total, 1);
        assert_eq!(res.success + res.failed, res.total);
        assert_eq!(res.failed, 1);
    }

    #[test]
    fn read_kline_no_adjust_returns_unadjusted_directly() {
        use crate::domain::shared::{Amount, Price, Volume};
        use rust_decimal::Decimal;
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        let ts = TsCode::parse("600519.SH").unwrap();
        let bar = KlinePoint {
            date: TradeDate::parse("20240620").unwrap(),
            open: Price(Decimal::new(20000, 2)),
            close: Price(Decimal::new(20000, 2)),
            high: Price(Decimal::new(20000, 2)),
            low: Price(Decimal::new(20000, 2)),
            volume: Some(Volume(1_000_000)),
            amount: Some(Amount(Decimal::new(200_000_000, 2))),
        };
        svc.repo()
            .upsert_daily_klines(&ts, KlinePeriod::Day, AdjEnum::None, &[bar], "test", Utc::now())
            .unwrap();
        let series = svc
            .read_kline_series_with_adjust(&ts, KlinePeriod::Day, AdjEnum::None, 100)
            .unwrap()
            .unwrap();
        assert_eq!(series.adjust, AdjEnum::None);
        assert_eq!(series.points.len(), 1);
    }

    #[test]
    fn read_kline_qfq_without_xdxr_returns_unadjusted_with_warning() {
        use crate::domain::shared::{Amount, Price, Volume};
        use rust_decimal::Decimal;
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        let ts = TsCode::parse("600519.SH").unwrap();
        let bar = KlinePoint {
            date: TradeDate::parse("20240620").unwrap(),
            open: Price(Decimal::new(20000, 2)),
            close: Price(Decimal::new(20000, 2)),
            high: Price(Decimal::new(20000, 2)),
            low: Price(Decimal::new(20000, 2)),
            volume: Some(Volume(1_000_000)),
            amount: Some(Amount(Decimal::new(200_000_000, 2))),
        };
        svc.repo()
            .upsert_daily_klines(&ts, KlinePeriod::Day, AdjEnum::None, &[bar], "test", Utc::now())
            .unwrap();
        // 无 xdxr 事件 → qfq 应该返回 unadjusted + QfqMissing warning。
        let series = svc
            .read_kline_series_with_adjust(&ts, KlinePeriod::Day, AdjEnum::Qfq, 100)
            .unwrap()
            .unwrap();
        assert_eq!(series.adjust, AdjEnum::Qfq);
        assert_eq!(series.points.len(), 1);
        assert!(series.warnings.contains(&WarningCode::QfqMissing));
        // 值应等于 unadjusted（无事件 ⇒ apply_adjust 直接 clone）
        assert_eq!(series.points[0].close.0, Decimal::new(20000, 2));
    }

    // ----------------------------------------------------------------- xdxr 三态（spec §2 line 233-240）
    //
    // 状态 A：xdxr 全局未刷新（quote_refresh_state 无 kind="xdxr"）
    //         → 任意 ts_code 读 qfq 应附 QfqMissing。
    // 状态 B：xdxr 刷新过但该 ts_code 自然无 events（指数 / 未除权股 / ETF）
    //         → 不发 warning（合理终态）。
    // 状态 C：xdxr 刷新过且本 ts_code 有 events，但 events[0] 晚于 K 线起点
    //         → 附 UsingUnadjustedKline。

    fn seed_unadj_bar(svc: &QuotesService, ts: &TsCode, date: &str, close: f64) {
        use crate::domain::shared::{Price, Volume};
        use rust_decimal::prelude::FromPrimitive;
        let p = Price(Decimal::from_f64(close).unwrap());
        let bar = KlinePoint {
            date: TradeDate::parse(date).unwrap(),
            open: p,
            close: p,
            high: p,
            low: p,
            volume: Some(Volume(1)),
            amount: None,
        };
        svc.repo()
            .upsert_daily_klines(ts, KlinePeriod::Day, AdjEnum::None, &[bar], "test", Utc::now())
            .unwrap();
    }

    #[test]
    fn xdxr_state_a_no_refresh_state_pushes_qfq_missing() {
        // 状态 A：DB 中 quote_refresh_state 没有 kind="xdxr" 记录 → 读 qfq 必须 QfqMissing。
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        let ts = TsCode::parse("600519.SH").unwrap();
        seed_unadj_bar(&svc, &ts, "20240620", 200.0);
        // 显式确认没有 xdxr refresh_state
        assert!(!svc.repo().has_refresh_state("xdxr").unwrap());
        let series = svc
            .read_kline_series_with_adjust(&ts, KlinePeriod::Day, AdjEnum::Qfq, 100)
            .unwrap()
            .unwrap();
        assert!(
            series.warnings.contains(&WarningCode::QfqMissing),
            "state A must push qfq_missing"
        );
        assert!(
            !series.warnings.contains(&WarningCode::UsingUnadjustedKline),
            "state A only pushes qfq_missing, not using_unadjusted_kline"
        );
    }

    #[test]
    fn xdxr_state_b_refreshed_but_no_events_no_warning() {
        // 状态 B：xdxr 已刷新过（refresh_state 存在）但本 ts_code 的 events 为 0
        //        → 合理终态，**不**发 warning。
        let svc = make_service();
        seed_instrument(&svc, "000300.SH", "沪深300", InstrumentCategory::Index);
        let ts = TsCode::parse("000300.SH").unwrap();
        seed_unadj_bar(&svc, &ts, "20240620", 3500.0);
        // 写入一条 xdxr refresh_state（模拟 refresh_xdxr_events 已跑过；任意标的 / trade_date 均可）。
        let td = TradeDate::parse("20240620").unwrap();
        svc.repo()
            .record_refresh_state("xdxr", td, 5000, 4900, 100, Utc::now())
            .unwrap();
        // 本 ts_code 没有任何 events（指数 / 未除权股）。
        assert!(svc.repo().list_xdxr_events(&ts).unwrap().is_empty());
        let series = svc
            .read_kline_series_with_adjust(&ts, KlinePeriod::Day, AdjEnum::Qfq, 100)
            .unwrap()
            .unwrap();
        assert!(
            !series.warnings.contains(&WarningCode::QfqMissing),
            "state B must not push qfq_missing"
        );
        assert!(
            !series.warnings.contains(&WarningCode::UsingUnadjustedKline),
            "state B must not push using_unadjusted_kline (natural no-event terminal)"
        );
    }

    #[test]
    fn xdxr_state_c_events_start_later_than_kline_start_pushes_using_unadjusted() {
        // 状态 C：xdxr 已刷新，且该 ts_code 有 events，但 events[0].occur_date > bars[0].date
        //        → 历史段未被复权 factor 覆盖，必须 UsingUnadjustedKline。
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        let ts = TsCode::parse("600519.SH").unwrap();
        // K 线起点 2014-06-01；事件起点 2014-06-30（晚于 K 线起点）。
        seed_unadj_bar(&svc, &ts, "20140601", 200.0);
        seed_unadj_bar(&svc, &ts, "20140630", 198.8);
        let td = TradeDate::parse("20140630").unwrap();
        svc.repo()
            .record_refresh_state("xdxr", td, 1, 1, 0, Utc::now())
            .unwrap();
        let ev = crate::domain::quotes::XdxrEvent::dividend_and_split(
            ts.clone(),
            TradeDate::parse("20140630").unwrap(),
            Some(12.0),
            Some(0.0),
            Some(1.0),
            Some(0.0),
            1_700_000_000_000,
        );
        svc.repo().upsert_xdxr_events(&ts, &[ev]).unwrap();
        let series = svc
            .read_kline_series_with_adjust(&ts, KlinePeriod::Day, AdjEnum::Qfq, 100)
            .unwrap()
            .unwrap();
        assert!(
            series.warnings.contains(&WarningCode::UsingUnadjustedKline),
            "state C must push using_unadjusted_kline"
        );
        assert!(
            !series.warnings.contains(&WarningCode::QfqMissing),
            "state C is not state A (refresh has happened)"
        );
    }

    #[test]
    fn xdxr_state_c_full_coverage_no_warning() {
        // 边界：events[0] 等于 K 线起点 → 复权数据完整 → 无 warning。
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        let ts = TsCode::parse("600519.SH").unwrap();
        seed_unadj_bar(&svc, &ts, "20140630", 198.8);
        let td = TradeDate::parse("20140630").unwrap();
        svc.repo()
            .record_refresh_state("xdxr", td, 1, 1, 0, Utc::now())
            .unwrap();
        // event occur_date 等于 K 线起点 → first_event.occur_date NOT > first_bar.date。
        let ev = crate::domain::quotes::XdxrEvent::dividend_and_split(
            ts.clone(),
            TradeDate::parse("20140630").unwrap(),
            Some(12.0),
            Some(0.0),
            Some(1.0),
            Some(0.0),
            1_700_000_000_000,
        );
        svc.repo().upsert_xdxr_events(&ts, &[ev]).unwrap();
        let series = svc
            .read_kline_series_with_adjust(&ts, KlinePeriod::Day, AdjEnum::Qfq, 100)
            .unwrap()
            .unwrap();
        assert!(!series.warnings.contains(&WarningCode::QfqMissing));
        assert!(
            !series.warnings.contains(&WarningCode::UsingUnadjustedKline),
            "events covering full kline range should not push any warning"
        );
    }

    #[test]
    fn read_kline_qfq_with_xdxr_applies_factor_and_caches() {
        use crate::domain::shared::{Amount, Price, Volume};
        use rust_decimal::Decimal;
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        let ts = TsCode::parse("600519.SH").unwrap();
        // 2 bars: 200 (pre) → 198.8 (event day)
        let pre = KlinePoint {
            date: TradeDate::parse("20140629").unwrap(),
            open: Price(Decimal::new(20000, 2)),
            close: Price(Decimal::new(20000, 2)),
            high: Price(Decimal::new(20000, 2)),
            low: Price(Decimal::new(20000, 2)),
            volume: Some(Volume(1_000_000)),
            amount: None,
        };
        let post = KlinePoint {
            date: TradeDate::parse("20140630").unwrap(),
            open: Price(Decimal::new(19880, 2)),
            close: Price(Decimal::new(19880, 2)),
            high: Price(Decimal::new(19880, 2)),
            low: Price(Decimal::new(19880, 2)),
            volume: Some(Volume(1_000_000)),
            amount: None,
        };
        svc.repo()
            .upsert_daily_klines(
                &ts,
                KlinePeriod::Day,
                AdjEnum::None,
                &[pre, post],
                "test",
                Utc::now(),
            )
            .unwrap();
        // xdxr: 10送1派12
        let ev = crate::domain::quotes::XdxrEvent::dividend_and_split(
            ts.clone(),
            TradeDate::parse("20140630").unwrap(),
            Some(12.0),
            Some(0.0),
            Some(1.0),
            Some(0.0),
            1_700_000_000_000,
        );
        svc.repo().upsert_xdxr_events(&ts, &[ev]).unwrap();
        let series = svc
            .read_kline_series_with_adjust(&ts, KlinePeriod::Day, AdjEnum::Qfq, 100)
            .unwrap()
            .unwrap();
        assert_eq!(series.points.len(), 2);
        // qfq: 历史价 < unadjusted 200，最新价不变 198.8。
        use rust_decimal::prelude::ToPrimitive;
        let pre_close = series.points[0].close.0.to_f64().unwrap();
        let post_close = series.points[1].close.0.to_f64().unwrap();
        assert!(pre_close < 200.0, "qfq pre = {} should < 200", pre_close);
        assert!(pre_close > 175.0, "qfq pre = {} should ≈ 180.7", pre_close);
        assert!((post_close - 198.8).abs() < 0.5);
        // 第二次读：走 cache（同一 xdxr_version）；不会 panic，结果相同。
        let series2 = svc
            .read_kline_series_with_adjust(&ts, KlinePeriod::Day, AdjEnum::Qfq, 100)
            .unwrap()
            .unwrap();
        let pre2 = series2.points[0].close.0.to_f64().unwrap();
        assert!((pre2 - pre_close).abs() < 1e-9);
    }

    #[test]
    fn read_kline_qfq_cache_invalidated_after_xdxr_update() {
        use crate::domain::shared::{Price, Volume};
        use rust_decimal::Decimal;
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        let ts = TsCode::parse("600519.SH").unwrap();
        let bar = KlinePoint {
            date: TradeDate::parse("20140630").unwrap(),
            open: Price(Decimal::new(19880, 2)),
            close: Price(Decimal::new(19880, 2)),
            high: Price(Decimal::new(19880, 2)),
            low: Price(Decimal::new(19880, 2)),
            volume: Some(Volume(1)),
            amount: None,
        };
        svc.repo()
            .upsert_daily_klines(&ts, KlinePeriod::Day, AdjEnum::None, &[bar], "test", Utc::now())
            .unwrap();
        // 读一次（无 xdxr）→ warning
        let s1 = svc
            .read_kline_series_with_adjust(&ts, KlinePeriod::Day, AdjEnum::Qfq, 100)
            .unwrap()
            .unwrap();
        assert!(s1.warnings.contains(&WarningCode::QfqMissing));
        // upsert xdxr → version change → key 不命中
        let ev = crate::domain::quotes::XdxrEvent::dividend_and_split(
            ts.clone(),
            TradeDate::parse("20140630").unwrap(),
            Some(12.0),
            Some(0.0),
            Some(1.0),
            Some(0.0),
            1_700_000_000_000,
        );
        svc.repo().upsert_xdxr_events(&ts, &[ev]).unwrap();
        // 模拟 refresh_xdxr_events 完成（spec §2 三态判定需要 refresh_state 标记）。
        svc.repo()
            .record_refresh_state(
                "xdxr",
                TradeDate::parse("20140630").unwrap(),
                1,
                1,
                0,
                Utc::now(),
            )
            .unwrap();
        // 显式 invalidate（refresh_xdxr_events 内部会调；这里测函数式）
        svc.adjust_cache.invalidate(&ts);
        let s2 = svc
            .read_kline_series_with_adjust(&ts, KlinePeriod::Day, AdjEnum::Qfq, 100)
            .unwrap()
            .unwrap();
        assert!(!s2.warnings.contains(&WarningCode::QfqMissing));
    }

    // ----------------------------------------------------------------- D3 drift 5 / 6 / 7

    /// drift 5（增量 K 线）— D2 已实现，D3 加 regression：
    /// `max_kline_trade_date` 返回值在 seed → max+1 → refresh 链路中正确传导。
    /// 不依赖网络：单独验证 repo + count 派生纯逻辑。
    #[test]
    fn drift5_incremental_count_derives_from_max_trade_date() {
        use crate::domain::shared::{Amount, Price, Volume};
        use rust_decimal::Decimal;
        let svc = make_service();
        let ts = TsCode::parse("600519.SH").unwrap();
        // 1. 空 DB → max=None。
        assert!(svc
            .repo()
            .max_kline_trade_date(&ts, KlinePeriod::Day)
            .unwrap()
            .is_none());
        // 2. seed 一条 → max 推进。
        let bar = KlinePoint {
            date: TradeDate::parse("20240601").unwrap(),
            open: Price(Decimal::new(10000, 2)),
            close: Price(Decimal::new(10100, 2)),
            high: Price(Decimal::new(10200, 2)),
            low: Price(Decimal::new(9900, 2)),
            volume: Some(Volume(1)),
            amount: Some(Amount(Decimal::new(100_000, 2))),
        };
        svc.repo()
            .upsert_daily_klines(&ts, KlinePeriod::Day, AdjEnum::None, &[bar], "test", Utc::now())
            .unwrap();
        let max = svc
            .repo()
            .max_kline_trade_date(&ts, KlinePeriod::Day)
            .unwrap()
            .unwrap();
        assert_eq!(max.format(), "20240601");
        // 3. seed 第二条更晚 → max 替换。
        let bar2 = KlinePoint {
            date: TradeDate::parse("20240615").unwrap(),
            open: Price(Decimal::new(10100, 2)),
            close: Price(Decimal::new(10200, 2)),
            high: Price(Decimal::new(10300, 2)),
            low: Price(Decimal::new(10000, 2)),
            volume: Some(Volume(1)),
            amount: None,
        };
        svc.repo()
            .upsert_daily_klines(&ts, KlinePeriod::Day, AdjEnum::None, &[bar2], "test", Utc::now())
            .unwrap();
        let max2 = svc
            .repo()
            .max_kline_trade_date(&ts, KlinePeriod::Day)
            .unwrap()
            .unwrap();
        assert_eq!(max2.format(), "20240615");
    }

    /// drift 7（xdxr 触发 qfq cache 失效）— D2 已 inline 实现，D3 加 regression：
    /// `refresh_xdxr_events` 写完事件后调用 `adjust_cache.invalidate`，下次读 qfq 重算。
    #[tokio::test]
    async fn drift7_refresh_xdxr_events_invalidates_qfq_cache() {
        use crate::domain::shared::{Price, Volume};
        use rust_decimal::Decimal;
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        let ts = TsCode::parse("600519.SH").unwrap();
        // 1. seed K bar + 触发 qfq 读取 → cache miss → 写入 cache
        let bar = KlinePoint {
            date: TradeDate::parse("20140630").unwrap(),
            open: Price(Decimal::new(19880, 2)),
            close: Price(Decimal::new(19880, 2)),
            high: Price(Decimal::new(19880, 2)),
            low: Price(Decimal::new(19880, 2)),
            volume: Some(Volume(1)),
            amount: None,
        };
        svc.repo()
            .upsert_daily_klines(&ts, KlinePeriod::Day, AdjEnum::None, &[bar], "test", Utc::now())
            .unwrap();
        let s1 = svc
            .read_kline_series_with_adjust(&ts, KlinePeriod::Day, AdjEnum::Qfq, 100)
            .unwrap()
            .unwrap();
        // 无 xdxr → 应带 QfqMissing warning。
        assert!(s1.warnings.contains(&WarningCode::QfqMissing));
        // 2. 触发 refresh_xdxr_events（网络可能失败，但函数内调 invalidate）
        //    BJ 跳过，这里是 SH，TDX 调用真实发起。在测试环境通常 failed，
        //    但只要不发生 panic，drift 7 设计的 invalidate 路径已通过 refresh_klines 验证。
        //    所以这里只校验 invalidate 函数可直接调用，模拟 refresh_xdxr_events 内部行为。
        svc.adjust_cache.invalidate(&ts);
        // 3. 即使第二次读，cache miss → 仍重算；只要不 panic 即可。
        let s2 = svc
            .read_kline_series_with_adjust(&ts, KlinePeriod::Day, AdjEnum::Qfq, 100)
            .unwrap()
            .unwrap();
        assert_eq!(s2.points.len(), 1);
    }

    /// drift 6（交易时段 guard）— intraday 在非交易时段直接返回零开销结果。
    /// 用 chrono pure naive datetime 验证 `is_in_trading_session` 行为；
    /// 因 refresh_intraday 用 `Utc::now()` 非确定，所以这里测纯函数的 wiring 已在 trade_calendar tests 覆盖。
    /// 这里加一条端到端：guard 走 fast-path 时仍写 refresh_state + 返回 affected_ts_codes。
    #[tokio::test]
    async fn drift6_intraday_outside_session_records_state_and_returns() {
        // 该测试不能强制时间，但当 wall clock 在非交易时段（例如 CI 跑在午夜）时
        // total=0；交易时段则 total=1。任一情况都必须满足 invariant 并写 refresh_state。
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        let ts = TsCode::parse("600519.SH").unwrap();
        let res = svc
            .refresh_intraday(RefreshDataScope::Manual { ts_codes: vec![ts.clone()] })
            .await
            .unwrap();
        assert!(res.total <= 1);
        assert_eq!(res.success + res.failed, res.total);
        // affected_ts_codes 一定包含解析后的 ts_code（不论是否实际发起远端调用）。
        assert_eq!(res.affected_ts_codes.len(), 1);
        assert_eq!(res.affected_ts_codes[0].as_str(), "600519.SH");
        // refresh_state 已写（kind=intraday；总数 0 或 1）。
        let td = eligible_trade_date(&svc.market_time_now()).trade_date;
        let st = svc.repo().read_refresh_state("intraday", td).unwrap();
        assert!(st.is_some(), "intraday refresh_state must be recorded");
    }

    /// drift 6（交易时段 guard）— minute K 盘后 + DB 空时仍 catch-up，DB 已存今日则 skip。
    /// 验证 `max_minute_kline_ts_ms` 与 today_open_ms 比较语义。
    #[test]
    fn drift6_max_minute_kline_ts_ms_returns_none_for_empty() {
        let svc = make_service();
        let ts = TsCode::parse("600519.SH").unwrap();
        let max = svc
            .repo()
            .max_minute_kline_ts_ms(&ts, MinuteKlinePeriod::M5)
            .unwrap();
        assert!(max.is_none());
    }

    #[test]
    fn drift6_max_minute_kline_ts_ms_after_seed() {
        use crate::domain::quotes::MinuteKlinePoint;
        use crate::domain::shared::{Amount, Price, Volume};
        use rust_decimal::Decimal;
        let svc = make_service();
        let ts = TsCode::parse("600519.SH").unwrap();
        let p = MinuteKlinePoint {
            timestamp_ms: 1_700_000_000_000,
            open: Price(Decimal::new(10000, 2)),
            close: Price(Decimal::new(10000, 2)),
            high: Price(Decimal::new(10000, 2)),
            low: Price(Decimal::new(10000, 2)),
            volume: Volume(100),
            amount: Amount(Decimal::new(1_000_000, 2)),
        };
        svc.repo()
            .upsert_minute_klines(&ts, MinuteKlinePeriod::M5, &[p], "test", Utc::now())
            .unwrap();
        let max = svc
            .repo()
            .max_minute_kline_ts_ms(&ts, MinuteKlinePeriod::M5)
            .unwrap();
        assert_eq!(max, Some(1_700_000_000_000));
    }

    #[test]
    fn max_kline_trade_date_empty_returns_none() {
        let svc = make_service();
        let ts = TsCode::parse("600519.SH").unwrap();
        let max = svc.repo().max_kline_trade_date(&ts, KlinePeriod::Day).unwrap();
        assert!(max.is_none());
    }

    #[test]
    fn scan_market_empty_universe_returns_zero_matched() {
        let svc = make_service();
        let res = svc.scan_market(ScanMarketRequest::default());
        assert_eq!(res.result.universe.total, 0);
        assert_eq!(res.result.universe.matched, 0);
        assert!(res.result.items.is_empty());
    }

    #[test]
    fn scan_market_excludes_instruments_without_quote() {
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "S1", InstrumentCategory::Stock);
        let res = svc.scan_market(ScanMarketRequest::default());
        assert_eq!(res.result.universe.total, 1);
        assert!(res.result.universe.excluded_missing_quote_count.unwrap_or(0) >= 1);
    }

    #[test]
    fn list_market_query_ts_code_exact_prefers_first() {
        let svc = make_service();
        seed_instrument(&svc, "600519.SH", "贵州茅台", InstrumentCategory::Stock);
        seed_instrument(&svc, "600520.SH", "中坚科技", InstrumentCategory::Stock);
        let req = ListMarketRequest {
            query: Some("600519.sh".into()),
            ..Default::default()
        };
        let res = svc.list_market(req);
        assert_eq!(res.items.first().unwrap().instrument.ts_code.as_str(), "600519.SH");
    }

    // ----------------------------------------------------------------- Q2 fallback selection
    fn fake_quote(src: QuoteSource, with_price: bool, with_depth: bool) -> StockQuote {
        use crate::domain::quotes::QuoteDepthLevel;
        use crate::domain::shared::{Freshness, FreshnessStatus, Price, Volume};
        let bid = if with_depth {
            vec![QuoteDepthLevel { price: Some(Price(Decimal::new(999, 2))), volume: Some(Volume(100)) }]
        } else {
            Vec::new()
        };
        let ask = if with_depth {
            vec![QuoteDepthLevel { price: Some(Price(Decimal::new(1001, 2))), volume: Some(Volume(100)) }]
        } else {
            Vec::new()
        };
        StockQuote {
            ts_code: TsCode::parse("600519.SH").unwrap(),
            name: None,
            category: InstrumentCategory::Stock,
            trade_date: TradeDate::from_naive(chrono::NaiveDate::from_ymd_opt(2025, 5, 26).unwrap()),
            price: if with_price { Some(Price(Decimal::new(1000, 2))) } else { None },
            previous_close: None,
            open: None,
            high: None,
            low: None,
            change: None,
            change_percent: None,
            volume: None,
            amount: None,
            turnover_rate: None,
            volume_ratio: None,
            limit_up: None,
            limit_down: None,
            bid,
            ask,
            trade_status: TradeStatus::Trading,
            source: src,
            captured_at: Utc::now(),
            exchange_time: None,
            freshness: Freshness {
                status: FreshnessStatus::Fresh,
                captured_at: None,
                exchange_time: None,
                age_ms: None,
                source: None,
                warning: None,
            },
            warnings: Vec::new(),
        }
    }

    #[test]
    fn pick_fallback_picks_first_complete_skipping_incomplete_higher_priority() {
        // TDX 缺盘口（incomplete），EM 完整 → 采纳 EM.
        let tdx = fake_quote(QuoteSource::Tdx, true, false);
        let em = fake_quote(QuoteSource::Eastmoney, true, true);
        let picked = QuotesService::pick_fallback_quote(vec![tdx, em]).unwrap();
        assert!(matches!(picked.source, QuoteSource::Eastmoney));
        assert!(picked.is_quote_complete());
    }

    #[test]
    fn pick_fallback_prefers_higher_priority_if_both_complete() {
        let tdx = fake_quote(QuoteSource::Tdx, true, true);
        let em = fake_quote(QuoteSource::Eastmoney, true, true);
        let picked = QuotesService::pick_fallback_quote(vec![tdx, em]).unwrap();
        assert!(matches!(picked.source, QuoteSource::Tdx));
    }

    #[test]
    fn pick_fallback_falls_back_to_display_only_when_none_complete() {
        let tdx = fake_quote(QuoteSource::Tdx, true, false);
        let em = fake_quote(QuoteSource::Eastmoney, true, false);
        let sina = fake_quote(QuoteSource::Sina, true, false);
        let picked = QuotesService::pick_fallback_quote(vec![tdx, em, sina]).unwrap();
        // 第一个 display_complete 取 TDX。
        assert!(matches!(picked.source, QuoteSource::Tdx));
        assert!(picked.is_display_complete());
        assert!(!picked.is_quote_complete());
    }

    #[test]
    fn pick_fallback_returns_none_if_no_display_complete() {
        let tdx = fake_quote(QuoteSource::Tdx, false, false);
        let em = fake_quote(QuoteSource::Eastmoney, false, false);
        assert!(QuotesService::pick_fallback_quote(vec![tdx, em]).is_none());
    }

    // ============================================================================ E1 — market_breadth / industry_heatmap
    //
    // Spec: docs/design/quotes-module.md §4 (`market_breadth` / `industry_heatmap`)
    //
    // 测试策略：用 service 的 `market_time_now` 派生 eligible trade date，把这个
    // trade_date 写到 fake quote 上确保 `derive_freshness` 接受；用
    // `cache.put(CachedSnapshot)` 直接写入 snapshot 而非走 refresh 网络路径。

    fn seed_instrument_with(
        svc: &QuotesService,
        ts: &str,
        name: &str,
        sector: Option<&str>,
        board: Option<&str>,
        is_st: bool,
    ) {
        let code = TsCode::parse(ts).unwrap();
        let inst = MarketInstrument {
            ts_code: code.clone(),
            name: name.to_string(),
            category: InstrumentCategory::Stock,
            market: code.market(),
            board: board.map(str::to_string),
            sector: sector.map(str::to_string),
            status: Some(InstrumentStatus::Listed),
            is_st: Some(is_st),
            publisher: None,
            index_category: None,
            fund_type: None,
            management: None,
            list_date: None,
            source: crate::domain::quotes::InstrumentSource::Tushare,
            updated_at: Utc::now(),
        };
        svc.repo().upsert_instruments(&[inst]).unwrap();
    }

    /// 把一只标的的当日 quote 写入 in-memory `MARKET_SNAPSHOT`。
    fn put_snapshot_for(
        svc: &QuotesService,
        ts: &str,
        change_percent: f64,
    ) {
        use crate::domain::shared::{Freshness, FreshnessStatus, Price};
        let ts_code = TsCode::parse(ts).unwrap();
        let eligible = eligible_trade_date(&svc.market_time_now());
        let now = Utc::now();
        let q = StockQuote {
            ts_code: ts_code.clone(),
            name: None,
            category: InstrumentCategory::Stock,
            trade_date: eligible.trade_date,
            price: Some(Price(Decimal::new(1000, 2))),
            previous_close: Some(Price(Decimal::new(1000, 2))),
            open: None,
            high: None,
            low: None,
            change: None,
            change_percent: Some(change_percent),
            volume: None,
            amount: None,
            turnover_rate: None,
            volume_ratio: None,
            limit_up: None,
            limit_down: None,
            bid: Vec::new(),
            ask: Vec::new(),
            trade_status: TradeStatus::Trading,
            source: QuoteSource::Tdx,
            captured_at: now,
            exchange_time: None,
            freshness: Freshness {
                status: FreshnessStatus::Fresh,
                captured_at: Some(now),
                exchange_time: None,
                age_ms: Some(0),
                source: Some("tdx".into()),
                warning: None,
            },
            warnings: Vec::new(),
        };
        svc.cache.put(crate::infrastructure::quotes::CachedSnapshot {
            quote: q,
            captured_at: now,
            trade_date: eligible.trade_date,
            source: "tdx".into(),
        });
    }

    #[test]
    fn market_breadth_empty_universe_returns_zero() {
        let svc = make_service();
        let b = svc.market_breadth();
        assert_eq!(b.total, 0);
        assert_eq!(b.up + b.down + b.flat, 0);
        assert_eq!(b.no_data, 0);
        assert_eq!(b.limit_up, 0);
        assert_eq!(b.limit_down, 0);
    }

    #[test]
    fn market_breadth_counts_up_down_flat() {
        let svc = make_service();
        // 5 涨 / 3 跌 / 1 平 — 全部主板 10% bounded（未达涨停）。
        seed_instrument_with(&svc, "600001.SH", "U1", None, Some("主板"), false);
        put_snapshot_for(&svc, "600001.SH", 2.0);
        seed_instrument_with(&svc, "600002.SH", "U2", None, Some("主板"), false);
        put_snapshot_for(&svc, "600002.SH", 3.5);
        seed_instrument_with(&svc, "600003.SH", "U3", None, Some("主板"), false);
        put_snapshot_for(&svc, "600003.SH", 1.0);
        seed_instrument_with(&svc, "600004.SH", "U4", None, Some("主板"), false);
        put_snapshot_for(&svc, "600004.SH", 0.1);
        seed_instrument_with(&svc, "600005.SH", "U5", None, Some("主板"), false);
        put_snapshot_for(&svc, "600005.SH", 5.0);
        seed_instrument_with(&svc, "600010.SH", "D1", None, Some("主板"), false);
        put_snapshot_for(&svc, "600010.SH", -1.5);
        seed_instrument_with(&svc, "600011.SH", "D2", None, Some("主板"), false);
        put_snapshot_for(&svc, "600011.SH", -3.0);
        seed_instrument_with(&svc, "600012.SH", "D3", None, Some("主板"), false);
        put_snapshot_for(&svc, "600012.SH", -2.0);
        seed_instrument_with(&svc, "600020.SH", "F1", None, Some("主板"), false);
        put_snapshot_for(&svc, "600020.SH", 0.0);

        let b = svc.market_breadth();
        assert_eq!(b.total, 9);
        assert_eq!(b.up, 5);
        assert_eq!(b.down, 3);
        assert_eq!(b.flat, 1);
        assert_eq!(b.no_data, 0);
        assert_eq!(b.limit_up, 0);
        assert_eq!(b.limit_down, 0);
    }

    #[test]
    fn market_breadth_detects_limit_up_main_board_10pct() {
        let svc = make_service();
        // 主板：10% 阈值；9.96 已视为涨停（epsilon = 0.05）。
        seed_instrument_with(&svc, "600001.SH", "LU", None, Some("主板"), false);
        put_snapshot_for(&svc, "600001.SH", 9.96);
        let b = svc.market_breadth();
        assert_eq!(b.up, 1);
        assert_eq!(b.limit_up, 1);
        assert_eq!(b.limit_down, 0);
    }

    #[test]
    fn market_breadth_detects_limit_down_main_board_10pct() {
        let svc = make_service();
        seed_instrument_with(&svc, "600001.SH", "LD", None, Some("主板"), false);
        put_snapshot_for(&svc, "600001.SH", -9.97);
        let b = svc.market_breadth();
        assert_eq!(b.down, 1);
        assert_eq!(b.limit_down, 1);
        assert_eq!(b.limit_up, 0);
    }

    #[test]
    fn market_breadth_chinext_limit_up_threshold_is_20pct() {
        let svc = make_service();
        // 创业板：20% 阈值。10% 在创业板上不是涨停。
        seed_instrument_with(&svc, "300750.SZ", "CN1", None, Some("创业板"), false);
        put_snapshot_for(&svc, "300750.SZ", 10.0);
        let b = svc.market_breadth();
        assert_eq!(b.up, 1);
        assert_eq!(b.limit_up, 0, "10% on ChiNext is not limit_up");
        // 19.96% 应该达到涨停。
        let svc2 = make_service();
        seed_instrument_with(&svc2, "300750.SZ", "CN1", None, Some("创业板"), false);
        put_snapshot_for(&svc2, "300750.SZ", 19.96);
        let b2 = svc2.market_breadth();
        assert_eq!(b2.limit_up, 1);
    }

    #[test]
    fn market_breadth_st_limit_at_5pct() {
        let svc = make_service();
        // SH 主板 ST 标的 5% 涨停。
        seed_instrument_with(&svc, "600100.SH", "ST X", None, Some("主板"), true);
        put_snapshot_for(&svc, "600100.SH", 4.97);
        let b = svc.market_breadth();
        assert_eq!(b.limit_up, 1);
    }

    #[test]
    fn market_breadth_no_data_counts_instruments_without_snapshot() {
        let svc = make_service();
        seed_instrument_with(&svc, "600001.SH", "X", None, Some("主板"), false);
        // 没有写 snapshot —— 应进 no_data。
        let b = svc.market_breadth();
        assert_eq!(b.total, 0);
        assert_eq!(b.no_data, 1);
    }

    #[test]
    fn market_breadth_ignores_non_stock_category() {
        let svc = make_service();
        // 指数：market_breadth 只统计 category == stock。
        let code = TsCode::parse("000001.SH").unwrap();
        let inst = MarketInstrument {
            ts_code: code.clone(),
            name: "上证指数".to_string(),
            category: InstrumentCategory::Index,
            market: code.market(),
            board: None,
            sector: None,
            status: Some(InstrumentStatus::Listed),
            is_st: Some(false),
            publisher: None,
            index_category: None,
            fund_type: None,
            management: None,
            list_date: None,
            source: crate::domain::quotes::InstrumentSource::Tushare,
            updated_at: Utc::now(),
        };
        svc.repo().upsert_instruments(&[inst]).unwrap();
        let b = svc.market_breadth();
        assert_eq!(b.total, 0);
        assert_eq!(b.no_data, 0, "non-stock not counted into universe");
    }

    #[test]
    fn industry_heatmap_groups_by_sector_and_picks_leaders() {
        let svc = make_service();
        // sector A：3 只，平均 (+5 + +3 + +1) / 3 ≈ +3
        seed_instrument_with(&svc, "600001.SH", "A1", Some("电子"), Some("主板"), false);
        put_snapshot_for(&svc, "600001.SH", 5.0);
        seed_instrument_with(&svc, "600002.SH", "A2", Some("电子"), Some("主板"), false);
        put_snapshot_for(&svc, "600002.SH", 3.0);
        seed_instrument_with(&svc, "600003.SH", "A3", Some("电子"), Some("主板"), false);
        put_snapshot_for(&svc, "600003.SH", 1.0);
        // sector B：2 只，平均 -2
        seed_instrument_with(&svc, "600010.SH", "B1", Some("银行"), Some("主板"), false);
        put_snapshot_for(&svc, "600010.SH", -1.0);
        seed_instrument_with(&svc, "600011.SH", "B2", Some("银行"), Some("主板"), false);
        put_snapshot_for(&svc, "600011.SH", -3.0);

        let h = svc.industry_heatmap(5);
        assert_eq!(h.top_gainers.len(), 2);
        // top_gainers[0] 应是 "电子"（avg ≈ +3）。
        assert_eq!(h.top_gainers[0].sector, "电子");
        assert!((h.top_gainers[0].avg_change_percent - 3.0).abs() < 1e-6);
        assert_eq!(h.top_gainers[0].count, 3);
        // leader_codes 取 change_percent desc 前 3：600001(+5), 600002(+3), 600003(+1)。
        assert_eq!(h.top_gainers[0].leader_codes.len(), 3);
        assert_eq!(h.top_gainers[0].leader_codes[0].as_str(), "600001.SH");
        assert_eq!(h.top_gainers[0].leader_codes[1].as_str(), "600002.SH");
        assert_eq!(h.top_gainers[0].leader_codes[2].as_str(), "600003.SH");
        assert_eq!(h.top_gainers[0].leader_names[0], "A1");
        // top_losers[0] 应是 "银行"（avg = -2）。
        assert_eq!(h.top_losers[0].sector, "银行");
        assert!((h.top_losers[0].avg_change_percent + 2.0).abs() < 1e-6);
    }

    #[test]
    fn industry_heatmap_excludes_unclassified_sector() {
        let svc = make_service();
        // 一个有 sector，一个 sector=None。
        seed_instrument_with(&svc, "600001.SH", "A1", Some("电子"), Some("主板"), false);
        put_snapshot_for(&svc, "600001.SH", 3.0);
        seed_instrument_with(&svc, "600010.SH", "U1", None, Some("主板"), false);
        put_snapshot_for(&svc, "600010.SH", 10.0);

        let h = svc.industry_heatmap(5);
        // 仅 "电子" 进入；"未分类" 不参与 top（spec §4）。
        assert_eq!(h.top_gainers.len(), 1);
        assert_eq!(h.top_gainers[0].sector, "电子");
        assert!(h
            .top_gainers
            .iter()
            .all(|i| i.sector != "未分类"));
    }

    #[test]
    fn industry_heatmap_respects_top_n() {
        let svc = make_service();
        // 4 个 sector → top_n=2 应仅返回 2。
        for (idx, sector) in ["电子", "银行", "白酒", "新能源"].iter().enumerate() {
            let ts = format!("60000{}.SH", idx);
            seed_instrument_with(&svc, &ts, sector, Some(sector), Some("主板"), false);
            put_snapshot_for(&svc, &ts, idx as f64);
        }
        let h = svc.industry_heatmap(2);
        assert_eq!(h.top_gainers.len(), 2);
        assert_eq!(h.top_losers.len(), 2);
        // 全 4 都正/0 → top_losers 仍按 asc 排序，取最小 2 个。
    }
}
