# Quotes 模块 Spec

> 本文档是 Quotes bounded context 的领域模型契约。`docs/design/architecture.md` 仍是架构权威基线。
>
> 本文档只描述最新设计目标，作为实现依据。

## 一句话定位

**市场数据本地读模型**：后台任务持续从 TDX / Eastmoney / 腾讯 / 新浪 / TuShare 补数据，UI 和 Agent 只读本地 `MARKET_SNAPSHOT` / SQLite cache / DB。

远端 provider 是 Quotes 内部实现细节，不暴露给前端或 Agent。

---

## 1. 责任边界

Quotes 负责：

- 维护股票 / 指数 / 场内基金的统一标的 universe。
- 维护实时行情 snapshot。
- 维护日 / 周 / 月 K、分钟 K、分时、技术指标所需的本地读模型。
- 维护 `daily_basic` 和公司事件的本地读模型。
- 提供前端展示接口和 Agent 行情工具。
- 提供扫描能力：涨跌幅、成交额、成交量、量比、PE/PB、市值等。

Quotes 不负责：

- 新闻获取或新闻分析。
- 持仓、下单、现金、PnL。
- Agent 决策、记忆、episode、学习闭环。
- 龙虎榜、北向资金、融资融券、概念 / 板块等研究扩展能力。

---

## 2. 领域模型

### 核心概念

| 概念 | 含义 | 主键 / 身份 |
|---|---|---|
| `MarketInstrument` | 可交易 / 可展示标的：股票、指数、场内基金 | `ts_code` |
| `MarketQuoteSnapshot` | 某个标的的实时行情快照 | `ts_code` |
| `KlineSeries` | 日 / 周 / 月 OHLCV 序列 | `(ts_code, period, adjust)` |
| `MinuteKlineSeries` | 1m/5m/15m/30m/60m OHLCV 序列 | `(ts_code, period)` |
| `IntradaySeries` | 当日分时价格线 | `(ts_code, trade_date)` |
| `DailyBasic` | 每日估值 / 换手 / 市值基础指标 | `(ts_code, trade_date)` |
| `CompanyEvent` | 分红、停复牌、ST、业绩预告、解禁等公司动作 | `id` |
| `ScanResult` | 基于本地行情和基本面的筛选结果 | 查询派生 |

### 不变量

- Quotes 读写主键优先使用 `ts_code`，格式如 `600519.SH` / `000001.SH` / `510300.SH`。
- `StockCode` 只代表 6 位输入 / 股票业务概念；不能用 `StockCode::to_ts_code()` 给指数、基金或已知 `ts_code` 的路径重新猜市场。
- `MarketInstrument.category` 决定该标的是 `stock` / `index` / `fund`，所有扫描、breadth、K 线 provider 分流都以它为准。
- UI / Agent 默认只读本地 snapshot/cache/DB，不在热路径直接请求 provider。
- 任何数据缺失不让整批请求失败；在 item 上返回 `warnings` / `errors`。
- source / freshness 必须可解释：前端和 Agent 要知道数据来自哪里、是否 stale。

### 统一标的表

股票、指数、基金统一存储在一张主表：

```sql
create table market_instruments (
    ts_code text primary key,
    code text not null,
    name text not null,
    category text not null,          -- stock / index / fund
    market text not null,            -- SH / SZ / BJ

    sector text,                     -- stock: 行业
    status text,                     -- listed / suspended / delisted / unknown

    publisher text,                  -- index
    index_category text,             -- index
    fund_type text,                  -- fund
    management text,                 -- fund
    list_date text,

    source text not null,            -- tdx / tushare / mixed
    updated_at text not null
);

create index idx_market_instruments_category on market_instruments(category);
create index idx_market_instruments_code on market_instruments(code);
create index idx_market_instruments_name on market_instruments(name);
```

对外模型：

```ts
type MarketInstrument = {
  tsCode: string;
  code: string;
  name: string;
  category: "stock" | "index" | "fund";
  market: "SH" | "SZ" | "BJ";
  sector?: string;
  status?: "listed" | "suspended" | "delisted" | "unknown";
  publisher?: string;
  indexCategory?: string;
  fundType?: string;
  management?: string;
  listDate?: string;
  source: "tdx" | "tushare" | "mixed";
  updatedAt: string;
};
```

### 实时快照

`MARKET_SNAPSHOT` 存带元数据的 snapshot item：

```rust
pub struct MarketQuoteSnapshot {
    pub ts_code: TsCode,
    pub category: InstrumentCategory,
    pub quote: StockQuote,
    pub source: QuoteSource,
    pub captured_at: OccurredAt,
    pub exchange_time: Option<OccurredAt>,
    pub stale: bool,
}
```

用途：

- breadth 只统计 `category == stock`。
- UI / Agent 能看到 source / freshness。
- fallback 源字段缺失时可解释。

### K 线和分时读模型

```sql
create table klines (
    ts_code text not null,
    category text not null,
    period text not null,            -- day / week / month
    adjust text not null,            -- none / qfq / hfq
    date text not null,              -- YYYYMMDD
    open real not null,
    close real not null,
    high real not null,
    low real not null,
    volume real,
    amount real,
    source text not null,            -- tdx / tushare / em
    warning text,
    fetched_at text not null,
    primary key (ts_code, period, adjust, date)
);

create table minute_klines (
    ts_code text not null,
    period text not null,            -- 1m / 5m / 15m / 30m / 60m
    timestamp_ms integer not null,
    open real not null,
    close real not null,
    high real not null,
    low real not null,
    volume integer not null,
    amount real not null,
    source text not null,            -- tdx / em
    fetched_at text not null,
    primary key (ts_code, period, timestamp_ms)
);

create table intraday_points (
    ts_code text not null,
    trade_date text not null,
    time text not null,
    price real not null,
    average real,
    volume integer,
    amount real,
    source text not null,
    fetched_at text not null,
    primary key (ts_code, trade_date, time)
);
```

### 基本面和事件读模型

```sql
create table daily_basic (
    ts_code text not null,
    trade_date text not null,
    pe real,
    pe_ttm real,
    pb real,
    ps real,
    ps_ttm real,
    turnover_rate real,
    turnover_rate_float real,
    volume_ratio real,
    total_mv real,
    circ_mv real,
    source text not null,
    fetched_at text not null,
    primary key (ts_code, trade_date)
);

create table company_events (
    id text primary key,
    ts_code text not null,
    event_type text not null,
    announce_date text,
    effective_date text,
    payload_json text not null,
    source text not null,
    fetched_at text not null
);

create index idx_company_events_ts_code on company_events(ts_code);
create index idx_company_events_effective_date on company_events(effective_date);
```

---

## 3. 数据流

### 写入流

```text
provider fetch
  -> infrastructure/quotes provider adapter
  -> normalize to domain/read-model rows
  -> SQLite cache / DB or MARKET_SNAPSHOT
  -> emit market data events when needed
```

### 读取流

```text
UI / Agent
  -> adapters command/tool DTO
  -> pipeline / infrastructure query facade
  -> MARKET_SNAPSHOT / SQLite cache / DB
  -> response with source/freshness/warnings
```

规则：

- `list_market` 只读 `market_instruments`，可左连接 `MARKET_SNAPSHOT`。
- `fetch_data_by_codes` 默认只读本地 snapshot/cache/DB。
- `refreshIfMissing = true` 才允许 lazy fetch；这不是 UI 默认路径。
- `scan` 先基于本地行情和 `daily_basic` 选出候选，再按 `include` 补数据。
- 技术指标不存储，基于 K 线读模型现算。

---

## 4. 对外接口

### 前端展示接口

UI 只暴露两类 command：

| Command | 用途 | 读取路径 |
|---|---|---|
| `list_market` | 股票 / 指数 / 场内基金全列表，可选携带实时行情摘要 | SQLite `market_instruments` + `MARKET_SNAPSHOT` |
| `fetch_data_by_codes` | 行情、K 线、分时、分钟 K、扫描、详情、基本面、公司事件 | 本地 snapshot / cache / DB |

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
      price?: number;
      change?: number;
      changePercent?: number;
      open?: number;
      high?: number;
      low?: number;
      previousClose?: number;
      volume?: number;
      amount?: number;
      source?: string;
      capturedAt?: number;
      stale?: boolean;
    };
    warnings?: string[];
  }>;
  page: {
    limit: number;
    offset: number;
    hasMore: boolean;
  };
};
```

约束：

- `includeQuote = true` 时只读 `MARKET_SNAPSHOT`，不触发外部 provider。
- `quote` 是列表页摘要字段，不包含五档盘口。
- `MARKET_SNAPSHOT` 缺某个标的时，该标的 `quote` 为空，并在 `warnings` 标记 `quote_missing`。

#### `fetch_data_by_codes`

```ts
type FetchDataByCodesRequest = {
  codes?: string[]; // 支持 600519 / 600519.SH / 510300.SH
  scan?: {
    filter?: "limit_up" | "limit_down" | "top_gain" | "top_loss" | "top_amount" | "top_volume";
    conditions?: ScanCondition[];
    sortBy?: "change_pct_desc" | "change_pct_asc" | "amount_desc" | "volume_desc" | "turnover_rate_desc";
    limit?: number;
  };
  include?: {
    quote?: boolean;
    intraday?: boolean;
    klines?: Array<"day" | "week" | "month">;
    minuteKlines?: Array<"1m" | "5m" | "15m" | "30m" | "60m">;
    indicators?: boolean;
    profile?: boolean;
    dailyBasic?: boolean;
    events?: boolean;
  };
  limit?: {
    kline?: number;
    minuteKline?: number;
    intradayDays?: number;
    eventsDaysAhead?: number;
  };
  refreshIfMissing?: boolean;
};

type FetchDataByCodesResponse = {
  scan?: ScanResult;
  items: Array<{
    code: string;
    tsCode: string;
    category: "stock" | "index" | "fund";
    name?: string;
    quote?: StockQuote;
    intraday?: MinutePoint[];
    klines?: Record<"day" | "week" | "month", KlinePoint[]>;
    minuteKlines?: Record<string, MinuteKlinePoint[]>;
    indicators?: IndicatorSnapshot;
    profile?: StockProfile;
    dailyBasic?: DailyBasic;
    events?: CompanyEvent[];
    freshness?: {
      quoteAgeMs?: number;
      klineFetchedAt?: Record<string, string>;
      dailyBasicTradeDate?: string;
    };
    sources?: Record<string, string>;
    warnings?: string[];
    errors?: string[];
  }>;
};
```

### Agent 调用方法

Agent 只暴露一个工具：

```ts
type FetchQuotesToolInput = FetchDataByCodesRequest;
```

工具名：`fetch_quotes`

约束：

- 复用 `fetch_data_by_codes` 的底层 query 能力。
- 默认输出应比 UI DTO 更精简，避免 token 爆炸。
- Agent 不直接调用 provider，也不需要知道 TDX / EM / TuShare 的细节。

### 内部 Rust API

内部 API 以 query facade 为主：

```rust
list_market(request) -> ListMarketResponse;
fetch_data_by_codes(request) -> FetchDataByCodesResponse;
refresh_market_instruments();
refresh_market_quotes(scope);
refresh_klines(scope);
refresh_daily_basic(scope);
refresh_company_events(scope);
```

---

## 5. 模块独有功能

### Provider 策略

全市场列表：

1. TDX 主源：启动 / 每日 08:30 拉基础 universe。
2. TuShare enrich：有 token 时补行业、上市状态、指数分类、基金类型、管理人、上市日期等。

实时行情：

```text
TDX > Eastmoney > 腾讯 > 新浪
```

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
- 前端展示优先 `qfq`，没有则用 `none`。
- Agent 做趋势 / 技术指标判断优先 `qfq`。
- 只能用 `none` 时必须返回 warning：

```text
不复权，除权除息附近的跳空可能扭曲趋势和技术指标
```

### 后台刷新

| 数据 | 策略 |
|---|---|
| 全市场列表 | 启动 + 每日 08:30：TDX 基础 universe；TuShare 可用时 enrich |
| 实时行情 | 自选股 / 持仓 / 核心指数盘中 15s；全市场 universe 盘中 60s |
| K 线 | 启动后预热关注标的；盘后 16:00 补日周月；TuShare 可用时补复权 |
| `daily_basic` | 每个交易日盘后刷新 |
| `company_events` | 每日低频刷新，覆盖未来 N 天事件窗口 |

---

## 6. 验收标准 / 例子

- SQLite 只有统一 `market_instruments` 作为股票 / 指数 / 基金 universe 主表；实现中不新增平行主表。
- `list_market` 和 `fetch_data_by_codes` 是 UI 读取 quotes 的唯一入口。
- `fetch_quotes` 是 Agent 读取 quotes 的唯一工具入口。
- UI / Agent 默认读取路径不直接请求 TDX / EM / TuShare / 腾讯 / 新浪。
- `list_market({ includeQuote: true })` 只读取 `market_instruments` + `MARKET_SNAPSHOT`，缺实时字段时返回 `null`，不触发远端补拉。
- `fetch_data_by_codes({ codes, include })` 默认只读本地 DB / snapshot；需要远端刷新必须走显式 refresh / 后台任务。
- `MARKET_SNAPSHOT` item 带 `category/source/freshness`，breadth 只统计 `category == stock`。
- K 线读取不通过 6 位代码重新猜市场；已知 `ts_code` 必须贯穿到 provider/cache。
- `daily_basic` 和 `company_events` 由本地 DB 读取，远端拉取只发生在后台刷新 / 显式 refresh 路径。
- 所有批量返回都是 per-item warning/error；单个标的缺数据不让整批失败。
- 指标计算优先使用 `qfq` K 线；只能使用 `none` 时返回复权 warning。
- 依赖方向自检为空：quotes 任一层不 import Agent / Account / News 代码。

---

## 7. 模块边界外

这些能力不属于 `fetch_data_by_codes`：

- 龙虎榜
- 个股资金流
- 北向资金
- 融资融券
- 概念 / 板块

如需这些能力，单独设计 `fetch_market_research`，避免 `fetch_data_by_codes` 变成不可维护的大杂烩。
