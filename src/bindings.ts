// Typed Tauri command bindings.
//
// Spec: docs/design/architecture.md（前端走强类型 wrapper，不裸调 invoke）
//
// 本文件是 specta 自动 export 目标（src-tauri/src/lib.rs::run() 在 debug build
// 时会覆盖 ../src/bindings.ts）。但 sub-agent 工作流不便启动 long-running
// `tauri dev`，所以这里手写一个与后端 DTO 一致的类型层：
//
// - 与 src-tauri/src/pipeline/quotes/service.rs / src-tauri/src/domain/* 中
//   带 #[derive(Type)] + #[serde(rename_all = "camelCase")] 的 DTO 对齐。
// - newtype（TsCode / TradeDate / Money / Price / Amount / Volume）对外都序列化为
//   字符串或数字 — 见 shared-types.md §2 / §3。前端用 string / number alias 表示。
//
// 后端命令清单（lib.rs::collect_commands![...]，2026-05-28 状态）：
//   ping
//   fetch_news / list_news_sources / warm_articles
//   list_market / fetch_data / scan_market
//   agent_list_skills
//   fetch_account / update_watchlist / mark_trigger_handled
//     / rebuild_account_snapshot
//
// 注意：account-module spec §4 — operate_account 写入口不通过 Tauri command
// 暴露，前端不能直接 invoke；下面 commands 对象也不暴露它。

import { invoke as tauriInvoke } from "@tauri-apps/api/core";

// ============================================================================
// 通用 result / error 类型
// ============================================================================

export type CommandResult<T, E = CommandError> =
  | { status: "ok"; data: T }
  | { status: "error"; error: E };

/**
 * Spec: shared-types.md §5 — 封闭集合（与 src-tauri/src/domain/shared/codes.rs::ErrorCode 对齐）。
 *
 * 历史：F1 / F2 阶段 `ErrorCode` 是开放 string；现在收紧成闭合 union，但保留
 * 后端 IPC 框架可能返回的额外 code（如 `ipc.unknown`、`tauri.*`）—— 这些走 string fallback。
 * 实际后端 DTO 的 code 都必须是下面 union 之一；前端 ResponseError / FetchNewsError /
 * WarmArticlesError 等都基于这个类型，跟 Rust serde 一致。
 */
export type ErrorCode =
  | "invalid_input"
  | "not_found"
  | "provider_unavailable"
  | "rate_limited"
  | "db_error"
  | "parse_error"
  | "quote_missing"
  | "quote_stale"
  | "quote_price_missing"
  | "depth_missing"
  | "outside_trading_session"
  | "instrument_not_tradable"
  | "instrument_suspended"
  | "limit_up_down_blocked"
  | "insufficient_cash"
  | "insufficient_sellable_quantity"
  | "invalid_lot_size"
  | "order_not_pending"
  | "risk_limit_exceeded"
  | "strategy_required"
  | "duplicate_event"
  | "version_conflict"
  | "article_extract_failed"
  | "tool_timeout"
  | "provider_context_too_long";

/**
 * 前端 IPC wrapper 可能产生的 envelope code（非后端 ErrorCode），
 * 仅在 invoke 失败 / 反序列化失败时出现。
 */
export type IpcEnvelopeCode = "ipc.unknown";

/** CommandError.code 包括后端 ErrorCode + 前端 wrapper code。 */
export type AnyErrorCode = ErrorCode | IpcEnvelopeCode | (string & {});

export interface CommandError {
  code: AnyErrorCode;
  message?: string;
  details?: unknown;
}

async function call<T>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<CommandResult<T, CommandError>> {
  try {
    const data = (await tauriInvoke(cmd, args ?? {})) as T;
    return { status: "ok", data };
  } catch (err) {
    if (typeof err === "object" && err !== null && "code" in err) {
      return { status: "error", error: err as CommandError };
    }
    return {
      status: "error",
      error: {
        code: "ipc.unknown",
        message: typeof err === "string" ? err : JSON.stringify(err),
      },
    };
  }
}

// ============================================================================
// shared scalar / 枚举 — 与 src-tauri/src/domain/shared 一致
// ============================================================================

/** `600519.SH` / `000001.SZ` / `430047.BJ` 。 Spec: shared-types.md §1 */
export type TsCode = string;
/** `YYYYMMDD` 字符串。 Spec: shared-types.md §3 */
export type TradeDate = string;
/** ISO-8601 UTC datetime 字符串。 */
export type OccurredAt = string;
export type TimestampMs = number;

/** rust_decimal 序列化为 string；前端用 string 保留精度。 */
export type Money = string;
export type Price = string;
export type Amount = string;
/** 整数股 / 整数份。 */
export type Volume = number;
export type Shares = number;
/** 0..1 比例 / 百分数。 */
export type Ratio = number;
export type Percent = number;

export type Market = "SH" | "SZ" | "BJ";
export type InstrumentCategory = "stock" | "index" | "fund";
export type InstrumentStatus = "listed" | "suspended" | "delisted" | "unknown";

export type FreshnessStatus = "fresh" | "stale" | "missing";

/** Spec: shared-types.md §5 — 封闭集合。 */
export type WarningCode =
  | "quote_missing"
  | "quote_stale"
  | "snapshot_expired"
  | "quote_price_missing"
  | "depth_missing"
  | "instrument_missing"
  | "provider_partial_failure"
  | "article_missing"
  | "qfq_missing"
  | "using_unadjusted_kline"
  | "daily_basic_missing"
  | "events_missing"
  | "strategy_omitted"
  | "mapping_missing"
  | "data_partial";

export interface Freshness {
  status: FreshnessStatus;
  capturedAt?: OccurredAt;
  exchangeTime?: OccurredAt;
  ageMs?: number;
  source?: string;
  warning?: WarningCode;
}

export interface ResponseError {
  code: ErrorCode;
  message?: string;
  field?: string;
  tsCode?: TsCode;
}

// ============================================================================
// Quotes — domain types
// ============================================================================

/** Spec: quotes-module.md §2 K 线 period（日/周/月）。 */
export type KlinePeriod = "day" | "week" | "month";
/** Spec: quotes-module.md §2 分钟 K period。 */
export type MinuteKlinePeriod = "1m" | "5m" | "15m" | "30m" | "60m";
/** 任意 K 线 period（含日/周/月 + 分钟）。前端切换器用。 */
export type AnyKlinePeriod = KlinePeriod | MinuteKlinePeriod;

export type Adjust = "none" | "qfq" | "hfq";

export type QuoteSource = "tdx" | "eastmoney" | "tencent" | "sina" | "mixed";
export type InstrumentSource = "tdx" | "eastmoney" | "tushare" | "mixed";
export type TradeStatus = "trading" | "halted" | "closed" | "unknown";

/** Spec: quotes-module.md §2 */
export interface MarketInstrument {
  tsCode: TsCode;
  name: string;
  category: InstrumentCategory;
  market: Market;
  board?: string;
  sector?: string;
  status?: InstrumentStatus;
  isSt?: boolean;
  publisher?: string;
  indexCategory?: string;
  fundType?: string;
  management?: string;
  listDate?: string;
  source: InstrumentSource;
  updatedAt: OccurredAt;
}

/** Spec: quotes-module.md §2 */
export interface StockProfile {
  tsCode: TsCode;
  name: string;
  category: InstrumentCategory;
  market: Market;
  board?: string;
  sector?: string;
  status?: InstrumentStatus;
  isSt?: boolean;
  listDate?: string;
}

export interface QuoteDepthLevel {
  price?: Price;
  volume?: Volume;
}

/** Spec: quotes-module.md §2 StockQuote */
export interface StockQuote {
  tsCode: TsCode;
  name?: string;
  category: InstrumentCategory;
  tradeDate: TradeDate;
  price?: Price;
  previousClose?: Price;
  open?: Price;
  high?: Price;
  low?: Price;
  change?: Price;
  changePercent?: Percent;
  volume?: Volume;
  amount?: Amount;
  turnoverRate?: Percent;
  volumeRatio?: number;
  limitUp?: Price;
  limitDown?: Price;
  bid?: QuoteDepthLevel[];
  ask?: QuoteDepthLevel[];
  tradeStatus: TradeStatus;
  source: QuoteSource;
  capturedAt: OccurredAt;
  exchangeTime?: OccurredAt;
  freshness: Freshness;
  warnings?: WarningCode[];
}

/** K 线点（日/周/月） */
export interface KlinePoint {
  date: TradeDate;
  open: Price;
  close: Price;
  high: Price;
  low: Price;
  volume?: Volume;
  amount?: Amount;
}

export interface KlineSeries {
  period: KlinePeriod;
  adjust: Adjust;
  points: KlinePoint[];
  freshness: Freshness;
  warnings?: WarningCode[];
}

export interface MinuteKlinePoint {
  timestampMs: TimestampMs;
  open: Price;
  close: Price;
  high: Price;
  low: Price;
  volume: Volume;
  amount: Amount;
}

export interface MinuteKlineSeries {
  period: MinuteKlinePeriod;
  points: MinuteKlinePoint[];
  freshness: Freshness;
  warnings?: WarningCode[];
}

/** 分时单点。 */
export interface MinutePoint {
  tradeDate: TradeDate;
  time: string;
  price: Price;
  average?: Price;
  volume?: Volume;
  amount?: Amount;
}

export interface IntradaySeries {
  tradeDate: TradeDate;
  points: MinutePoint[];
  freshness: Freshness;
  warnings?: WarningCode[];
}

export interface DailyBasic {
  tsCode: TsCode;
  tradeDate: TradeDate;
  pe?: number;
  peTtm?: number;
  pb?: number;
  ps?: number;
  psTtm?: number;
  turnoverRate?: Percent;
  turnoverRateFloat?: Percent;
  volumeRatio?: number;
  totalMv?: Money;
  circMv?: Money;
  source: string;
  fetchedAt: OccurredAt;
}

export type CompanyEventType =
  | "dividend"
  | "suspension"
  | "resume"
  | "st"
  | "earnings_forecast"
  | "unlock"
  | "other";

export interface CompanyEvent {
  id: string;
  tsCode: TsCode;
  eventType: CompanyEventType;
  announceDate?: TradeDate;
  effectiveDate?: TradeDate;
  payload: unknown;
  source: string;
  fetchedAt: OccurredAt;
}

export type IndicatorName =
  | "ma5"
  | "ma10"
  | "ma20"
  | "ma60"
  | "ema12"
  | "ema26"
  | "macd_dif"
  | "macd_dea"
  | "macd_hist"
  | "rsi6"
  | "rsi12"
  | "rsi24"
  | "kdj_k"
  | "kdj_d"
  | "kdj_j"
  | "boll_upper"
  | "boll_mid"
  | "boll_lower"
  | "volume_ma5"
  | "volume_ma10";

export interface IndicatorBasis {
  period: KlinePeriod;
  adjust: Adjust;
  fetchedAt: OccurredAt;
}

export interface IndicatorSnapshot {
  tsCode: TsCode;
  basis: IndicatorBasis;
  /** key 是 indicator 名（lower_snake_case，对应 IndicatorName） */
  values: Record<string, number | null>;
  warnings?: WarningCode[];
}

// ============================================================================
// Quotes — list_market
// ============================================================================

export interface ListMarketRequest {
  category?: InstrumentCategory;
  query?: string;
  includeQuote?: boolean;
  limit?: number;
  offset?: number;
}

export interface ListMarketQuoteSummary {
  tradeDate?: TradeDate;
  price?: number;
  change?: number;
  changePercent?: number;
  open?: number;
  high?: number;
  low?: number;
  previousClose?: number;
  volume?: number;
  amount?: number;
}

/** 与后端 `#[serde(flatten)] instrument` + quote/quoteFreshness/warnings 对齐。 */
export type ListMarketItem = MarketInstrument & {
  quote?: ListMarketQuoteSummary;
  quoteFreshness?: Freshness;
  warnings?: WarningCode[];
};

export interface ListMarketPage {
  limit: number;
  offset: number;
  hasMore: boolean;
}

export interface ListMarketResponse {
  items: ListMarketItem[];
  page: ListMarketPage;
}

// ============================================================================
// Quotes — fetch_data
// ============================================================================

/** indicators: `true` = 全部；数组 = subset；不传 = 不计算。 */
export type FetchIndicators = boolean | IndicatorName[];

export interface FetchInclude {
  quote?: boolean;
  intraday?: boolean;
  klines?: KlinePeriod[];
  minuteKlines?: MinuteKlinePeriod[];
  indicators?: FetchIndicators;
  profile?: boolean;
  dailyBasic?: boolean;
  events?: boolean;
}

export interface FetchLimits {
  kline?: number;
  minuteKline?: number;
  eventsDaysAhead?: number;
}

export interface FetchDataRequest {
  tsCodes: TsCode[];
  include?: FetchInclude;
  limit?: FetchLimits;
}

export interface FetchDataItem {
  tsCode: TsCode;
  category: InstrumentCategory;
  name?: string;
  quote?: StockQuote;
  quoteFreshness?: Freshness;
  intraday?: IntradaySeries;
  /** BTreeMap<period, series> — 后端用 period.as_str() 做 key（如 "day"/"week"/"month"）。 */
  klines?: Partial<Record<KlinePeriod, KlineSeries>>;
  /** 同理，key 是 "1m"/"5m" 等。 */
  minuteKlines?: Partial<Record<MinuteKlinePeriod, MinuteKlineSeries>>;
  indicators?: IndicatorSnapshot;
  profile?: StockProfile;
  dailyBasic?: DailyBasic;
  events?: CompanyEvent[];
  warnings?: WarningCode[];
}

export interface FetchDataResponse {
  errors?: ResponseError[];
  items: FetchDataItem[];
}

// ============================================================================
// Quotes — scan_market
// ============================================================================

export type ScanFilter =
  | "limit_up"
  | "limit_down"
  | "top_gain"
  | "top_loss"
  | "top_amount"
  | "top_volume";

export type ScanSortBy =
  | "change_pct_desc"
  | "change_pct_asc"
  | "amount_desc"
  | "volume_desc"
  | "turnover_rate_desc";

export type ScanOp = "gt" | "gte" | "lt" | "lte" | "eq" | "between";

export type ScanConditionField =
  | "change_percent"
  | "amount"
  | "volume"
  | "turnover_rate"
  | "volume_ratio"
  | "pe_ttm"
  | "pb"
  | "total_mv"
  | "circ_mv";

export interface ScanCondition {
  field: ScanConditionField;
  op: ScanOp;
  value: number | [number, number];
}

export interface ScanMarketRequest {
  category?: InstrumentCategory;
  filter?: ScanFilter;
  conditions?: ScanCondition[];
  sortBy?: ScanSortBy;
  limit?: number;
}

export interface ScanUniverse {
  category?: InstrumentCategory;
  total: number;
  validQuoteCount?: number;
  excludedMissingQuoteCount?: number;
  excludedExpiredQuoteCount?: number;
  matched: number;
}

export interface ScanCriteria {
  filter?: string;
  conditions?: ScanCondition[];
  sortBy?: string;
  limit: number;
}

export interface ScanItem {
  rank: number;
  tsCode: TsCode;
  name?: string;
  category: InstrumentCategory;
  quote?: StockQuote;
  dailyBasic?: DailyBasic;
  warnings?: WarningCode[];
}

export interface ScanMarketResponse {
  generatedAt: OccurredAt;
  universe: ScanUniverse;
  criteria: ScanCriteria;
  items: ScanItem[];
  warnings?: WarningCode[];
  errors?: ResponseError[];
}

// ============================================================================
// Ping
// ============================================================================

export interface PingResult {
  ok: boolean;
  message: string;
}

// ============================================================================
// News — fetch_news / list_news_sources / warm_articles
//
// Spec: docs/design/news-module.md §4 §5
// 后端 DTO: src-tauri/src/domain/news/{types, source, errors, events}.rs
// ============================================================================

/** Spec: news-module.md §4 fetch_news. */
export interface FetchNewsRequest {
  /** FTS 文本查询；trim + 折叠连续空白后才能命中。 */
  query?: string;
  /** sourceId 数组；未知 source 后端会返回 invalid_input。 */
  sources?: string[];
  /** ISO-8601。 */
  publishedFrom?: OccurredAt;
  /** ISO-8601。 */
  publishedTo?: OccurredAt;
  /** 是否附带本地 ArticleContent；不会触发远端抽取。 */
  includeArticle?: boolean;
  /** 默认 50，最大 200。 */
  limit?: number;
  /** 默认 0。 */
  offset?: number;
}

/** Spec: news-module.md §4 fetch_news item.article。 */
export interface ArticleSnippet {
  title?: string;
  content: string;
  fetchedAt?: OccurredAt;
}

export interface NewsItemFreshness {
  /** 距离 fetchedAt 的毫秒数。 */
  ageMs?: number;
  articleFetchedAt?: OccurredAt;
}

export interface FetchNewsItem {
  id: string;
  /** sourceId，与 NewsSource.sourceId 对应。 */
  source: string;
  title: string;
  summary?: string;
  url?: string;
  publishedAt?: OccurredAt;
  /** 已清洗 + 截断的正文摘录，默认前 500 字符。 */
  articleExcerpt?: string;
  /** includeArticle=true 时存在；缺失正文时返回 article_missing warning，不返回 article。 */
  article?: ArticleSnippet;
  freshness?: NewsItemFreshness;
  warnings: WarningCode[];
  errors: ErrorCode[];
}

export interface FetchNewsError {
  field?: string;
  code: ErrorCode;
  message?: string;
}

export interface FetchNewsPage {
  limit: number;
  offset: number;
  hasMore: boolean;
}

export interface FetchNewsResponse {
  items: FetchNewsItem[];
  errors?: FetchNewsError[];
  page: FetchNewsPage;
}

/** Spec: news-module.md §2 NewsSource。 */
export interface NewsSourceLastError {
  code: ErrorCode;
  message?: string;
  occurredAt: OccurredAt;
}

export interface NewsSource {
  /** `namespace:channel` 形式。 */
  sourceId: string;
  /** Provider key（"rss" / "newsnow" / ...）。 */
  provider: string;
  displayName?: string;
  enabled: boolean;
  dynamic?: boolean;
  lastRefreshAt?: OccurredAt;
  lastError?: NewsSourceLastError;
}

export interface ListNewsSourcesResponse {
  items: NewsSource[];
}

/** Spec: news-module.md §4 warm_articles. */
export interface WarmArticlesRequest {
  /** 指定要 warm 的 NewsItem.id 列表（最多 200）。 */
  newsIds?: string[];
  /** 未传 newsIds 时按 recent_limit 拿最近未抽取的（默认 50，最大 200）。 */
  recentLimit?: number;
  /** 强制重新抽取，即使本地已有 ArticleContent。 */
  force?: boolean;
}

/** NewsRefreshStage —— 与 Rust 端 NewsRefreshStage 对齐。 */
export type NewsRefreshStage = "fetch" | "normalize" | "save" | "article";

export interface NewsFailure {
  provider: string;
  source?: string;
  code: ErrorCode;
  message?: string;
  details?: unknown;
  stage?: NewsRefreshStage;
  retryable?: boolean;
  occurredAt: OccurredAt;
}

export interface NewsRefreshWarning {
  provider: string;
  source?: string;
  code: WarningCode;
  message?: string;
  stage?: NewsRefreshStage;
  skippedCount?: number;
  occurredAt: OccurredAt;
}

export interface WarmArticlesResult {
  batchId: string;
  requestedCount: number;
  attemptedCount: number;
  articleUpdatedCount: number;
  articleUpdatedNewsIds: string[];
  warnings?: NewsRefreshWarning[];
  failures?: NewsFailure[];
}

export type WarmArticlesErrorField = "newsIds" | "recentLimit";

export interface WarmArticlesErrorBody {
  code: ErrorCode;
  field?: WarmArticlesErrorField;
  message?: string;
}

/**
 * Discriminated union — Rust 端用 `#[serde(untagged)]`，但每个分支 `ok: true | false`
 * 充当判别字段（TrueFlag / FalseFlag 序列化为 bool literal）。
 */
export type WarmArticlesResponse =
  | { ok: true; result: WarmArticlesResult }
  | { ok: false; error: WarmArticlesErrorBody };

/** Spec: shared-types.md §6 news-refreshed event payload。 */
export interface NewsRefreshedPayload {
  batchId: string;
  fetchedCount: number;
  skippedCount: number;
  savedCount: number;
  articleUpdatedCount: number;
  newIds: string[];
  updatedIds: string[];
  articleUpdatedNewsIds: string[];
  failedCount: number;
  firstFailure?: NewsFailure;
  failures?: NewsFailure[];
  warnings?: NewsRefreshWarning[];
}

// ============================================================================
// Account — fetch_account / update_watchlist / mark_trigger_handled
//          / rebuild_account_snapshot
//
// Spec: docs/design/account-module.md §4
// 注意：operate_account 不暴露给前端。
// 占位类型：F4 sub-agent 完成时补精确字段。
// ============================================================================

export interface FetchAccountRequest {
  include?: {
    snapshot?: boolean;
    positions?: boolean;
    watchlist?: boolean;
    triggers?: boolean;
    recentClosed?: boolean;
    [key: string]: unknown;
  };
  [key: string]: unknown;
}

export interface FetchAccountResponse {
  snapshot?: unknown;
  positions?: unknown[];
  watchlist?: unknown[];
  triggers?: unknown[];
  [key: string]: unknown;
}

export interface UpdateWatchlistRequest {
  add?: TsCode[];
  remove?: TsCode[];
  note?: string | null;
  [key: string]: unknown;
}

export interface UpdateWatchlistResponse {
  watchlist?: unknown[];
  [key: string]: unknown;
}

export interface MarkTriggerHandledRequest {
  triggerId: string;
  outcome?: string | null;
  [key: string]: unknown;
}

export interface MarkTriggerHandledResponse {
  [key: string]: unknown;
}

export interface AccountSnapshot {
  [key: string]: unknown;
}

// ============================================================================
// Agent — agent_list_skills
//
// Spec: docs/design/agent-infra-module.md §5
// ============================================================================

export interface SkillSpec {
  name: string;
  description?: string | null;
  [key: string]: unknown;
}

// ============================================================================
// commands —— 强类型 wrapper
// ============================================================================

export const commands = {
  // ping
  ping: () => call<PingResult>("ping"),

  // quotes
  listMarket: (request: ListMarketRequest) =>
    call<ListMarketResponse>("list_market", { request }),
  fetchData: (request: FetchDataRequest) =>
    call<FetchDataResponse>("fetch_data", { request }),
  scanMarket: (request: ScanMarketRequest) =>
    call<ScanMarketResponse>("scan_market", { request }),

  // news
  fetchNews: (request: FetchNewsRequest) =>
    call<FetchNewsResponse>("fetch_news", { request }),
  listNewsSources: () => call<ListNewsSourcesResponse>("list_news_sources"),
  warmArticles: (request: WarmArticlesRequest) =>
    call<WarmArticlesResponse>("warm_articles", { request }),

  // account
  fetchAccount: (request: FetchAccountRequest) =>
    call<FetchAccountResponse>("fetch_account", { request }),
  updateWatchlist: (request: UpdateWatchlistRequest) =>
    call<UpdateWatchlistResponse>("update_watchlist", { request }),
  markTriggerHandled: (request: MarkTriggerHandledRequest) =>
    call<MarkTriggerHandledResponse>("mark_trigger_handled", { request }),
  rebuildAccountSnapshot: () =>
    call<AccountSnapshot>("rebuild_account_snapshot"),

  // agent
  agentListSkills: () => call<SkillSpec[]>("agent_list_skills"),
};

// ============================================================================
// Events —— 后端 emit 的事件常量（前端 listen 用）
//
// Spec: docs/design/architecture.md（流式数据走 emit / listen）
// ============================================================================

export const EVENTS = {
  newsRefreshed: "news-refreshed",
  marketQuotesRefreshed: "market-quotes-refreshed",
  accountUpdated: "account-updated",
  accountTriggered: "account-triggered",
} as const;

export type EventName = (typeof EVENTS)[keyof typeof EVENTS];
