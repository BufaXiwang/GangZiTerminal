# Quotes 模块 Spec

> 本文档是 Quotes 模块的设计契约。`docs/architecture.md` 仍是架构权威基线；本文聚焦 quotes 的数据模型、统一读取 API、数据源优先级和后台刷新策略。
>
> 所有实现均以本文档的最新设计为准；不保留旧接口、旧表结构或兼容路径作为设计目标。

## 一句话定位

**市场数据本地读模型**：后台任务持续从 TDX / Eastmoney / 腾讯 / 新浪 / TuShare 补数据，UI 和 Agent 只读本地 `MARKET_SNAPSHOT` / SQLite cache / DB。

不把远端 provider 暴露给前端或 Agent。

---

## 1. 对外接口

### UI Commands

UI 只暴露两类 command：

| Command | 用途 | 读取路径 |
|---|---|---|
| `list_market` | 股票 / 指数 / 场内基金全列表，可选携带实时行情摘要 | SQLite `market_instruments` + `MARKET_SNAPSHOT` |
| `fetch_data_by_codes` | 行情、K 线、分时、分钟 K、扫描、详情、基本面、公司事件 | 本地 snapshot / cache / DB |

### Agent Tool

Agent 只暴露一个工具：

| Tool | 用途 |
|---|---|
| `fetch_quotes` | 复用 `fetch_data_by_codes` 的底层 query 能力，按参数选择 quote / klines / indicators / profile / events / scan |

Agent 不直接调用 provider，也不需要知道 TDX / EM / TuShare 的细节。

---

## 2. 统一标的模型

股票、指数、基金统一存储在一张主表：

```sql
create table market_instruments (
    ts_code text primary key,        -- 600519.SH / 000001.SH / 510300.SH
    code text not null,              -- 600519
    name text not null,
    category text not null,          -- stock / index / fund
    market text not null,            -- SH / SZ / BJ

    sector text,                     -- stock: 行业
    status text,                     -- listed / suspended / delisted / unknown

    -- category-specific metadata
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

Rust / TS 对外模型：

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

硬规则：

- 所有 quotes 读写主键优先使用 `ts_code`。
- `StockCode` 只代表 6 位输入 / 股票业务概念；不能用 `StockCode::to_ts_code()` 给指数、基金或已知 `ts_code` 的路径重新猜市场。
- 股票 / 指数 / 基金在 query 层统一；provider 层仍可按 category 分流。

---

## 3. 数据源策略

### 全市场列表

优先级：

1. **TDX 主源**：启动 / 每日 08:30 拉基础 universe。优点是不依赖 token，冷启动可用。
2. **TuShare enrich**：有 token 时补行业、上市状态、指数分类、基金类型、管理人、上市日期等。

写入策略：

- TDX 先 upsert minimal row，保证 `list_market` 可用。
- TuShare 后台 enrich 同一行。
- 若 TDX 和 TuShare 都命中，`source = mixed`。
- TuShare 不可用不阻塞列表可用性。

### 实时报价

优先级：

```text
TDX > Eastmoney > 腾讯 > 新浪
```

读取策略：

- UI / Agent 读 `MARKET_SNAPSHOT`。
- 后台任务负责刷新。
- `refreshIfMissing = true` 时可 lazy fetch；UI 默认不依赖 lazy fetch。

### 日 / 周 / 月 K

TDX 能获取日 / 周 / 月 K，但限制如下：

- 不支持 BJ。
- 不复权，`adjust = none`。
- 单次根数有限，适合快速图表展示。

策略：

- 快速展示：TDX 补 `none`。
- TuShare enrich：补长历史和 `qfq` / `hfq`。
- 股票趋势 / 技术指标优先使用 `qfq`；没有 `qfq` 时使用 `none` 并返回 warning。

### 分钟 K / 分时

优先级：

```text
TDX > Eastmoney
```

BJ 可以不支持，返回 per-item warning / error 即可。

### 基本面 / 公司事件

`daily_basic` 和 `company_events` 必须本地化，不能在 UI 热路径临时打 TuShare。

---

## 4. 本地读模型表

### 实时快照

`MARKET_SNAPSHOT` 建议从 `ts_code -> StockQuote` 升级为带元数据的 snapshot item：

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

### 日 / 周 / 月 K

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
```

### 分钟 K

```sql
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
```

### 分时

分时可以先用短 TTL cache；若落库，使用：

```sql
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

### 每日基本面

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
```

### 公司事件

```sql
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

## 5. `list_market`

Request：无参数或仅筛选参数。

```ts
type ListMarketRequest = {
  category?: "stock" | "index" | "fund";
  query?: string;
  includeQuote?: boolean;          // true 时从 MARKET_SNAPSHOT 左连接实时摘要
  limit?: number;
  offset?: number;
};
```

Response：

```ts
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

- 标的列表只读 SQLite `market_instruments`。
- `includeQuote = true` 时只读 `MARKET_SNAPSHOT`，不触发外部 provider。
- `quote` 是列表页摘要字段，不包含五档盘口；完整行情仍通过 `fetch_data_by_codes(include.quote)` 获取。
- `MARKET_SNAPSHOT` 缺某个标的时，该标的 `quote` 为空，并在 `warnings` 标记 `quote_missing`。

---

## 6. `fetch_data_by_codes`

统一读取行情、K 线、扫描和详情。`codes` 与 `scan` 至少提供一个。

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
```

Response：

```ts
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

默认行为：

- 只读本地 snapshot / cache / DB。
- 缺数据时在 item 上返回 warning / error，不让整个请求失败。
- `refreshIfMissing = true` 时可以触发 lazy fetch，但这不是 UI 默认路径。
- `scan` 会先基于本地数据选出候选，再按 `include` 补 item 数据。

---

## 7. 复权策略

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

---

## 8. 后台刷新策略

### 全市场列表

| 时机 | 动作 |
|---|---|
| 启动 | TDX 拉基础 universe，写 `market_instruments` |
| 每日 08:30 | TDX 刷基础 universe |
| TuShare token 可用时 | 后台 enrich 行业、状态、指数 / 基金元信息 |

### 实时行情

| 范围 | 频率 | 数据源 |
|---|---:|---|
| 自选股 / 持仓 / 核心指数 | 盘中 15s，盘外 60s | TDX > EM > 腾讯 > 新浪 |
| 全市场 universe | 盘中 60s，盘外 5min，周末 30min | TDX 主，EM fallback |

### K 线

| 时机 | 动作 |
|---|---|
| 启动后 | 预热自选股 / 持仓日周月 K |
| 盘后 16:00 | TDX 快速补日周月 `none` |
| TuShare 可用时 | 补长历史、`qfq` / `hfq` |

### 基本面 / 公司事件

| 数据 | 频率 |
|---|---|
| `daily_basic` | 每个交易日盘后刷新 |
| `company_events` | 每日低频刷新，覆盖未来 N 天事件窗口 |

---

## 9. 模块边界外

这些能力不属于 `fetch_data_by_codes`：

- 龙虎榜
- 个股资金流
- 北向资金
- 融资融券
- 概念 / 板块

如需这些能力，单独设计 `fetch_market_research`，避免 `fetch_data_by_codes` 变成不可维护的大杂烩。
