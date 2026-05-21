# Quotes 模块 Spec

> 本文档是 Quotes bounded context 的领域模型契约。`docs/design/architecture.md` 仍是架构权威基线。
>
> 本文档只描述最新设计目标，作为实现依据。

## 一句话定位

**市场数据本地读模型**：后台任务持续从 TDX / Eastmoney / 腾讯 / 新浪 / TuShare 补数据；对外读取只访问本地 `MARKET_SNAPSHOT`、cache 和读模型。

远端 provider 是 Quotes 内部实现细节，不暴露给对外读取 API。

契约强度：

- `MarketInstrument`、`MarketQuoteSnapshot`、K 线 / 分时 / 基本面 / 公司事件读模型、`list_market`、`fetch_data`、`scan_market` 是 `Spec-as-source`。
- provider fallback 顺序、后台刷新频率是 `Spec-anchored`。
- 新增市场研究能力必须先扩展本 spec 或单独新增模块 spec，不能塞进现有统一读取接口。

共享类型见 [shared-types.md](shared-types.md)。

---

## 1. 责任边界

Quotes 负责：

- 维护股票 / 指数 / 场内基金的统一标的 universe。
- 维护实时行情 snapshot。
- 维护日 / 周 / 月 K、分钟 K、分时、技术指标所需的本地读模型。
- 维护 `daily_basic` 和公司事件的本地读模型。
- 提供统一行情读取和扫描能力。
- 提供扫描能力：涨跌幅、成交额、成交量、量比、PE/PB、市值等。

Quotes 不负责：

- 新闻获取或新闻分析。
- 持仓、下单、现金、PnL。
- 投资决策、记忆、episode、学习闭环。
- 用核心行情读取接口承载龙虎榜、北向资金、融资融券、概念 / 板块等研究扩展能力。

---

## 2. 领域模型

### 核心概念

| 概念 | 含义 | 主键 / 身份 |
|---|---|---|
| `MarketInstrument` | 可交易 / 可展示标的：股票、指数、场内基金 | `ts_code` |
| `MarketQuoteSnapshot` | 某个标的的行情快照 | `ts_code` |
| `KlineSeries` | 日 / 周 / 月 OHLCV 序列 | `(ts_code, period, adjust)` |
| `MinuteKlineSeries` | 1m/5m/15m/30m/60m OHLCV 序列 | `(ts_code, period)` |
| `IntradaySeries` | 当日分时价格线 | `(ts_code, trade_date)` |
| `DailyBasic` | 每日估值 / 换手 / 市值基础指标 | `(ts_code, trade_date)` |
| `CompanyEvent` | 分红、停复牌、ST、业绩预告、解禁等公司动作 | `id` |
| `ScanResult` | 基于本地行情和基本面的筛选结果 | 查询派生 |

### 不变量

- Quotes 读写主键只使用 `ts_code`，格式如 `600519.SH` / `000001.SH` / `510300.SH`。
- 对外查询、缓存 key、跨模块引用和 provider 路由都必须使用 `TsCode`；Quotes 不提供非 `TsCode` 标识的市场推断能力。
- `MarketInstrument.category` 决定该标的是 `stock` / `index` / `fund`，所有扫描、breadth、K 线 provider 分流都以它为准。
- 对外读取默认只读本地 snapshot/cache/DB，不在热路径直接请求 provider。
- 任何数据缺失不让整批请求失败；在 item 上返回 `warnings` / `errors`。
- source / freshness 必须可解释：调用方要知道数据来自哪里、是否 stale。

### 统一标的模型

股票、指数、基金统一建模为 `MarketInstrument`。实现可以使用任意持久化结构，但对外和跨模块只认这个模型。

```ts
type MarketInstrument = {
  tsCode: TsCode;
  name: string;
  category: InstrumentCategory;
  market: Market;
  sector?: string;
  status?: InstrumentStatus;
  publisher?: string;
  indexCategory?: string;
  fundType?: string;
  management?: string;
  listDate?: string;
  source: "tdx" | "eastmoney" | "tushare" | "mixed";
  updatedAt: string;
};
```

规则：

- `tsCode` 是唯一身份。
- 股票、指数、基金不能拆成互不兼容的平行主模型。
- `category` 决定 provider 分流、扫描范围和 breadth 统计。
- TuShare / TDX / Eastmoney 等来源只能 enrich 该模型，不能改变 `tsCode` 身份。
- `source` 表示标的档案 / universe 来源，不表示实时行情来源；实时行情来源以 `StockQuote.source` 和 `StockQuote.freshness.source` 为准。

### 行情快照

`MARKET_SNAPSHOT` 存带元数据的 snapshot item：

```ts
type MarketQuoteSnapshot = {
  tsCode: TsCode;
  category: InstrumentCategory;
  quote: StockQuote;
  updatedAt: OccurredAt;
};
```

`MARKET_SNAPSHOT` 语义上是进程内 snapshot cache，不是持久化真源。重启后由后台 refresh / cache hydrate 重新填充；长期历史、K 线、基本面和公司事件仍以本地读模型为准。

`MarketQuoteSnapshot` 不对外暴露 freshness 字段。实现可以为索引或 cache 存储 `tradeDate` / `capturedAt` / `source`，但对外 freshness 只能由 query facade 派生到 `StockQuote.freshness` 或 item-level `quoteFreshness`。

用途：

- breadth 只统计 `category == stock`。
- 调用方通过 `StockQuote` / `quoteFreshness` 能看到 source / freshness。
- fallback 源字段缺失时可解释。

freshness 定义：

- `capturedAt` 表示本地成功获取并写入该 quote snapshot 的时间，不表示交易所成交时间。
- provider / refresh 成功写入 snapshot 时更新 `capturedAt`、`exchangeTime`、`source`；读取路径不能刷新 `capturedAt`，也不能因为被读取而延长有效期。
- `exchangeTime` 只用于审计和展示市场时间，不能替代 `capturedAt` 或 `tradeDate` 做有效性判断。
- `tradeDate` 表示该 quote 对应的交易日。Quotes 通过 `resolve_market_time(now)` 返回的 `MarketTimeContext` 和 `tradeDate` 推导它是盘中事实还是收盘事实，不额外暴露 snapshot kind 枚举。
- `MarketTimeContext.isTradingTime = true` 时，读取必须使用 `tradeDate = currentTradeDate` 的 quote；`now - capturedAt > quote_stale_threshold_secs` 时 `freshness.status = "stale"`，默认阈值为 30s。
- `MarketTimeContext.isTradingTime = true` 时，`now - capturedAt > quote_snapshot_expire_secs` 的当日 quote 硬过期；默认阈值为 1 小时。硬过期时 `quote` 不返回可用行情字段，`quoteFreshness.status = "missing"`，`quoteFreshness.warning = "snapshot_expired"`。
- `MarketTimeContext.isTradingTime = false` 时，读取优先使用 `tradeDate = latestCompletedTradeDate` 的 quote；只要 trade date 匹配最新已完成交易日，就视为收盘事实，不因 `capturedAt > 1h` 过期。
- 非交易时段如果缺少最新已完成交易日的 close snapshot，则 quote 为空并返回 `snapshot_expired` 或 `quote_missing`；不能退回更早交易日的旧 quote。
- 默认阈值不因 TDX / Eastmoney / 腾讯 / 新浪 source 改变；如配置 source-specific threshold，响应必须保留 source 以便审计。

行情 DTO：

```ts
type QuoteDepthLevel = {
  price?: Price;
  volume?: Volume;
};

type StockQuote = {
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
  tradeStatus: "trading" | "halted" | "closed" | "unknown";
  source: string;
  capturedAt: OccurredAt;
  exchangeTime?: OccurredAt;
  freshness: Freshness;
  warnings?: WarningCode[];
};
```

规则：

- `price` 是当前最新价；不能用 `previousClose` 伪造当前价。
- instrument `status = suspended` 时 `tradeStatus` 必须为 `halted`；instrument `status = delisted` 时 `tradeStatus` 必须为 `closed` 或 `unknown`。
- `tradeStatus = halted` / `closed` / `unknown` 时 Account 写路径必须 fail closed，不能即时成交。
- Quotes 提供 L1 五档盘口能力：`bid` 表示买一到买五，`ask` 表示卖一到卖五，数组按离成交价最近到最远排序，最多 5 档。
- `QuoteDepthLevel.volume` 使用 shared `Volume` 规范化单位；价格或数量缺失的档位不得伪造为 0。
- 缺盘口、盘口为空、买一 / 卖一价格缺失时必须返回 `depth_missing` warning。
- 少于 5 档但买一 / 卖一可用时仍返回已有档位，并返回 `data_partial` warning；Quotes 不在本模块判断某笔订单需要消耗几档盘口。
- TDX 是五档盘口主路径；Eastmoney / Tencent 可补充可用盘口；Sina 不保证盘口，通常只可作为基础展示 fallback。
- `limitUp` / `limitDown` 优先由 Quotes 基于 `previousClose`、市场板块、ST 状态和 A 股涨跌幅规则计算；provider 返回值只能作为校验或补充。缺少计算所需字段时可为空，并必须返回 `quote_price_missing` warning；Account 不得执行涨跌停相关判断。
- `changePercent` 使用百分点，例如 `3.25` 表示上涨 3.25%。

### K 线和分时读模型

K 线和分时是 Quotes 的本地读模型，不是 provider 原始数据直出。

身份规则：

- 日 / 周 / 月 K：`(tsCode, period, adjust, date)` 唯一。
- 分钟 K：`(tsCode, period, timestampMs)` 唯一。
- 分时点：`(tsCode, tradeDate, time)` 唯一。
- 本地读模型必须记录 source / fetchedAt；对外日 / 周 / 月 K 通过 `KlineSeries.freshness` 暴露统一 freshness。

对外点位模型：

```ts
type KlinePoint = {
  date: TradeDate;
  open: Price;
  close: Price;
  high: Price;
  low: Price;
  volume?: Volume;
  amount?: Amount;
};

type KlineSeries = {
  period: "day" | "week" | "month";
  adjust: "none" | "qfq" | "hfq";
  points: KlinePoint[];
  freshness: Freshness;
  warnings?: WarningCode[];
};

type MinuteKlinePoint = {
  timestampMs: TimestampMs;
  open: Price;
  close: Price;
  high: Price;
  low: Price;
  volume: Volume;
  amount: Amount;
  source: string;
  fetchedAt: OccurredAt;
};

type MinutePoint = {
  tradeDate: TradeDate;
  time: string; // HH:mm
  price: Price;
  average?: Price;
  volume?: Volume;
  amount?: Amount;
  source: string;
  fetchedAt: OccurredAt;
};

type IndicatorName =
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

type IndicatorSnapshot = {
  tsCode: TsCode;
  basis: {
    period: "day" | "week" | "month";
    adjust: "none" | "qfq" | "hfq";
    fetchedAt: OccurredAt;
  };
  values: Partial<Record<IndicatorName, number | null>>;
  warnings?: WarningCode[];
};
```

规则：

- 指标名只能使用固定 `IndicatorName`，新增指标必须先扩展 spec。
- 指标基于本地 K 线现算，不作为持久化真源。
- 趋势 / 指标默认使用 `qfq` 日 K；只能使用 `none` 时必须返回 `using_unadjusted_kline`。
- 指标参数和公式是字段名语义的一部分；同名指标不得因实现或 provider 改变参数。
- 修改既有指标参数属于破坏性变更，必须新增 `IndicatorName` 或 bump schema，不能静默改变同名字段含义。

指标参数契约：

| IndicatorName | 参数 / 公式 |
|---|---|
| `ma5` / `ma10` / `ma20` / `ma60` | `close` 简单移动平均，窗口分别为 5 / 10 / 20 / 60 |
| `ema12` / `ema26` | `close` EMA，span 分别为 12 / 26 |
| `macd_dif` | `ema12 - ema26` |
| `macd_dea` | `macd_dif` 的 EMA，span = 9 |
| `macd_hist` | `2 * (macd_dif - macd_dea)` |
| `rsi6` / `rsi12` / `rsi24` | Wilder RSI，窗口分别为 6 / 12 / 24，基于 `close` 变化 |
| `kdj_k` / `kdj_d` / `kdj_j` | RSV window = 9，K smoothing = 3，D smoothing = 3，初始 K/D = 50，J = `3*K - 2*D` |
| `boll_mid` | `close` MA20 |
| `boll_upper` / `boll_lower` | MA20 ± `2 * population_stddev(close, 20)` |
| `volume_ma5` / `volume_ma10` | `volume` 简单移动平均，窗口分别为 5 / 10 |

规则：

- 指标计算使用 `IndicatorSnapshot.basis.period` 指定的 K 线周期；默认趋势判断使用 `period = "day"`。
- 窗口不足时对应指标值为 `null`，不能用 0 代替。
- 所有指标值只基于本地 canonical K 线点位计算，不直接采用 provider 自带指标。

### 基本面和事件读模型

`DailyBasic` 和 `CompanyEvent` 是 Quotes 的本地读模型。它们只表达市场事实和公司动作，不表达投资判断。

身份规则：

- `DailyBasic` 以 `(tsCode, tradeDate)` 唯一。
- `CompanyEvent` 以 `id` 唯一，并必须关联 `tsCode`。
- 事件按 `announceDate` / `effectiveDate` 支持时间窗口查询。
- provider 原始扩展字段放入 `payload`，但对外必须保留稳定的 `eventType`。

```ts
type DailyBasic = {
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
};

type CompanyEvent = {
  id: string;
  tsCode: TsCode;
  eventType:
    | "dividend"
    | "suspension"
    | "resume"
    | "st"
    | "earnings_forecast"
    | "unlock"
    | "other";
  announceDate?: TradeDate;
  effectiveDate?: TradeDate;
  payload: JsonValue;
  source: string;
  fetchedAt: OccurredAt;
};

type StockProfile = {
  tsCode: TsCode;
  name: string;
  category: InstrumentCategory;
  market: Market;
  sector?: string;
  status?: InstrumentStatus;
  listDate?: string;
};

type ScanCondition = {
  field:
    | "changePercent"
    | "amount"
    | "volume"
    | "turnoverRate"
    | "volumeRatio"
    | "peTtm"
    | "pb"
    | "totalMv"
    | "circMv";
  op: "gt" | "gte" | "lt" | "lte" | "eq" | "between";
  value: number | [number, number];
};

type ScanResult = {
  generatedAt: OccurredAt;
  universe: {
    category?: InstrumentCategory;
    total: number;
    matched: number;
  };
  criteria: {
    filter?: string;
    conditions?: ScanCondition[];
    sortBy?: string;
    limit: number;
  };
  items: Array<{
    rank: number;
    tsCode: TsCode;
    name?: string;
    category: InstrumentCategory;
    quote?: StockQuote;
    dailyBasic?: DailyBasic;
    score?: number;
    warnings?: WarningCode[];
  }>;
  warnings?: WarningCode[];
};
```

规则：

- 扫描只能基于本地 snapshot / `daily_basic` / K 线派生数据。
- 扫描使用 quote 字段时必须先应用 quote 有效性规则：`isTradingTime = true` 时使用 `tradeDate = currentTradeDate` 且未硬过期的 quote；`isTradingTime = false` 时使用 `tradeDate = latestCompletedTradeDate` 的 quote。无有效 snapshot 的 item 不能参与排名、条件判断或 breadth 统计。
- `ScanCondition.field` 不允许自由字符串；新增字段必须先扩展 spec。
- `conditions` 内部按 AND 组合；任一条件不满足则该 item 不进入结果。
- 条件字段缺失时该 item 不匹配该条件，并在响应级 `warnings` 返回 `data_partial`；不能把缺失当作 0。
- `filter` 是预设筛选模板；与 `conditions` 同时出现时先应用 `filter`，再按 AND 应用 `conditions`。
- `sortBy` 显式传入时覆盖 `filter` 的默认排序；未传入时使用 `filter` 对应默认排序，再按 `tsCode` 稳定 tie-breaker。
- 连续竞价时段内未硬过期但 freshness 为 `stale` 的 quote 可以参与扫描，但该 item 必须带 `quote_stale` warning。

---

## 3. 数据流

### 写入流

```text
provider fetch
  -> infrastructure/quotes provider adapter
  -> normalize to domain/read-model rows
  -> local read model / cache or MARKET_SNAPSHOT
  -> emit market data events when needed
```

### 读取流

```text
external read request
  -> adapters command DTO
  -> pipeline / infrastructure query facade
  -> MARKET_SNAPSHOT / local cache / read model
  -> response with source/freshness/warnings
```

规则：

- `list_market` 只读 `MarketInstrument` 本地读模型，可左连接 `MARKET_SNAPSHOT`。
- `fetch_data` 只读本地 snapshot/cache/DB。
- 读取 quote 时必须先应用 quote 有效性规则；无有效当日盘中 quote 或最新已完成交易日 quote 时不返回旧行情字段。
- `scan_market` 基于本地行情和 `daily_basic` 选出候选，不返回 K 线、分时、公司事件等详情。
- 技术指标不存储，基于 K 线读模型现算。

查询条件规则：

- `fetch_data` 的目标来源必须是 `tsCodes`；缺失或为空时必须返回 `invalid_input`。
- `tsCodes` 是标准 `TsCode` 精确查询条件，原样贯穿，不做二次解析。
- 名称搜索只属于 `list_market({ query })`；调用方需要详情时必须先解析出 `TsCode`，再调用 `fetch_data({ tsCodes })`。
- 非 `TsCode` 标识解析不属于 `fetch_data` 查询条件；读取详情必须使用标准 `TsCode`。

---

## 4. 对外接口

### 读取接口

Quotes 对外暴露三类读取 command：

| Command | 用途 | 读取路径 |
|---|---|---|
| `list_market` | 股票 / 指数 / 场内基金全列表，可选携带实时行情摘要 | `MarketInstrument` 本地读模型 + `MARKET_SNAPSHOT` |
| `fetch_data` | 按 `tsCodes` 读取行情、K 线、分时、分钟 K、详情、基本面、公司事件 | 本地 snapshot / cache / DB |
| `scan_market` | 从本地 universe 扫描候选标的，返回轻量排名结果 | 本地 snapshot / `daily_basic` / K 线派生数据 |

#### `list_market`

```ts
type ListMarketRequest = {
  category?: "stock" | "index" | "fund";
  query?: string;
  includeQuote?: boolean;
  limit?: number;
  offset?: number;
};

type ListMarketResponse = {
  items: Array<MarketInstrument & {
    quote?: {
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
    };
    quoteFreshness?: Freshness;
    warnings?: WarningCode[];
  }>;
  page: {
    limit: number;
    offset: number;
    hasMore: boolean;
  };
};
```

约束：

- `query` 只匹配标准 `TsCode` 和 `MarketInstrument.name`；非 `TsCode` 标识不参与匹配。
- `query` 匹配前必须 trim、折叠连续空白；`TsCode` 匹配大小写不敏感，名称按 substring 匹配。
- `query` 排序优先级：exact `tsCode`、exact name、name prefix、name substring、`status = listed`、`category`、`tsCode`。
- `query` 未命中时返回空 `items`；调用方需要精确查询标的时必须提供完整 `TsCode`。
- `limit` 默认 100，最大 500；`offset` 默认 0；排序必须稳定，避免翻页重复或漏项。
- `includeQuote = true` 时只读 `MARKET_SNAPSHOT`，不触发外部 provider。
- `quote` 是列表页摘要字段，不包含五档盘口。
- `quoteFreshness` 使用共享 `Freshness`，不能另设 `stale` boolean 作为第二套 freshness 表达。
- `MARKET_SNAPSHOT` 缺某个标的时，该标的 `quote` 为空，并在 `warnings` 标记 `quote_missing`。
- 连续竞价时段内，`MARKET_SNAPSHOT` 存在但当日 quote `capturedAt` 超过 1 小时时，该标的 `quote` 为空，`quoteFreshness.status = "missing"`，`quoteFreshness.warning = "snapshot_expired"`；不得返回旧价格、旧涨跌幅或旧成交额。
- 非交易时段内，`list_market` 可返回 `tradeDate = latestCompletedTradeDate` 的 quote；该 quote 不因 `capturedAt > 1h` 过期，但必须带 `tradeDate`。

#### `fetch_data`

```ts
type ResponseError = {
  code: ErrorCode;
  message?: string;
  field?: string;
};

type FetchDataRequest = {
  tsCodes?: TsCode[];
  include?: {
    quote?: boolean;
    intraday?: boolean;
    klines?: Array<"day" | "week" | "month">;
    minuteKlines?: Array<"1m" | "5m" | "15m" | "30m" | "60m">;
    indicators?: true | IndicatorName[];
    profile?: boolean;
    dailyBasic?: boolean;
    events?: boolean;
  };
  limit?: {
    kline?: number;
    minuteKline?: number;
    intradayDays?: number;
    eventsDaysAhead?: number;
    instruments?: number;
  };
};

type FetchDataResponse = {
  errors?: ResponseError[];
  items: Array<{
    tsCode: TsCode;
    category: "stock" | "index" | "fund";
    name?: string;
    quote?: StockQuote;
    quoteFreshness?: Freshness;
    intraday?: MinutePoint[];
    klines?: Partial<Record<"day" | "week" | "month", KlineSeries>>;
    minuteKlines?: Partial<Record<"1m" | "5m" | "15m" | "30m" | "60m", MinuteKlinePoint[]>>;
    indicators?: IndicatorSnapshot;
    profile?: StockProfile;
    dailyBasic?: DailyBasic;
    events?: CompanyEvent[];
    sources?: Record<string, string>;
    warnings?: WarningCode[];
    errors?: ErrorCode[];
  }>;
};
```

规则：

- `tsCodes` 是唯一目标来源；缺失或为空时返回顶层 `errors[]`，`code = "invalid_input"`，且 `items = []`。
- `tsCodes` 是精确身份查询，所有 item 必须返回标准 `tsCode`。
- 名称模糊搜索必须走 `list_market({ query })`；`fetch_data` 不做名称匹配，避免详情读取出现多义结果。
- `include` 缺失时默认等同于 `{ profile: true, quote: true }`。
- `include.klines` / `include.minuteKlines` 只返回调用方请求的周期；未请求的周期 key 不出现在响应中，不能用空数组伪装为“已请求但无数据”。
- `include.klines` 默认优先返回 `adjust = "qfq"`；缺少 qfq 时可降级为 `adjust = "none"`，必须在对应 `KlineSeries.warnings` 和 item `warnings` 返回 `using_unadjusted_kline`。
- `include.quote = true` 时，如果 snapshot 缺失，`quote` 为空并返回 `quote_missing` warning。
- `include.quote = true` 时，必须返回 `quoteFreshness`；有可用 `quote` 时它与 `quote.freshness` 语义一致，`quote` 为空时它承载缺失 / 过期原因。
- 如果连续竞价时段内当日 quote 已超过 1 小时硬过期，`quote` 为空，`quoteFreshness.status = "missing"`，`quoteFreshness.warning = "snapshot_expired"`；不得把硬过期行情降级塞入 `quote`。
- `include.quote = true` 时，如果当前为非交易时段，可返回 `tradeDate = latestCompletedTradeDate` 的 quote；缺少该交易日 quote 时返回空 quote 和 `snapshot_expired` / `quote_missing` warning。
- `include.indicators = true` 返回完整默认指标集合；`include.indicators = IndicatorName[]` 只返回请求的指标子集，未知指标名必须返回 `invalid_input`。
- `fetch_data` 不触发远端 provider；缺失、过期或字段不足只通过 item warning / error 表达。刷新必须走显式 refresh use case 或后台任务。

#### `scan_market`

```ts
type ScanMarketRequest = {
  category?: InstrumentCategory;
  filter?: "limit_up" | "limit_down" | "top_gain" | "top_loss" | "top_amount" | "top_volume";
  conditions?: ScanCondition[];
  sortBy?: "change_pct_desc" | "change_pct_asc" | "amount_desc" | "volume_desc" | "turnover_rate_desc";
  limit?: number;
};

type ScanMarketResponse = ScanResult & {
  errors?: ResponseError[];
};
```

规则：

- `scan_market` 只负责发现候选标的，不返回 K 线、分钟 K、分时、公司事件等详情。
- `scan_market` 的返回项可以包含用于筛选和展示的 `quote` / `dailyBasic` / `score` / `rank`。
- 调用方需要深入分析候选标的时，必须再用 `fetch_data({ tsCodes })` 读取详情。
- `scan_market` 使用 quote 字段时必须先应用 quote 有效性规则；`isTradingTime = true` 时使用 `tradeDate = currentTradeDate` 且未硬过期的 quote，`isTradingTime = false` 时使用 `tradeDate = latestCompletedTradeDate` 的 quote。
- `scan_market` 不触发远端 provider；缺失、过期或字段不足只通过 item warning / response warning 表达。

### 内部 Rust API

内部 API 以 query facade 为主：

```rust
list_market(request) -> ListMarketResponse;
fetch_data(request) -> FetchDataResponse;
scan_market(request) -> ScanMarketResponse;
refresh_market_instruments();
refresh_market_quotes(scope);
refresh_klines(scope);
refresh_daily_basic(scope);
refresh_company_events(scope);
core_indexes() -> Vec<TsCode>;
```

---

## 5. 模块独有功能

### Provider 策略

Provider reference：

- [TDX](references/quotes/tdx.md)
- [Eastmoney](references/quotes/eastmoney.md)
- [TuShare](references/quotes/tushare.md)
- [Tencent](references/quotes/tencent.md)
- [Sina](references/quotes/sina.md)

规则：

- 本 spec 定义 provider 选择策略和 canonical contract。
- 具体连接方式、字段映射、单位转换、timeout、retry 写在 provider reference。
- 所有 provider 输出必须 normalize 到 `MarketInstrument`、`StockQuote`、`KlineSeries` / K 线读模型行、`DailyBasic` 或 `CompanyEvent`。
- Provider 失败默认是 item / batch 级 partial failure，不改变对外读取契约。
- 当前 provider 集合保留 TDX / Eastmoney / 腾讯 / 新浪 / TuShare；后续可以继续扩展 provider，但新增 provider 必须先补 reference 文档，并 normalize 到本 spec 的 canonical model。

全市场列表：

1. TDX 主源：启动 / 每日 08:30 拉基础 SH / SZ universe。
2. Eastmoney 补 BJ / TDX 缺失标的。
3. TuShare enrich：有 token 时补行业、上市状态、指数分类、基金类型、管理人、上市日期等。

实时行情：

```text
TDX > Eastmoney > 腾讯 > 新浪
```

- TDX 是 SH / SZ 实时报价主路径。
- Eastmoney 是 BJ 主路径，也是 TDX 失败或缺字段时的第一 fallback。
- Tencent / Sina 只作为基础展示 fallback，不能覆盖更新鲜且字段更完整的 snapshot。
- fallback 选择以单个 provider 的完整 normalized quote 为单位；默认不做跨 provider 字段拼接。若未来引入 field-level merge，必须显式标记 `source = "mixed"` 并提供字段来源审计。
- Account 成交模拟需要 fresh quote 和盘口；fallback 源缺盘口时必须返回 `depth_missing`，是否可成交由 Account 交易规则判断。

日 / 周 / 月 K：

- TDX 能获取日 / 周 / 月 K，但不支持 BJ、不复权、单次根数有限。
- TDX 补快速展示用的 `adjust = none`。
- TuShare 补长历史和 `qfq` / `hfq`。
- 股票趋势 / 技术指标优先使用 `qfq`；没有 `qfq` 时使用 `none` 并返回 warning。

分钟 K / 分时：

```text
TDX > Eastmoney
```

BJ 可以不支持，返回 per-item warning / error。

基本面 / 公司事件 / 交易日历：

- TuShare 是 `daily_basic`、公司事件和交易日历主源。
- TuShare token 缺失时，这些读模型保持旧数据并返回 freshness / warning。
- TuShare 失败不得影响实时行情 refresh。

### 复权策略

复权用于消除分红、送股、转增、配股导致的历史价格断层。

| 模式 | 含义 | 用途 |
|---|---|---|
| `none` | 原始成交价，不复权 | 盘口附近、短线真实价格 |
| `qfq` | 前复权，当前价格不变，历史价格修正 | 图表展示、趋势、技术指标 |
| `hfq` | 后复权，早期价格不变，后续价格修正 | 长期收益率研究 |

规则：

- TDX K 线默认 `adjust = none`。
- TuShare 可补 `qfq` / `hfq`。
- K 线展示优先 `qfq`，没有则用 `none`。
- 趋势 / 技术指标判断优先 `qfq`。
- 只能用 `none` 时必须返回 warning：

```text
不复权，除权除息附近的跳空可能扭曲趋势和技术指标
```

### 后台刷新

Quotes 提供 refresh use case；触发节奏和 scope 由模块外运行时传入，Quotes 不关心 scope 来源。下表是推荐默认值，实际调度权威写在 [orchestration.md](orchestration.md)。

| 数据 | 策略 |
|---|---|
| 全市场列表 | 启动 + 每日 08:30：TDX 基础 universe；TuShare 可用时 enrich |
| 实时行情 | 连续竞价时段：关注标的 + 核心指数 15s，全市场 universe 60s |
| 收盘快照 | 收盘后执行全市场 quote refresh，写入 `tradeDate = latestCompletedTradeDate` 的最终行情；失败时可低频重试直到获得最新已完成交易日快照，不做整夜持续刷新 |
| K 线 | 启动后预热关注标的；盘后 16:00 补日周月；TuShare 可用时补复权 |
| `daily_basic` | 每个交易日盘后刷新 |
| `company_events` | 每日低频刷新，覆盖未来 N 天事件窗口 |

规则：

- Quotes 不读取其他 bounded context 的内部实现。
- Quotes refresh scope 是 use case 入参。
- 收盘快照用于维护展示 / 分析可用的最后行情事实，不表示可交易。Account 仍必须按交易日历和交易时段规则禁止即时成交。
- 非交易时段不为了维持 `capturedAt < 1h` 持续刷新 quote；只要 quote 的 `tradeDate` 等于最新已完成交易日，就可用于读取。
- 如果 app 暂停、网络不可用或 provider 失败导致缺少最新已完成交易日 quote，非交易时段读取接口按 `snapshot_expired` / `quote_missing` 返回空 quote。
- `market-quotes-refreshed` 只表示 snapshot 已更新；payload 使用 [shared-types.md](shared-types.md) 定义的 `MarketQuotesRefreshedPayload`；下游重建和事件路由由模块外编排处理。

### 核心指数集合

Quotes 拥有默认核心指数集合，并通过 `core_indexes()` 暴露给外部调度：

```text
000001.SH  上证指数
399001.SZ  深证成指
399006.SZ  创业板指
000300.SH  沪深300
```

规则：

- 外部调度只调用 `core_indexes()` 合并 refresh scope，不内嵌指数列表。
- 核心指数变更属于 Quotes 配置 / 数据契约变更。

### 研究扩展能力边界

龙虎榜、资金流、北向资金、融资融券、概念 / 板块等属于市场研究扩展能力，不进入 `fetch_data`。

规则：

- 若接入这些 provider adapter，必须走独立读取能力，例如 `fetch_market_research`，或新增单独模块 spec。
- `fetch_data` 只承载本 spec 定义的行情、K 线、分时、指标、基础估值和公司事件。
- `scan_market` 只承载本 spec 定义的本地行情 / 基本面扫描。
- 研究扩展能力不能改变 `MarketInstrument`、`MarketQuoteSnapshot`、`DailyBasic` 的核心语义。
- 研究扩展能力返回的 provider 原始字段必须 normalize 到独立 canonical model，不能临时扩展 `StockQuote` 或 `DailyBasic` 的核心 DTO。

---

## 6. 验收标准 / 例子

- 股票 / 指数 / 基金 universe 统一建模为 `MarketInstrument`；实现中不新增互不兼容的平行主模型。
- `list_market`、`fetch_data` 和 `scan_market` 是读取 quotes 的统一入口。
- 对外读取路径默认不直接请求 TDX / EM / TuShare / 腾讯 / 新浪。
- `list_market({ includeQuote: true })` 只读取 `MarketInstrument` 本地读模型 + `MARKET_SNAPSHOT`，缺实时字段时 `quote` 为空，不触发远端补拉。
- 连续竞价时段超过 1 小时的当日 quote 不得返回；非交易时段可返回最新已完成交易日 quote。
- `fetch_data({ tsCodes, include })` 只读本地 DB / snapshot；需要远端刷新必须走显式 refresh / 后台任务。
- `scan_market` 返回候选排名结果；需要详情时再调用 `fetch_data({ tsCodes })`。
- `MARKET_SNAPSHOT` item 带 `category/tradeDate/capturedAt/source`，对外 freshness 由 query facade 派生；breadth 只统计 `category == stock`。
- K 线读取必须使用 `TsCode`；已知 `ts_code` 必须贯穿到 provider/cache。
- `DailyBasic` 和 `CompanyEvent` 由本地读模型读取，远端拉取只发生在后台刷新 / 显式 refresh 路径。
- 所有批量返回都是 per-item warning/error；单个标的缺数据不让整批失败。
- 指标计算优先使用 `qfq` K 线；只能使用 `none` 时返回复权 warning。
- refresh scope 注入是模块外编排职责，不是 Quotes 反向读取其他模块。
- 依赖方向自检为空：quotes 任一层不 import 其他 bounded context 代码。

---

## 7. 模块边界外

这些能力不属于 `fetch_data`：

- 龙虎榜
- 个股资金流
- 北向资金
- 融资融券
- 概念 / 板块

如需这些能力，单独设计 `fetch_market_research` 或独立模块 spec，避免 `fetch_data` 变成不可维护的大杂烩。
