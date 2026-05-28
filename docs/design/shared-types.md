# Shared Types

> 本文档定义跨 bounded context 共享的契约类型。模块 spec 只引用这些类型，不重复定义。

## 一句话定位

Shared types 是跨 Quotes / News / Account / Agent / Agent Runtime 使用的最小公共语言：标的、金额、数量、时间、freshness、错误码和事件 envelope。

---

## 1. 标的和市场

```ts
type TsCode = string;     // e.g. "600519.SH", "000001.SH", "510300.SH"

type Market = "SH" | "SZ" | "BJ";
type InstrumentCategory = "stock" | "index" | "fund";

type InstrumentStatus =
  | "listed"
  | "suspended"
  | "delisted"
  | "unknown";
```

规则：

- 跨模块主键必须使用 `TsCode`。
- `TsCode` 格式为 6 位数字 + `.` + 市场后缀，例如 `600519.SH` / `000001.SZ` / `430047.BJ`；匹配大小写不敏感，但对外返回必须大写。
- 非 `TsCode` 标识不能作为跨模块持久化主键。
- Quotes 对外读取不提供非 `TsCode` 标识的市场推断能力；调用方需要精确查询标的时必须提供完整 `TsCode`。

---

## 2. 金额、数量和比例

```ts
type Money = number;       // CNY，保留到分；内部计算可使用更高精度
type Price = number;       // CNY / 股或份
type Shares = number;      // 股 / 份数量，Account 股票和场内基金交易要求 100 股 / 份整数倍
type Ratio = number;       // 0..1
type Percent = number;     // 百分数点，例如 3.25 表示 3.25%
type Amount = number;      // 成交额，CNY
type Volume = number;      // 成交量，股或份；provider 单位必须 normalize 到最小交易数量单位，不使用“手”
```

规则：

- 对外 DTO 不使用 provider 原始单位。
- 股票成交量 / 盘口量统一为股；基金成交量统一为份；不使用“手”或“千股”作为 DTO 单位。
- 金额、价格、数量不能用字符串承载，除非是用户展示文案。
- Account 写接口必须校验 `Shares` 为正数；股票和场内基金买卖数量必须是 100 股 / 份整数倍。

---

## 3. 时间和交易日历

```ts
type OccurredAt = string;  // ISO-8601 with timezone
type TradeDate = string;   // YYYYMMDD
type TimestampMs = number;

type MarketTimeContext = {
  now: OccurredAt;
  isTradingTime: boolean;
  currentTradeDate?: TradeDate;
  latestCompletedTradeDate: TradeDate;
  nextTradeDate?: TradeDate;
};
```

A 股默认连续竞价时间窗口：

| 窗口 | 时间 |
|---|---|
| 上午连续竞价 | 09:30-11:30 Asia/Shanghai |
| 下午连续竞价 | 13:00-15:00 Asia/Shanghai |

统一方法：

```rust
resolve_market_time(now) -> MarketTimeContext
```

规则：

- 交易时间判断必须基于交易日历和 `Asia/Shanghai`。
- `isTradingTime = true` 只表示当前处于 A 股连续竞价时段，默认 09:30-11:30 / 13:00-15:00。
- 集合竞价、午休、收盘后、周末和节假日都视为 `isTradingTime = false`。
- `currentTradeDate` 只在当前自然日是交易日时返回；盘前 / 午休 / 盘后仍可返回当日交易日。
- `latestCompletedTradeDate` 表示最近一个已经完成收盘的交易日；周末 / 节假日沿用最近一个完成交易日。
- Quotes / Account / Agent 涉及行情有效性或即时成交判断时，必须使用该统一方法，不能各自手写时间段判断。
- 非交易时间行情不因 age 自动判为 stale，但必须返回 `capturedAt` 和 age。

---

## 4. Freshness

```ts
type Freshness = {
  status: "fresh" | "stale" | "missing";
  capturedAt?: OccurredAt;
  exchangeTime?: OccurredAt;
  ageMs?: number;
  source?: string;
  warning?: WarningCode;
};
```

规则：

- `missing` 表示本地读模型没有可用数据，或数据已超过模块定义的硬过期阈值而不可再作为可用事实返回。
- `stale` 表示本地有数据，但不满足当前用途的新鲜度要求。
- Quotes 负责根据交易日历和 snapshot age 计算行情 freshness。
- Account 交易写路径必须 fail closed：`stale` / `missing` quote 不得成交。
- Agent 不能把旧工具结果当作当前交易事实。

---

## 5. Warning 和 Error Code

```ts
type WarningCode =
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

type ErrorCode =
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
```

规则：

- `WarningCode` 和 `ErrorCode` 是封闭共享集合；新增机器可读 code 必须先修改本文件，再被模块 spec 引用。
- 对外接口的机器可读错误必须用 code；人类可读 message 只能作为补充。
- 模块不能临时发明新的机器可读 code；provider 原始错误、调试信息放入 `message` / `details` / `payload`。
- 批量读取使用 item 级 `warnings` / `errors`。
- 写接口失败必须返回单一主 `reason` code，可附带 details。

---

## 6. 应用事件

通用 JSON payload：

```ts
type JsonValue =
  | string
  | number
  | boolean
  | null
  | JsonValue[]
  | { [key: string]: JsonValue };
```

```ts
type AppEventEnvelope<T> = {
  eventId: string;
  type: string;
  occurredAt: OccurredAt;
  correlationId?: string;
  causationId?: string;
  payload: T;
};
```

跨模块事件 payload：

```ts
type NewsFailure = {
  provider: string;
  source?: string;
  code: ErrorCode;
  message?: string;
  details?: JsonValue;
  stage?: "fetch" | "normalize" | "save" | "article";
  retryable?: boolean;
  occurredAt: OccurredAt;
};

type NewsRefreshWarning = {
  provider: string;
  source?: string;
  code: WarningCode;
  message?: string;
  stage?: "fetch" | "normalize" | "save" | "article";
  skippedCount?: number;
  occurredAt: OccurredAt;
};

type NewsRefreshedPayload = {
  batchId: string;
  fetchedCount: number;
  skippedCount: number;
  savedCount: number;
  articleUpdatedCount: number;
  newIds: string[];
  updatedIds: string[];
  articleUpdatedNewsIds?: string[];
  failedCount: number;
  firstFailure?: NewsFailure;
  failures?: NewsFailure[];
  warnings?: NewsRefreshWarning[];
};

type MarketQuotesRefreshedPayload = {
  scope: "subscribed" | "universe" | "manual";
  purpose: "intraday" | "close";
  tradeDate?: TradeDate;
  affectedTsCodes?: TsCode[];
  total: number;
  success: number;
  failedBatches: number;
  capturedAt: OccurredAt;
};

// market-quotes-refresh-progress：universe scope 中间态进度。
// emit 节奏由 quotes-module.md "全市场 quote 刷新执行契约" 规定（默认每 200 只）。
// 消费者只用于增量列表刷新；终态仍以 MarketQuotesRefreshedPayload 为准。
type MarketQuotesRefreshProgressPayload = {
  scope: "universe";
  purpose: "intraday" | "close";
  tradeDate?: TradeDate;
  completed: number;        // 累计已完成数量（含失败）
  success: number;          // 累计成功数量
  total: number;            // 本轮目标总数
  affectedTsCodes: TsCode[];// 本次 progress 增量写入成功的标的（自上一个 progress 起）
  capturedAt: OccurredAt;
};

type AccountUpdatedPayload = {
  accountEventIds: string[];
  affectedOrderIds?: string[];
  affectedPositionIds?: string[];
  affectedTsCodes?: TsCode[];
  affectedWatchlistTsCodes?: TsCode[];
  triggerIds?: string[];
  snapshotCapturedAt: OccurredAt;
};

type AccountTriggeredPayload = {
  triggerId: string;
  positionId?: string;
  orderId?: string;
  tsCode?: TsCode;
  triggerType:
    | "stop_loss"
    | "take_profit"
    | "time_stop"
    | "order_filled"
    | "order_rejected"
    | "order_expired"
    | "invalidated";
  quoteFreshness?: Freshness;
  warnings?: WarningCode[];
};
```

规则：

- `eventId` 唯一标识一次事件事实。
- `eventId` 和 `occurredAt` 由事件发布 helper 在 emit 时生成。
- `correlationId` 串联一次用户请求、后台 run 或跨模块流程，由调用 use case / scheduler / 上游事件上下文传入；没有上下文时可省略。
- `causationId` 指向直接导致本事件的上游 event / command / run，由调用方传入；没有上游事实时可省略。
- 生产者只表达事实，不指定消费者。
- 消费者必须以模块规定的 event key 做幂等处理。
- 跨模块事件 payload 只在本文件定义一次；模块 spec 和 Agent Runtime 只引用类型名。

---

## 7. 通用分页和结果状态

```ts
type PageRequest = {
  limit?: number;
  offset?: number;
};

type PageInfo = {
  limit: number;
  offset: number;
  hasMore: boolean;
};

type ItemIssue = {
  code: WarningCode | ErrorCode;
  message?: string;
  field?: string;
  source?: string;
};
```

规则：

- 列表接口必须限制最大 `limit`，默认值由模块 spec 定义。
- `hasMore` 基于查询条件和 offset 计算，不依赖前端猜测。
- `ItemIssue.code` 是前端和 Agent 判断行为的依据。
