// ============================================================================
// 模拟账户（新 DDD：domain::account 序列化）
// ============================================================================

export type DomainPositionStatus =
  | { state: "open" }
  | { state: "closed"; exitPrice: number; exitAt: number; reason: DomainCloseReason };

export type DomainCloseReason = "manual" | "stop_loss" | "take_profit" | "time_stop" | "invalidated";

/** 对应后端 domain::account::Position（rename_all = camelCase） */
export type DomainPosition = {
  id: string;
  code: string;
  name: string;
  avgEntryPrice: number;
  currentShares: number;
  status: DomainPositionStatus;
  stopLoss: number | null;
  takeProfit: number | null;
  timeStopAt: number | null;
  thesis: string;
  sourceAnalysisId: string;
  enteredAt: number;
};

/** 对应后端 domain::account::AccountSnapshot —— ACCOUNT_SNAPSHOT in-memory 真源 */
export type DomainAccountSnapshot = {
  initialCash: number;
  cash: number;
  openPositions: DomainPosition[];
  closedPositions: DomainPosition[];
  marketValue: number;
  realizedPnl: number;
  unrealizedPnl: number;
  totalPnl: number;
  totalAssets: number;
  capturedAt: number;
};

/** 对应后端 domain::account::PositionEventKind —— tag-discriminated */
export type DomainPositionEventKind =
  | { kind: "opened"; entryPrice: number; shares: number; commission: number }
  | { kind: "scaled_in"; delta: number; price: number; newAvg: number; commission: number }
  | { kind: "scaled_out"; delta: number; price: number; commission: number; stampTax: number }
  | { kind: "closed"; exitPrice: number; shares: number; reason: DomainCloseReason; commission: number; stampTax: number }
  | { kind: "stops_adjusted"; stopLoss: number | null; takeProfit: number | null; timeStopAt: number | null };

export type DomainEventSource =
  | { kind: "briefing"; analysis_id: string }
  | { kind: "review"; analysis_id: string }
  | { kind: "chat"; message_id: string }
  | { kind: "manual" }
  | { kind: "system" };

export type DomainPositionEvent = {
  id: string;
  positionId: string;
  kind: DomainPositionEventKind;
  occurredAt: number;
  source: DomainEventSource;
  agentNoteMd: string;
};

/** 自选股 + 元信息（list_watchlist_with_info IPC） */
export type WatchlistEntry = {
  tsCode: string;
  code: string;
  name: string;
};

// ============================================================================

export type NewsAnalysisStatus = "pending" | "processing" | "consumed";

export type NewsItem = {
  id: string;
  title: string;
  link?: string;
  source: string;
  published?: string;
  summary?: string;
  analysisStatus?: NewsAnalysisStatus;
};

export type ArticleContent = {
  url: string;
  title: string;
  source?: string;
  published?: string;
  author?: string;
  paragraphs: string[];
  images: string[];
  fetchedAt: string;
  extraction: string;
};

export type PositionEventKind =
  | "opened"
  | "reviewed"
  | "adjusted"
  | "trimmed"
  | "added"
  | "stop_triggered"
  | "take_profit_hit"
  | "time_stop_hit"
  | "invalidated"
  | "closed";

export type PositionEvent = {
  id: string;
  positionId: string;
  eventKind: PositionEventKind;
  occurredAt: string;
  sourceKind?: "briefing" | "review" | "chat" | "manual" | "system";
  sourceRef?: string;          // task_id / record_id / message_id
  payload?: Record<string, unknown>;
  agentNoteMd?: string;        // Agent 当时的判断摘要
};

export type SimulatedPosition = {
  id: string;
  code: string;
  name: string;
  entryPrice: number;
  shares: number;
  entryAt: string;
  exitPrice?: number;
  exitAt?: string;
  closeReason?: "stop_loss" | "take_profit" | "time_stop" | "invalidated" | "manual_reset" | string;
  thesis: string;
  stopLoss?: number;
  takeProfit?: number;
  /** ISO 8601 — 超过即触发时间止损平仓。开仓时由后端 derive_time_stop_at 写入。 */
  timeStopAt?: string;
  sourceAnalysisId: string;
  status: "open" | "closed";
};

export type InvestorMemory = {
  focusThemes: string[];
  preferredMarkets: string[];
  riskPreference: string;
  learningGoals: string[];
  knownBiases: string[];
  investmentPrinciples: string[];
  watchQuestions: string[];
  recentInsights: string[];
  updatedAt: string;
};

export type InvestorMemoryUpdate = Partial<Omit<InvestorMemory, "updatedAt">>;

/** 单一对话流的消息行：briefing/review/chat/system/highlight 全部混排 */
export type ChatMessageKind = "chat" | "briefing" | "review" | "system" | "highlight";

export type ChatMessage = {
  id: string;
  createdAt: string;
  role: "user" | "assistant" | "system";
  kind: ChatMessageKind;
  contentMd: string;
  contentJson?: ChatMessageContent | null;
  sourceTaskId?: string | null;
  sourceNewsIds?: string[] | null;
  sourceRecordId?: string | null;
};

export type ChatMessageContent = {
  /** 仅 chat（assistant）携带：本轮新沉淀的长期记忆增量（自动应用） */
  memoryUpdates?: InvestorMemoryUpdate;
  /** 本轮主动从长期记忆里删除的条目（按字段名+精确字符串匹配）——重放和审计用 */
  memoryRemovals?: InvestorMemoryUpdate;
  /** 系统消息附带备注 */
  note?: string;
  /** chat（user）携带：用户粘/拖进来的图片，存的是后端落盘的绝对路径 */
  images?: string[];
};

export type RiskAlert = {
  id: string;
  severity: "info" | "warning" | "danger";
  code?: string;
  title: string;
  detail: string;
  action?: string;
};

/** 实时报价 + 五档盘口（adapter StockQuoteDto 序列化） */
export type StockQuote = {
  code: string;
  name: string;
  price: number | null;
  change: number | null;
  changePercent: number | null;
  open: number | null;
  high: number | null;
  low: number | null;
  previousClose: number | null;
  dayVolume: number | null;
  dayAmount: number | null;
  // 五档
  bidPrices?: number[] | null;
  bidVolumes?: number[] | null;
  askPrices?: number[] | null;
  askVolumes?: number[] | null;
  bidTotal?: number | null;
  askTotal?: number | null;
  insideVolume?: number | null;
  outsideVolume?: number | null;
  quoteTime: number;       // unix ms
  capturedAt: number;      // unix ms
};

/** KlineChart 支持的周期 */
export type KlinePeriod =
  | "minute"   // 分时（EM trends2）
  | "1m"
  | "5m"
  | "15m"
  | "60m"
  | "day"
  | "week"
  | "month";

/** 分时点（EM trends2） */
export type MinutePoint = {
  time: string;
  price: number;
  average?: number | null;
  volume?: number | null;
  amount?: number | null;
};

/** 分钟 K：1m/5m/15m/30m/60m */
export type MinuteKlinePoint = {
  timestamp: number; // unix ms
  open: number;
  close: number;
  high: number;
  low: number;
  volume: number;
  amount: number;
};

/** 全市场标的——股票/指数/基金的合集 */
export type InstrumentCategory = "stock" | "index" | "fund";

export type MarketInstrument = {
  tsCode: string;   // "000001.SH"
  code: string;     // 6 位
  name: string;
  category: InstrumentCategory;
  sector: string | null;
};

export type OrderBookLevel = {
  price: number | null;
  volume: number | null;
};

/** 全市场旁路实时行情快照 */
export type MarketQuote = {
  tsCode: string;
  code: string;
  name: string;
  price: number | null;
  changePercent: number | null;
  change: number | null;
  open: number | null;
  high: number | null;
  low: number | null;
  previousClose: number | null;
  volume: number | null;
  amount: number | null;
  capturedAt: number;  // unix ms
  bidLevels?: OrderBookLevel[];
  askLevels?: OrderBookLevel[];
  buyVolume?: number | null;
  sellVolume?: number | null;
  orderImbalance?: number | null;
};

export type KlinePoint = {
  date: string;
  open: number;
  close: number;
  high: number;
  low: number;
  volume?: number | null;
  amount?: number | null;
};

export type MarketIndex = {
  code: string;
  name: string;
  price: number | null;
  change: number | null;
  changePercent: number | null;
  capturedAt: number; // unix ms
};

export type MarketBreadth = {
  rise: number;
  fall: number;
  flat: number;
};

export type SectorHot = {
  code: string;
  name: string;
  changePercent: number | null;
};

export type MarketOverview = {
  indices: MarketIndex[];
  breadth: MarketBreadth;
  sectors: SectorHot[];
  capturedAt: number; // unix ms
};

// ===== Agent loop 事件流（镜像 src-tauri/src/agent/types.rs::AgentEvent）=====

export type PipelineKind = "chat" | "briefing" | "review";

export type StopReason =
  | "end_turn"
  | "max_tokens"
  | "stop_sequence"
  | "max_turns"
  | "search_budget_exhausted"
  | "refusal"
  | "pause_turn";

export type ToolResultContent =
  | { type: "text"; text: string }
  | { type: "image"; mime: string; data: string }
  | { type: "json"; raw: unknown };

export type AgentEvent =
  | { type: "run_start"; run_id: string; pipeline: PipelineKind; model: string }
  | { type: "text_delta"; run_id: string; delta: string }
  | { type: "thinking"; run_id: string; delta: string }
  | {
      type: "tool_start";
      run_id: string;
      tool_use_id: string;
      name: string;
      input: unknown;
      server_side: boolean;
    }
  | {
      type: "tool_end";
      run_id: string;
      tool_use_id: string;
      name: string;
      output: ToolResultContent[];
      is_error: boolean;
      duration_ms: number;
      server_side: boolean;
    }
  | {
      type: "usage";
      run_id: string;
      input_tokens: number;
      output_tokens: number;
      cache_read_tokens: number;
      cache_write_tokens: number;
    }
  | {
      type: "compacted";
      run_id: string;
      // micro_clear: 清掉了易腐工具的旧 ToolResult（无模型调用）
      // summarize:   调便宜模型把老对话压成一段中文摘要 + 边界 user 消息
      // drop:        直接丢弃最老消息（兜底）
      tier: "micro_clear" | "summarize" | "drop";
      dropped_messages: number;
      summary_tokens: number;
    }
  | { type: "done"; run_id: string; stop_reason: StopReason; turns: number }
  | { type: "error"; run_id: string; message: string };

/**
 * 后端 `quotes-fetch-status` 事件 payload——只在行情拉取**有问题**时发，成功不打扰。
 *
 * - `providerError` 非 null = 三源全失败（接口异常）
 * - `missing` 非空 = 部分股票缺数据（停牌 / 接口部分未返回）
 * - 两者都为空 = 不应该收到这个事件（后端 has_any_issue 已过滤）
 */
export type QuotesFetchStatus = {
  stage: "chat" | "briefing" | "review" | "refresh";
  requested: number;
  ok: number;
  missing: string[];
  providerError: string | null;
};

// 单条 in-progress run 的本地累积状态
export type StreamingRunState = {
  runId: string;
  pipeline: PipelineKind;
  model: string;
  text: string;
  thinking: string;
  toolCalls: Array<{
    id: string;
    name: string;
    input: unknown;
    serverSide: boolean;
    status: "running" | "done" | "error";
    durationMs?: number;
  }>;
};
