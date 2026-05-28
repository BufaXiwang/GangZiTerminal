// Typed Tauri command bindings.
//
// Spec: docs/design/architecture.md（前端走强类型 wrapper，不裸调 invoke）
//
// 本文件是 specta 自动 export 目标（src-tauri/src/lib.rs::run() 在 debug build
// 时会覆盖 ../src/bindings.ts）。但 sub-agent 工作流不便启动 long-running
// `tauri dev`，所以这里手写一个最小可用版本：
//
// - 提供与后端命令一一对应的强类型函数
// - 输入 / 输出 payload 暂时用宽松类型（多数为 `unknown` / 结构占位）；
//   F2/F3/F4 sub-agent 在实现具体页面时按需补精确字段。
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

export type ErrorCode = string;

export interface CommandError {
  code: ErrorCode;
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
// Ping
// ============================================================================

export interface PingResult {
  ok: boolean;
  message: string;
}

// ============================================================================
// Quotes — list_market / fetch_data / scan_market
//
// Spec: docs/design/quotes-module.md §4
//
// 占位类型：F2 在实现市场页时按 Rust DTO（pipeline::quotes::service）补精确字段。
// ============================================================================

export type AssetClass = "stock" | "index" | "fund" | "concept" | string;
export type Board = string;
export type TsCode = string;
export type TradeDate = string; // YYYYMMDD
export type KlinePeriod =
  | "day"
  | "week"
  | "month"
  | "1m"
  | "5m"
  | "15m"
  | "30m"
  | "60m";

export interface ListMarketRequest {
  asset_class?: AssetClass | null;
  board?: Board | null;
  tag?: string | null;
  watchlist_only?: boolean | null;
  limit?: number | null;
  offset?: number | null;
  // F2 按需补充：search / sort / 其他字段
  [key: string]: unknown;
}

export interface ListMarketItem {
  ts_code: TsCode;
  name?: string | null;
  asset_class?: AssetClass | null;
  board?: Board | null;
  // 价格 / 涨跌幅 / 成交额 / 量比等占位
  [key: string]: unknown;
}

export interface ListMarketResponse {
  items: ListMarketItem[];
  total?: number | null;
  updated_at?: string | null;
  [key: string]: unknown;
}

export interface FetchDataRequest {
  ts_codes: TsCode[];
  include?: {
    quote?: boolean;
    klines?: KlinePeriod[];
    minute_klines?: KlinePeriod[];
    indicators?: string[];
    [key: string]: unknown;
  };
  trade_date?: TradeDate | null;
  [key: string]: unknown;
}

export interface FetchDataResponse {
  items: Record<TsCode, unknown>;
  [key: string]: unknown;
}

export interface ScanMarketRequest {
  // F2 按需补充
  [key: string]: unknown;
}

export interface ScanMarketResponse {
  items: unknown[];
  [key: string]: unknown;
}

// ============================================================================
// News — fetch_news / list_news_sources / warm_articles
//
// Spec: docs/design/news-module.md §4 §5
// ============================================================================

export interface FetchNewsRequest {
  source_keys?: string[] | null;
  from?: string | null;
  to?: string | null;
  keyword?: string | null;
  limit?: number | null;
  offset?: number | null;
  [key: string]: unknown;
}

export interface NewsItem {
  id: string;
  source_key?: string | null;
  title?: string | null;
  url?: string | null;
  published_at?: string | null;
  [key: string]: unknown;
}

export interface FetchNewsResponse {
  items: NewsItem[];
  total?: number | null;
  [key: string]: unknown;
}

export interface NewsSource {
  key: string;
  name?: string | null;
  enabled?: boolean | null;
  [key: string]: unknown;
}

export interface ListNewsSourcesResponse {
  sources: NewsSource[];
  [key: string]: unknown;
}

export interface WarmArticlesRequest {
  ids?: string[];
  urls?: string[];
  [key: string]: unknown;
}

export interface WarmArticlesResponse {
  ok: boolean;
  results?: unknown[];
  [key: string]: unknown;
}

// ============================================================================
// Account — fetch_account / update_watchlist / mark_trigger_handled
//          / rebuild_account_snapshot
//
// Spec: docs/design/account-module.md §4
// 注意：operate_account 不暴露给前端。
// ============================================================================

export interface FetchAccountRequest {
  include?: {
    snapshot?: boolean;
    positions?: boolean;
    watchlist?: boolean;
    triggers?: boolean;
    recent_closed?: boolean;
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
  trigger_id: string;
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
