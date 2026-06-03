# Quotes 模块 Spec

> 本文档是 Quotes bounded context 的领域模型契约。`docs/design/architecture.md` 仍是架构权威基线。
>
> 本文档只描述最新设计目标，作为实现依据。

## 一句话定位

**市场数据本地读模型**：后台任务以 **TDX 为主源**持续补数据，**腾讯**作为 TDX 实时行情 fallback（兼 BJ 实时主路径），**Eastmoney** 作 BJ universe 枚举 + K 线 / 分时 / 日线兜底（不参与实时报价）；**TuShare 仅在 token 配置且健康检查通过时**作 enrich 补充（`daily_basic`、公司事件、交易日历校准）。对外读取只访问本地 `MARKET_SNAPSHOT`、cache 和读模型。

远端 provider 是 Quotes 内部实现细节，不暴露给对外读取 API。

### 数据主源原则

1. **TDX 是 Quotes 数据的主源**：universe / 实时行情 / 日 K / 周 K / 月 K / 分钟 K / 分时 / xdxr 除权数据 全部从 TDX 直接获取。
2. **本地基于 TDX xdxr 自算复权**：日 / 周 / 月 K 在本地存 unadjusted；`qfq` / `hfq` 由本地 xdxr 事件按需 in-memory 计算，不依赖 TuShare adj_factor。
3. **TuShare 是 enrich，不是主源**：仅当 token 配置且 `TushareHealthState.is_available = true` 时，才向 TuShare 拉取 universe enrich（行业 / 上市状态 / 基金分类等）、`daily_basic`、公司事件、交易日历校准。
4. **TuShare 不可用必须降级而非失败**：token 缺失或健康检查失败时，Quotes 仍能正常提供 TDX 路径的全部能力；只是对应 enrich 字段为空，相应 series 带 freshness warning。

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

> **当前 universe 只覆盖股票 / 指数 / 场内基金（三类），债券 / 逆回购等一律不纳入。** TDX `security_list`
> 原始全量含交易所所有证券（实测 ~5 万：股 ~5200、指 ~560、基 ~1700，**其余 ~4.3 万是国债/可转债/
> 企业债/逆回购等**）。`universe::classify` 白名单只放行三类（SH 股 600/601/603/605/688/689、基 51/56/58、
> 指 000/999；SZ 股 000-004/300/301、基 159、指 399；BJ 股），其余 `None` 丢弃。
> 注意分层：**底层 TDX provider 方法（`fetch_quote`/`fetch_kline_*` 等）是 category-无关的**——`TsCode` 只校验
> 「6 位数字 + 市场后缀」，传入债券代码（如 `110059.SH`）TDX 同样会返回数据。所以「不取债券」是 universe
> 策展层（`classify` + 读路径只遍历 curated universe）的约束，**不是 provider 方法的硬限制**。
> **按需取债券已可正确处理（universe 仍不收）**：底层 provider 方法 category-无关，按代码取债券时
> `map_security_quote` 的价格缩放是 **decimal-driven** 的——`is_bond(market, code)` 识别债券（**4 位小数**，
> 实测可转债 TDX integer = 真值×10000）并 `×0.01` 校正（ETF 是 3 位 `×0.1`；否则同样 10× 错）。所以
> 「取债券行情」开箱即用且报价正确（实证与腾讯一致），但债券**不进
> universe**（list_market / 扫描 / 详情遍历 curated universe，看不到债券）。债券**无复权**：xdxr 为空
> → adjust 自然退化为 none。
> **债券作为一等公民**（纳入 universe 展示 / 可转债交易 / 专属类目）**仍延后**——见
> [issues/quotes-bond-support-todo.md](../../issues/quotes-bond-support-todo.md)（届时需 `InstrumentCategory::Bond`
> + 放开 `classify` + universe 纳入 + Account 交易规则配合）。

```ts
type MarketInstrument = {
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
- `board` 和 `isSt` 是 Quotes 维护的当前派生属性，用于涨跌停规则；来源可以是 universe enrich、名称变更 / ST 事件或 provider 字段，但对外只暴露当前事实。

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

`MARKET_SNAPSHOT` 以 `tsCode` 为单槽 cache。交易日切换时不要求立即清空旧槽位；读取路径必须按 eligible trade date 校验，`tradeDate` 不匹配时视为无可用 quote。

用途：

- breadth 只统计 `category == stock`。
- 调用方通过 `StockQuote` / `quoteFreshness` 能看到 source / freshness。
- fallback 源字段缺失时可解释。

freshness 定义：

- `capturedAt` 表示本地成功获取并写入该 quote snapshot 的时间，不表示交易所成交时间。
- provider / refresh 成功写入 snapshot 时更新 `capturedAt`、`exchangeTime`、`source`；读取路径不能刷新 `capturedAt`，也不能因为被读取而延长有效期。
- `exchangeTime` 只用于审计和展示市场时间，不能替代 `capturedAt` 或 `tradeDate` 做有效性判断。
- `tradeDate` 表示该 quote 对应的交易日。Quotes 通过 `resolve_market_time(now)` 返回的 `MarketTimeContext` 和 `tradeDate` 推导它是盘中事实还是收盘事实，不额外暴露 snapshot kind 枚举。
- 每个读取请求只调用一次 `resolve_market_time(now)`，并由该结果计算唯一 eligible trade date：交易时段内为 `currentTradeDate`，非交易时段为 `latestCompletedTradeDate`。
- snapshot `tradeDate` 不等于 eligible trade date 时不得返回 quote；交易时段内返回 `quote_missing`，非交易时段返回 `snapshot_expired` 或 `quote_missing`。
- `MarketTimeContext.isTradingTime = true` 时，读取必须使用 `tradeDate = currentTradeDate` 的 quote；`now - capturedAt` 超过当前读取意图的 stale threshold 时 `freshness.status = "stale"`。
- stale threshold 按读取意图选择：`detail` 默认 30s，用于 `fetch_data(include.quote = true)` 等精确标的详情读取；`universe` 默认 90s，用于 `list_market(includeQuote = true)` 和 `scan_market` 这类全市场 / 大范围读取。
- `quote_stale` warning 只在超过适用 threshold 时返回；全市场 60s refresh 下，正常完成的 universe snapshot 不应天然产生 stale warning。
- `MarketTimeContext.isTradingTime = true` 时，`now - capturedAt > quote_snapshot_expire_secs` 的当日 quote 硬过期；默认阈值为 1 小时。硬过期时 `quote` 不返回可用行情字段，`quoteFreshness.status = "missing"`，`quoteFreshness.warning = "snapshot_expired"`。
- `MarketTimeContext.isTradingTime = false` 时，读取优先使用 `tradeDate = latestCompletedTradeDate` 的 quote；只要 trade date 匹配最新已完成交易日，就视为收盘事实，不因 `capturedAt > 1h` 过期。
- 非交易时段如果缺少最新已完成交易日的 close snapshot，则 quote 为空并返回 `snapshot_expired` 或 `quote_missing`；不能退回更早交易日的旧 quote。
- 启动冷加载或 cache hydrate 未完成时，读取接口按无可用 snapshot 处理：quote 为空，并在 response 或 item warning 返回 `snapshot_expired` / `quote_missing`。
- stale threshold 和硬过期阈值不因 TDX / 腾讯 source 改变；如配置 source-specific threshold，响应必须保留 source 以便审计。

行情 DTO：

```ts
type QuoteDepthLevel = {
  price?: Price;
  volume?: Volume;
};

// 实时报价当前只会产出 "tdx" / "tencent"（/ 未来 "mixed"）。"eastmoney" / "sina" 变体
// 保留仅为向后兼容旧 quote_close_snapshot 行的反序列化；新数据不再产出这两个 source。
type QuoteSource = "tdx" | "eastmoney" | "tencent" | "sina" | "mixed";

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
  source: QuoteSource;
  capturedAt: OccurredAt;
  exchangeTime?: OccurredAt;
  freshness: Freshness;
  warnings?: WarningCode[];
};
```

规则：

- `price` 是当前最新价；不能用 `previousClose` 伪造当前价。
- `tradeStatus` 是 query facade 按当前 `MarketTimeContext`、instrument lifecycle 和 quote eligibility 派生的读取时状态，不是 provider 原始状态。
- instrument `status = suspended` 时 `tradeStatus` 必须为 `halted`；非交易时段或不可交易标的可为 `closed`；无法判断时为 `unknown`。
- instrument `status = delisted` 由 `InstrumentStatus` 表达；`tradeStatus = closed` 只表示当前读取时不可即时交易，不承载退市原因。
- `tradeStatus = halted` / `closed` / `unknown` 时 Account 写路径必须 fail closed，不能即时成交。
- Quotes 提供 L1 五档盘口能力：`bid` 表示买一到买五，`ask` 表示卖一到卖五，数组按离成交价最近到最远排序，最多 5 档。
- `QuoteDepthLevel.volume` 使用 shared `Volume` 规范化单位；价格或数量缺失的档位不得伪造为 0。
- 缺盘口、盘口为空、买一 / 卖一价格缺失时必须返回 `depth_missing` warning。
- 少于 5 档但买一 / 卖一可用时仍返回已有档位，并返回 `data_partial` warning；Quotes 不在本模块判断某笔订单需要消耗几档盘口。
- TDX 是五档盘口主路径；腾讯可补充可用盘口（实测含五档）。EM 不参与实时报价路径，不提供报价盘口。
- `limitUp` / `limitDown` 优先由 Quotes 基于 `previousClose`、`MarketInstrument.board`、`MarketInstrument.isSt`、上市日期 / 公司事件和 A 股涨跌幅规则计算；provider 返回值只能作为校验或补充。
- 计算涨跌停价必须使用纯规则：确定适用涨跌幅、处理新股 / 无涨跌幅限制场景、按最小价格 tick 舍入；缺少必要输入时可为空，并必须返回 `quote_price_missing` warning。
- Account 不得自行推导涨跌停价；`limitUp` / `limitDown` 缺失时不得执行涨跌停相关判断。
- `changePercent` 使用百分点，例如 `3.25` 表示上涨 3.25%。

### K 线和分时读模型

K 线和分时是 Quotes 的本地读模型，不是 provider 原始数据直出。

身份规则：

- 日 / 周 / 月 K：`(tsCode, period, adjust, date)` 是对外身份；本地存储只持久化 `adjust = "none"` 的 unadjusted 行，`qfq` / `hfq` 行由 unadjusted + 本地 xdxr 事件按需现算，不预存。
- 分钟 K：`(tsCode, period, timestampMs)` 唯一。
- 分时点：`(tsCode, tradeDate, time)` 唯一。
- 本地读模型必须记录 source / fetchedAt；对外 K 线、分钟 K 和分时都通过 series-level `freshness` 暴露统一 freshness。

> **分时（intraday）当前已下线（descoped）**。原因：实测当前 TDX 服务器池返回的 `minute_time`（`get_minute_time_data` / cmd 0x051d）响应为**非标准格式** —— body 在 `num` 之后回显了请求的 6 位 code，且 per-point 字节结构与 pytdx/mootdx 假设的 `(price, reversed1, vol)×N` 不一致（前 2 个点能解出正确价，第 3 点起 varint 失步）。pytdx/mootdx 在该服务器上同样无法可靠解码。分时是 nice-to-have，不属于研究 / 模拟核心（K 线 / 报价 / 资讯不受影响），故前端移除「分时」tab、不再展示。后端 `refresh_intraday` / `IntradaySeries` 读模型代码保留为 dormant，待将来找到返回标准格式的服务器或完成专项逆向再启用。`ChartPeriod` 仍保留 `"intraday"` 变体但 UI 不可达。

### 本地复权计算（基于 TDX xdxr）

复权数据真源是 TDX 协议层提供的 xdxr 除权事件（送股、转增、配股、分红）。Quotes 本地存以下两部分：

1. **unadjusted K 线**：`quote_klines_daily` 等表只存 `adjust = "none"` 的原始行（OHLCV 来自 TDX）。
2. **xdxr 事件**：本地表 `quote_xdxr_events` 存每个 `tsCode` 的除权事件列表（来自 TDX 协议层）。

`adjust = "qfq"` / `"hfq"` 不预存。读取时 Quotes 内部按 `(tsCode, period, adjust)` 现算：

- 取 unadjusted K 线点位序列。
- 取该 `tsCode` 的 xdxr 事件序列。
- 按经典前复权 / 后复权公式逐点平移收盘价、开盘价、最高价、最低价；成交量 / 成交额不复权。
- 结果以 series 为单位 cache（key = `(tsCode, period, adjust, xdxr_version)`），xdxr 事件刷新时整体失效。

规则：

- 本地不依赖 TuShare `adj_factor`；TuShare 即使可用，也不替代本地基于 xdxr 的复权计算。
- xdxr 事件缺失 / 未刷新时，`qfq` / `hfq` 读取必须返回当前能算出来的结果，并在 `KlineSeries.warnings` 标 `using_unadjusted_kline` 或 `qfq_missing`（取决于是否完全无 xdxr 数据）。
- xdxr 缺失三态语义（实现锚：`pipeline/quotes/service.rs::read_kline_series_with_adjust`）：
  - **状态 A · 全局未刷新**：本地从未跑过 `refresh_xdxr_events`，即 `quote_refresh_state` 没有 `kind = "xdxr"` 记录。任意 `tsCode` 读 `qfq` / `hfq` 都等同 unadjusted，必须返回 `qfq_missing` warning（语义："xdxr 尚未刷新，结果可能扭曲"）。
  - **状态 B · 该标的天然无除权事件**：xdxr 已刷新过（有 `kind = "xdxr"` 完成记录）但目标 `tsCode` 的 `quote_xdxr_events` 行数为 0。可能是指数 / 基金（无除权概念）或股票从未除权；此时 `qfq` / `hfq` 自然等同 unadjusted，**不**返回 warning（这是合理终态，不是数据缺失）。
  - **状态 C · 该标的部分历史缺失**：xdxr 已刷新但 events 数量不能完整覆盖 unadjusted K 线时间段（例如 events 起点晚于 K 线起点）。按现有 events 计算复权 factor，series 整体仍可用，返回 `using_unadjusted_kline` warning（语义："复权数据可能不完整"）。
  - 区分方法：`refresh_xdxr_events` 完成后写 `quote_refresh_state` 一行 `kind = "xdxr"`；读 qfq 时联合查 "xdxr refresh state 是否存在 + 目标 ts_code events 行数" 判 A/B/C。
- K 线为 **TDX-only**：`refresh_klines_full` 用 TDX 分页（`start=0,800,1600,…`）覆盖可获取的全部历史，落本地 `adjust = "none"`，复权统一走本地 xdxr 算法。TuShare **不再**补 K 线段（2026-06-02 决策）。

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
};

type MinutePoint = {
  tradeDate: TradeDate;
  time: string; // HH:mm
  price: Price;
  average?: Price;
  volume?: Volume;
  amount?: Amount;
};

type MinuteKlineSeries = {
  period: "1m" | "5m" | "15m" | "30m" | "60m";
  points: MinuteKlinePoint[];
  freshness: Freshness;
  warnings?: WarningCode[];
};

type IntradaySeries = {
  tradeDate: TradeDate;
  points: MinutePoint[];
  freshness: Freshness;
  warnings?: WarningCode[];
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
  board?: string;
  sector?: string;
  status?: InstrumentStatus;
  isSt?: boolean;
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
    validQuoteCount?: number;
    excludedMissingQuoteCount?: number;
    excludedExpiredQuoteCount?: number;
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
    warnings?: WarningCode[];
  }>;
  warnings?: WarningCode[];
};
```

规则：

- 扫描只能基于本地 snapshot / `daily_basic` / K 线派生数据。
- 扫描开始时固定一次 `MarketTimeContext` 和 snapshot view；同一次扫描不能混用 refresh 前后的 quote。
- 扫描使用 quote 字段时必须先应用 quote 有效性规则：`isTradingTime = true` 时使用 `tradeDate = currentTradeDate` 且未硬过期的 quote；`isTradingTime = false` 时使用 `tradeDate = latestCompletedTradeDate` 的 quote。无有效 snapshot 的 item 不能参与排名、条件判断或 breadth 统计。
- `validQuoteCount` 表示参与扫描的有效 quote 数；`excludedMissingQuoteCount` / `excludedExpiredQuoteCount` 表示因缺失或过期被排除的数量。覆盖不完整时响应级 `warnings` 必须包含 `data_partial`。
- 需要 `DailyBasic` 的条件使用 `tradeDate <= eligible quote tradeDate` 的最新一条 `DailyBasic`；缺失时该 item 不匹配该条件，并返回 `daily_basic_missing` 或 `data_partial`。
- `ScanCondition.field` 不允许自由字符串；新增字段必须先扩展 spec。
- `conditions` 内部按 AND 组合；任一条件不满足则该 item 不进入结果。
- 条件字段缺失时该 item 不匹配该条件，并在响应级 `warnings` 返回 `data_partial`；不能把缺失当作 0。
- `filter` 是预设筛选模板；与 `conditions` 同时出现时先应用 `filter`，再按 AND 应用 `conditions`。
- `sortBy` 显式传入时覆盖 `filter` 的默认排序；未传入时使用 `filter` 对应默认排序，再按 `tsCode` 稳定 tie-breaker。
- 连续竞价时段内未硬过期但 freshness 为 `stale` 的 quote 可以参与扫描，但该 item 必须带 `quote_stale` warning。

预设 filter：

| filter | 条件 | 默认排序 |
|---|---|---|
| `limit_up` | `price == limitUp`，且 `limitUp` 存在 | `amount desc, tsCode asc` |
| `limit_down` | `price == limitDown`，且 `limitDown` 存在 | `amount desc, tsCode asc` |
| `top_gain` | `changePercent` 存在 | `changePercent desc, tsCode asc` |
| `top_loss` | `changePercent` 存在 | `changePercent asc, tsCode asc` |
| `top_amount` | `amount` 存在 | `amount desc, tsCode asc` |
| `top_volume` | `volume` 存在 | `volume desc, tsCode asc` |

缺少 filter 必需字段的 item 不进入结果，并计入 coverage / warning；不能把缺失字段当 0。

### TuShare 健康状态

TuShare 是 Quotes 的可选 enrich 源。Quotes 内部维护一份全局 `TushareHealthState`，所有 TuShare provider 调用前必须先检查这个 state：

```ts
type TushareHealthState = {
  isAvailable: boolean;
  lastPingAt?: OccurredAt;
  lastSuccessAt?: OccurredAt;
  lastError?: string;
  nextRecheckAt?: OccurredAt;
};
```

健康检查协议：

- **启动 ping**：进程启动时，如果配置了 TuShare token，调用一次轻量 API（默认 `trade_cal` 单交易日查询）作为健康探针。成功 → `isAvailable = true`；超时 / HTTP 错误 / 鉴权失败 / rate limited → `isAvailable = false`，`lastError` 记录原因。启动 ping 单次超时默认 5 s，避免阻塞 setup。
- **定期重试**：`isAvailable = false` 时，按可配置间隔（默认 `recheck_interval = 1 小时`）重新 ping；探针成功 → `isAvailable = true`，连续失败计数器立即归零，恢复业务调用。
- **熔断**：维护进程内连续失败计数器，规则：
  - 每次 TuShare API 业务调用（含 universe enrich、`daily_basic`、公司事件、`trade_cal` 校准）失败时累加 1；任何一次业务调用成功立即重置为 0。
  - 计数器达到 `max_consecutive_failures`（默认 3）时主动把 `isAvailable` 翻回 `false`，进入定期重试循环。
  - 计数器**不持久化**：进程重启 / 跨日都从 0 重新计数，避免因历史失败导致的假性熔断。
  - 熔断态下不发起任何业务 API；仅 `recheck_if_due()` 用 `trade_cal` 单日 ping 探针。探针绕过计数器，不计为业务失败。
- **token 缺失**：直接视为 `isAvailable = false`，不发起任何网络请求，`lastError = "token_missing"`。

规则：

- 所有 TuShare 路径（universe enrich、`daily_basic`、公司事件、交易日历校准）调用前必须 check `isAvailable`；为 `false` 时跳过 TuShare 调用，走本地 / TDX 路径并在对应 series / item 返回适用 warning。
- 健康状态变更（`true ↔ false`）应该向外 emit 事件，便于运维 / UI 提示（事件名 / payload 由 [agent-runtime-module.md](agent-runtime-module.md) 协调）；本 spec 不强制 event 名称。
- 健康检查失败不得影响 TDX / 腾讯 / Eastmoney（universe + K线/分时）任何路径的可用性。

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

Quotes 对外暴露以下读取 command：

| Command | 用途 | 读取路径 |
|---|---|---|
| `list_market` | 股票 / 指数 / 场内基金全列表，可选携带实时行情摘要 | `MarketInstrument` 本地读模型 + `MARKET_SNAPSHOT` |
| `fetch_data` | 按 `tsCodes` 读取行情、K 线、分时、分钟 K、详情、基本面、公司事件 | 本地 snapshot / cache / DB |
| `scan_market` | 从本地 universe 扫描候选标的，返回轻量排名结果 | 本地 snapshot / `daily_basic` / K 线派生数据 |
| `market_breadth` | 全市场涨跌家数 + 涨停 / 跌停统计 | 本地 snapshot / `quote_close_snapshot` |
| `industry_heatmap` | 按行业聚合的涨幅 top N 卡片 | 本地 snapshot / `quote_close_snapshot` |
| `ensure_chart_data` | 前端切换标的 / 周期时，DB 空就触发后端拉一份 | 触发 `refresh_klines` / `refresh_minute_klines` / `refresh_intraday` |
| `extend_chart_history` | 前端 K 线图左拉到尽头时扩展更深历史 | 触发 `refresh_klines_extended(target_days)` |

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
  tsCode?: TsCode;
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
    eventsDaysAhead?: number;
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
    intraday?: IntradaySeries;
    klines?: Partial<Record<"day" | "week" | "month", KlineSeries>>;
    minuteKlines?: Partial<Record<"1m" | "5m" | "15m" | "30m" | "60m", MinuteKlineSeries>>;
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
- `tsCodes` 是精确身份查询，最多 200 个；超过上限返回 `invalid_input`。
- `tsCodes` 按 shared `TsCode` 格式校验；格式非法返回 `invalid_input`，合法但本地 universe 未知返回 `not_found`。
- 重复 `tsCode` 按首次出现去重；响应 item 顺序按去重后的请求顺序返回。
- 名称模糊搜索必须走 `list_market({ query })`；`fetch_data` 不做名称匹配，避免详情读取出现多义结果。
- `include` 缺失时默认等同于 `{ profile: true, quote: true }`。
- `include.klines` / `include.minuteKlines` 只返回调用方请求的周期；未请求的周期 key 不出现在响应中，不能用空数组伪装为“已请求但无数据”。
- `include.intraday = true` 只返回一个交易日的 `IntradaySeries`：交易时段内为 `currentTradeDate`，非交易时段为 `latestCompletedTradeDate`。多日分时不属于当前 `fetch_data` 契约；如需扩展必须先调整响应 DTO。
- `include.klines` 默认优先返回 `adjust = "qfq"`；缺少 qfq 时可降级为 `adjust = "none"`，必须在对应 `KlineSeries.warnings` 和 item `warnings` 返回 `using_unadjusted_kline`。
- `include.quote = true` 时，如果 snapshot 缺失，`quote` 为空并返回 `quote_missing` warning。
- `include.quote = true` 时，必须返回 `quoteFreshness`；有可用 `quote` 时它与 `quote.freshness` 语义一致，`quote` 为空时它承载缺失 / 过期原因。
- `include.quote = true` 返回完整 `StockQuote`，包括可用的 `bid` / `ask`、`limitUp` / `limitDown`、`tradeStatus` 和 warnings；Account 估值、成交模拟和保护条件评估必须走该 facade 或同等内部 query，不得使用 `list_market.quote` 摘要字段。
- 如果连续竞价时段内当日 quote 已超过 1 小时硬过期，`quote` 为空，`quoteFreshness.status = "missing"`，`quoteFreshness.warning = "snapshot_expired"`；不得把硬过期行情降级塞入 `quote`。
- `include.quote = true` 时，如果当前为非交易时段，可返回 `tradeDate = latestCompletedTradeDate` 的 quote；缺少该交易日 quote 时返回空 quote 和 `snapshot_expired` / `quote_missing` warning。
- `include.indicators = true` 返回完整默认指标集合；`include.indicators = IndicatorName[]` 只返回请求的指标子集，未知指标名必须返回 `invalid_input`。
- `fetch_data` 不触发远端 provider；缺失、过期或字段不足只通过 item warning / error 表达。刷新必须走显式 refresh use case 或后台任务。

Warning / Error code 规则：

| 条件 | Code |
|---|---|
| `TsCode` 格式非法、数量超限或 unknown indicator | `invalid_input` |
| 合法 `TsCode` 不在本地 universe | `not_found` / `instrument_missing` |
| 请求 quote 但 snapshot 不存在 | `quote_missing` |
| snapshot 存在但不符合当前 eligible trade date / 硬过期规则 | `snapshot_expired` |
| 当前价、昨收、涨跌停价等关键价格缺失 | `quote_price_missing` |
| 五档盘口缺失、为空或买一 / 卖一价格缺失 | `depth_missing` |
| 默认 qfq K 线缺失但可降级返回 `adjust = "none"` | `using_unadjusted_kline` |
| 调用方或后续扩展明确要求 qfq 且不得降级时缺少 qfq | `qfq_missing` |
| refresh 部分 provider / batch 失败但仍有可用结果 | `provider_partial_failure` |

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
- `scan_market` 的返回项可以包含用于筛选和展示的 `quote` / `dailyBasic` / `rank`。
- 调用方需要深入分析候选标的时，必须再用 `fetch_data({ tsCodes })` 读取详情。
- `scan_market` 使用 quote 字段时必须先应用 quote 有效性规则；`isTradingTime = true` 时使用 `tradeDate = currentTradeDate` 且未硬过期的 quote，`isTradingTime = false` 时使用 `tradeDate = latestCompletedTradeDate` 的 quote。
- `scan_market` 不触发远端 provider；缺失、过期或字段不足只通过 item warning / response warning 表达。

#### `market_breadth`

```ts
type MarketBreadth = {
  total: number;        // 有有效 quote 的标的总数；up + down + flat == total
  up: number;           // changePercent > 0
  down: number;         // changePercent < 0
  flat: number;         // changePercent == 0（或缺失但 quote 仍有效）
  limitUp: number;      // 涨停家数（详见下方阈值规则）；是 up 的子集
  limitDown: number;    // 跌停家数；是 down 的子集
  noData: number;       // universe 中没有有效 quote 的标的数
  tradeDate: TradeDate;
  computedAt: OccurredAt;
};
```

规则：

- 仅统计 `category == stock`；指数 / 基金不计入 `total` 也不计入 `noData`。
- 必须先应用 quote 有效性规则：`isTradingTime = true` 时使用 `tradeDate = currentTradeDate` 且未硬过期的 quote；`isTradingTime = false` 时使用 `tradeDate = latestCompletedTradeDate` 的 quote。无有效 quote 的 stock 计入 `noData`，不进入 `total`。
- `market_breadth` 不触发远端 provider；只读 `MARKET_SNAPSHOT` 和 `quote_close_snapshot`。
- 涨停 / 跌停阈值复用 `compute_limit_band` 规则（与 `StockQuote.limitUp` / `limitDown` 完全一致的派生口径）；
  - 主板：±10%（ST：±5%）
  - 创业板（300/301）/ 科创板（688/689）：±20%
  - 北交所：±30%
  - 指数 / 部分基金：`bounded = false`，永远不计为涨停 / 跌停
- 判定使用 `changePercent.abs() >= up_percent - 0.05`（百分点 epsilon），允许撮合 tick 抖动；这与对外 `limit_up` filter 用 `price == limitUp` 的精确口径不冲突，但允许该 API 在涨停板附近的边界 case 把"实质涨停"统计进来。

#### `industry_heatmap`

```ts
type IndustryHeatmapItem = {
  sector: string;             // MarketInstrument.sector
  avgChangePercent: number;   // 该行业内有效 quote 标的的 changePercent 算术平均（百分点）
  count: number;              // 该行业参与统计的有效 quote 标的数
  leaderCodes: TsCode[];      // 涨幅 top 3（按 changePercent desc + tsCode asc）
  leaderNames: string[];      // 对应名称
};

type IndustryHeatmap = {
  topGainers: IndustryHeatmapItem[];  // 按 avgChangePercent desc 取前 N
  topLosers: IndustryHeatmapItem[];   // 按 avgChangePercent asc 取前 N
  tradeDate: TradeDate;
  computedAt: OccurredAt;
};
```

规则：

- 仅统计 `category == stock`，行业归属来自 `MarketInstrument.sector`。
- `sector` 为 `null` 或空串 → 归入虚拟桶 `"未分类"`，**不**参与 `topGainers` / `topLosers`（避免空数据噪音淹没卡片）。
- 无有效 quote 的标的不参与统计；行业内若全部无 quote 则该行业不出现。
- `topN` 默认 5；caller 可传 1–50。当行业总数 < `topN` 时返回全部。
- `leaderCodes` / `leaderNames` 按 `changePercent desc, tsCode asc` 取前 3；不足 3 时返回实际数量。
- `industry_heatmap` 不触发远端 provider；只读 `MARKET_SNAPSHOT` 和 `quote_close_snapshot`。
- 调用方需要看具体标的时再用 `fetch_data({ tsCodes })` 拉详情。

#### `ensure_chart_data`

**用途**：UI 切换到某标的 + 某 chart period 时，如果 DB 没数据就触发后端拉一份，**不阻塞 UI 主流程但同步等待结果**。

```ts
ensure_chart_data(ts_code: TsCode, period: ChartPeriod): Promise<void>
// ChartPeriod = "intraday" | "1m" | "5m" | "15m" | "30m" | "60m" | "day" | "week" | "month"
```

行为：
- `period ∈ {day, week, month}` → **只同步拉首屏一页**（`fetch_kline_page(scope=Subscribed[ts_code], period, start=0)`，TDX `security_bars(start=0)` 的最新 ~800 根），落 DB 后立即返回，让 UI 第一时间出图。**更早的历史不由本命令拉**：前端 `KlineCanvas` 在首屏渲染后用 background loop 调 `fetch_kline_page(start=800, 1600, ...)` 渐进 prepend 补到 DB（upsert 幂等）。
  - 设计目的：首屏感知速度优先。老股全量历史需 10+ 次 TDX 调用（每次 80ms 间隔 + 100-500ms 协议延迟，总 3-10s），若同步全量拉会让用户进详情页干等数秒；改为"首屏即出 + 后台补全"。
  - 代价 / 边界：若用户在后台分页跑完前离开详情页，DB 不保证有该 ts_code 的全量历史。需要全量历史的场景（盘后扫描 / agent 长周期分析）由调度路径 `refresh_klines` / `refresh_klines_extended(history_days > 800)` 保证，不依赖 `ensure_chart_data` 这一次前台触发。
  - `refresh_klines_full`（一次性分页全量）仍保留实现（`service.rs::refresh_klines_full`），供需要"同步落全量"的调用方（如显式补段）使用，但**不是** `ensure_chart_data` 的默认路径。
- `period ∈ {1m, 5m, 15m, 30m, 60m}` → `refresh_minute_klines(...)`（同步全量当日 catch-up）。
- `period == intraday` → `refresh_intraday(...)`（分时已 descope，dormant）。
- 已有数据时也会重新拉首屏（upsert 幂等，PK = `(ts_code, period, adjust, trade_date)`）；UI 调用方决定何时触发。
- 首屏页失败由底层 abort 并返回 `Err`（partial 落库无意义）；前端后台分页的单页失败不影响首屏。

#### `extend_chart_history`

**用途**：用户在 K 线图左拉到尽头时，扩展该 ts_code 的历史深度。

```ts
extend_chart_history(ts_code: TsCode, period: "day" | "week" | "month", target_days: u32): Promise<void>
```

行为：
- `target_days ≤ 800` → 走 TDX 主路径（单次 fetch 上限 ~800 根，约 3 年日 K）。
- `target_days > 800` → 走 `refresh_klines_full`（TDX 分页 `start=0,800,…`）补全更深历史，落库 `adjust = none`。（K 线 TDX-only，不调 TuShare。）
- 否则（token 缺失或 health 不可用）silent skip 长历史段，只返回 TDX 已拉的部分。
- 分钟 K / 分时**不**走此命令（分钟 K 一天 240 根，单次 fetch 已足够；不需要"更深历史"语义）。

调用频率：UI 每次左拉到边界增加 ~300 天再调一次。建议 UI 内部对 `target_days` 设置上限（如 2000 天）。

### 内部 Rust API

内部 API 以 query facade 为主：

```ts
type RefreshMarketQuotesScope =
  | { kind: "subscribed"; tsCodes: TsCode[] }
  | { kind: "universe" }
  | { kind: "manual"; tsCodes: TsCode[] };

type RefreshMarketQuotesRequest = {
  scope: RefreshMarketQuotesScope;
  purpose: "intraday" | "close";
  tradeDate?: TradeDate;
};
```

```rust
list_market(request) -> ListMarketResponse;
fetch_data(request) -> FetchDataResponse;
scan_market(request) -> ScanMarketResponse;
refresh_market_instruments();
refresh_market_quotes(request: RefreshMarketQuotesRequest);
refresh_klines(scope);
refresh_daily_basic(scope);
refresh_company_events(scope);
core_indexes() -> Vec<TsCode>;
```

规则：

- `scope.kind = "subscribed"` 时，调用方必须传入已合并的关注标的集合；Quotes 不读取 Account，也不内嵌核心指数列表。
- `scope.kind = "manual"` 时，`tsCodes` 必须非空并通过 `TsCode` 校验；缺失或为空返回 `invalid_input`。
- `scope.kind = "universe"` 表示刷新 Quotes 当前全市场 universe，不接受调用方自带 `tsCodes`。
- `purpose = "close"` 表示为最新已完成交易日补收盘快照；`tradeDate` 缺省时由 Quotes `MarketTimeContext.latestCompletedTradeDate` 派生。
- `purpose = "intraday"` 表示盘中 / 手动常规刷新；非交易时段可刷新最新已完成交易日的可读快照，但不表示可交易。

---

## 5. 模块独有功能

### Provider 策略

Provider reference：

- [TDX](references/quotes/tdx.md)
- [Eastmoney](references/quotes/eastmoney.md) — 仅 BJ universe + K 线 / 分时 / 日线兜底，不参与实时报价
- [TuShare](references/quotes/tushare.md)
- [Tencent](references/quotes/tencent.md) — 实时行情唯一 HTTP fallback + BJ 实时主路径

规则：

- 本 spec 定义 provider 选择策略和 canonical contract。
- 具体连接方式、字段映射、单位转换、timeout、retry 写在 provider reference。
- 所有 provider 输出必须 normalize 到 `MarketInstrument`、`StockQuote`、K 线 / 分钟 K / 分时读模型行、`DailyBasic` 或 `CompanyEvent`，对外由对应 series DTO 暴露。
- Provider 失败默认是 item / batch 级 partial failure，不改变对外读取契约。
- 当前 provider 集合：TDX / 腾讯 / Eastmoney / TuShare（**Sina 已于 2026-06-01 移除**）。后续可以继续扩展 provider，但新增 provider 必须先补 reference 文档，并 normalize 到本 spec 的 canonical model。

全市场列表：

0. **Cold-start seed**：进程启动时，先把内置 `BUILTIN_INSTRUMENTS` 同步 upsert 进 `quote_instruments`，保证 UI 第一帧非空。
   - 内容约 80 条核心标的：核心指数 + 沪深主板代表性蓝筹 + 主流 ETF；与代码一起打包进 binary，无外部 I/O。
   - 字段构成：`(market, code6, name)` 三元组 → 经 `universe::classify` 推断 `category` / `board`，其他 enrich 字段（`sector` / `isSt` / `listDate` / `publisher` / `indexCategory` / `fundType` / `management`）一律留空，等真实 provider 覆盖。
   - seed 行 `source = "tdx"`、`status = "listed"`、`updatedAt = now`；写入路径为同步阻塞 upsert，必须在 step 1 异步 refresh 启动前完成，覆盖窗口 ~5–10 s（从启动到 TDX universe 真实拉取完成）。
   - seed size 是 runtime 断言（下限 60）；少于该阈值视为代码 bug，不是数据缺失。
   - step 1–3 真实 provider 数据通过 ts_code 主键 upsert 覆盖 seed 行（含 `source` 字段）；同 `tsCode` 冲突时真实 provider 数据为准。
1. TDX 主源：启动 / 每日 08:30 拉基础 SH / SZ universe，覆盖 seed 行。
2. Eastmoney 补 BJ / TDX 缺失标的。
3. TuShare enrich：仅当 `TushareHealthState.isAvailable = true` 时补行业、上市状态、指数分类、基金类型、管理人、上市日期等；不可用时这些字段保持上次成功 enrich 结果或空，并在 instrument 级 `warnings` / freshness 反映 enrich 不完整。

实时行情：

```text
TDX > 腾讯
```

- TDX 是 SH / SZ 实时报价主路径。
- **腾讯是唯一的 HTTP 实时行情 fallback**（TDX 失败 / 缺字段时），也是 **BJ 实时报价主路径**（BJ 不走 TDX，直接腾讯）。腾讯实测覆盖股票 / 指数 / 场内基金 / BJ，含五档盘口 + 换手率 + 成交额，价格为真值（无缩放问题）。
- **Eastmoney 不参与实时报价**：EM 的 push2 `stock/get` 价格按 `10^f59` 缩放（实现曾硬编码 `/100`，对 3 位小数 ETF 产生 10× 错价），且其五档字段映射 / BJ secid 前缀无法在受限环境实测确认。为「避免数据源错误」，EM 退出实时报价路径，仅保留它不可替代 / 低风险的角色：**BJ universe 枚举** + **K 线 / 分时 / 日线兜底**（这些走 CSV 真值解析，不受 f59 缩放影响）。
- **Sina 已移除**（2026-06-01）：字段严格弱于腾讯（无盘口、无换手、exchange_time 解析依赖末位字段而实际 date/time 在固定索引、status 字段尾随导致恒失败），且只在腾讯也失败时才轮到的末位冗余，价值不足以维护。
- fallback 选择以单个 provider 的完整 normalized quote 为单位；默认不做跨 provider 字段拼接。若未来引入 field-level merge，必须显式标记 `source = "mixed"` 并提供字段来源审计。
- quote 写入 snapshot 前先判断该 provider 输出是否满足当前用途的必需字段：展示至少需要 `price/tradeDate/capturedAt`，成交模拟还需要可用买一 / 卖一盘口。多个 provider 同时可用时，先比较 eligible trade date 和 freshness，再比较字段完整度，最后按 `TDX > 腾讯` tie-breaker。
- **可用性判定与候选选取**（实现锚：`domain/quotes/quote.rs::StockQuote::{is_display_complete, is_quote_complete}`、`pipeline/quotes/service.rs::pick_fallback_quote`）：
  1. 按 `TDX > 腾讯` 顺序逐个尝试 provider（BJ 跳过 TDX 直接腾讯），每个调用返回一条 normalized quote 候选。
  2. 遇到**首个** `is_quote_complete = true`（display 必备字段全有 + 五档盘口可用：bid[0]/ask[0] 含 price+volume）立即采纳并 short-circuit。
  3. 全程未命中 quote-complete 时，回退到**首个** `is_display_complete = true` 的候选；该候选缺盘口，写入 snapshot 时附 `depth_missing` warning。
  4. 全部 provider 都未达到 display-complete → 视为无可用 quote，不写 snapshot。
- **`is_display_complete` 的硬门槛 = `price` 非空**（`tsCode/category/tradeDate/capturedAt` 在 `StockQuote` 类型上非空，恒满足）。`previousClose` / `changePercent` 是**首选但非必备**字段：指数 / 基金在 TDX / 腾讯 多源下经常缺 `previousClose`，若强求会把它们全推进 fallback chain 且最终仍无更优候选。缺这两个字段时 UI 涨跌幅显示 `—`，可接受。这与 universe batch 接受门槛（§5 接受门槛：price 非空）一致——全局只有一个 price-only 谓词。
- `is_display_complete` 是 UI 展示和扫描的最低准入；`is_quote_complete` 仅在 Account 写路径成交模拟时作为可成交前提，**不**是 fallback 选取的硬条件——缺盘口的 display-complete quote 仍然可用于展示。
- `StockQuote.source` 与 `StockQuote.freshness.source` 必须一致；缺盘口的 fallback quote 可以用于展示，但必须带 `depth_missing` warning。
- Account 成交模拟需要 fresh quote 和盘口；fallback 源缺盘口时必须返回 `depth_missing`，是否可成交由 Account 交易规则判断。

#### TDX 连接池与并发（实现锚：`infrastructure/quotes/tdx/manager.rs`）

为压榨 TDX 吞吐并解耦"后台批量刷新"与"前台交互请求"（K 线 / 详情），TDX 连接层用**连接池**而非单连接：

- **连接池（动态并发，按低延时台数自适应）**：池大小 **N 不固定**，由 host 探测结果决定。各连接独立 socket + 独立 per-call 节流（`MIN_CALL_INTERVAL` 80ms per-connection）。每次调用 round-robin 取一条执行；失败换一台 host 重连。
- **并行探测全部候选 → 选低延时的做并发**（重要）：`HQ_HOSTS` 维护数十台候选（pytdx 主站/云行情/券商站，去重 ~50 台）。**并行**（每台一线程，`PROBE_TIMEOUT` 上限，墙钟 ≈ 最慢一台，非串行累加）探测各台 connect 延迟。候选越多越能挑到低延时台。可达的按延迟升序，取**低延时子集** = 延迟 `≤ 最快台 + 250ms` 的那些（自然排除"可达但慢"的台），**并发数 N = 低延时台数**，clamp **[2, 12]**（可达不足 2 则有几台用几台，≥1）。每条连接 **pin 到一台不同的低延时 host**（每台仅 1 连接）。
  - 好处：① 慢/死 host **不进池**（不像固定 N 会把槽 pin 到慢台拖慢）；② 按网络状况自适应——多台快则多并发、网络差则少而精；③ 每台只 1 连接 → 绕单台限频、单台抖动只影响该槽（换台兜底）。
  - 探测结果**缓存**（一次性），cold burst 期每台 ~5 req/s 数秒、稳态趋近 0，极安全。
  - **启动时后台预热**：app 启动即后台 `warm()`（探测选池 + 预建所有 active 连接），不阻塞 setup、不依赖交易时段。让首笔用户请求（含盘后/休市冷开）免去 ~3s 一次性探测 + 建连延迟、即暖态。（探测本身是惰性可触发的，但启动预热把这一次性成本提前到后台、移出用户首次交互的关键路径。）
- **交互解耦**：前台请求（`ensure_chart_data` / `fetch_data` / K 线分页 / `refresh_quotes`）和后台 universe 滚动共享连接池。universe 滚动只占 ~1 连接、负载平滑 → 其余 ~7 条随时给前台/agent，**不再有 universe 全量扫描那几秒把前台饿死**的问题。
- **并发批次**：cold-start burst（一次性全量）把 80-batch **并发**发起（并发度 ≤ N），~94 批跑在 8 连接上、完成时间 ~2.5s。稳态 universe 滚动则按节奏逐批推（~1 连接）。每批完成即写 cache（盘中只写 in-memory cache，线程安全；`purpose=close` 的 EOD 才写 `quote_close_snapshot`）+ emit progress。
- **`refresh_quotes` 内部**：传入 tsCodes 同样按 80/批切、并发跑在池上；先按新鲜度跳过 cache 内 < ~1.5s 的 code。
- **顺序无关**：并发后批次完成顺序不保证，但 progress 是中间态、前端读 cache/DB 重排，故 Stock→Index→Fund 仅影响入队顺序、不要求完成顺序。

日 / 周 / 月 K：

- **TDX 是主源**：日 / 周 / 月 K 全部从 TDX 拉取 unadjusted bar；单次拉取根数受 TDX 协议限制（默认 ~800 根），SH / SZ 全覆盖，BJ 不支持。
- **首屏单页（`fetch_kline_page`）**：`ensure_chart_data` 的默认路径只同步拉 `start=0` 的最新 ~800 根（首屏），落 DB 立即返回；更早历史由前端 background loop 调 `fetch_kline_page(start=800, 1600, ...)` 渐进 prepend 补全（见 §4 `ensure_chart_data`）。`start` 是从最新往回跳过的根数，更早 batch prepend 到累计 Vec 前，最终升序。
- **全量历史（`refresh_klines_full`）**：保留实现，走分页 loop —— TDX `security_bars(start=0, 800, 1600, ...)` 直到返回空 / 不足 800 根 / 命中硬上限 50 000 根，一次性同步把全量历史落 DB。供需要"同步落全量"的显式补段调用方使用；**不是** `ensure_chart_data` 的默认路径（首屏速度优先，见上）。
- **增量回溯（`refresh_klines` / `refresh_klines_extended`）**：每只 (ts_code, period) 先查 `max(trade_date) FROM quote_klines_daily WHERE adjust='none'`：
  - DB 空 → 初始拉过去 ~365 天（`history_days` 可加深，cap 至 TDX 单次上限 ~800 根）。
  - 有数据 → 从 `max+1` 拉到 today，幂等 upsert。
  - 用途：盘后调度补当日新 bar，不为 `ensure_chart_data` 的全量场景。
- **本地复权**：`qfq` / `hfq` 由 Quotes 基于本地 unadjusted K 线 + TDX xdxr 事件现算（见 §2 "本地复权计算"）；不依赖 TuShare adj_factor。
- **更深历史 = TDX 全量分页**（**不调 TuShare**，2026-06-02 决策）：回溯超出 TDX 单次 ~800 根时，走 `refresh_klines_full` 的分页 loop（`start=0,800,1600,…`）覆盖 TDX 可获取的全部历史段。TuShare K 线（`fetch_kline`）仅保留作**准确性测试 oracle**，不在产品 K 线路径。
- **Eastmoney fallback**：TDX 失败时可用 Eastmoney 补 SH / SZ 当日 / 近期段；BJ 没有日 / 周 / 月 K 备源，按 per-item warning / error 返回。
- 股票趋势 / 技术指标优先使用 `qfq`；只能用 `none` 时返回 `using_unadjusted_kline` warning。

分钟 K：

```text
TDX > Eastmoney
```

- TDX 是主源（SH / SZ）；Eastmoney 是 fallback。
- BJ 可以不支持，返回 per-item warning / error。
- **增量回溯（trading-session 感知）**：每只 (ts_code, period) 拉取前按 `is_in_trading_session(now_beijing)` 分支：
  - 交易时段内（含 09:15–09:30 集合竞价、午休、最后的 14:57–15:00）：直接拉远端，不查 `max_minute_kline_ts_ms`，因为当前 bar 可能仍在变化。
  - 盘后：查 `max_minute_kline_ts_ms`；若已存任意 ≥ 当日 09:15 (北京) 的 bar → skip 远端拉取（视为今日已 catch-up）；否则一次性 catch-up 当日 240 点（首次启动 / 之前网络失败的 backfill 路径）。
  - skip 路径不计入 `total`，保留 `success + failed == total` 不变量。
- 跨日切换时不主动清理旧分钟 K；读取按 `ts_ms` 自然排序、按调用方 `limit` 取最近的 N 点即可。

分时：

```text
TDX (minute_time 0x0fb4) > Eastmoney
```

- TDX `minute_time` 协议返回当日 240 点分时（09:30–14:59，每分钟一点），覆盖连续竞价段；**不含**集合竞价（09:15–09:25 / 14:57–15:00）。集合竞价价格由 `MARKET_SNAPSHOT` 的实时 quote 单独承载，不进 `IntradaySeries`。
- Eastmoney 仅作 TDX 失败 / BJ 的 fallback。
- 分时只返回一个交易日的 `IntradaySeries`，与 §4 `fetch_data` 契约一致。

基本面 / 公司事件 / 交易日历：

- **`daily_basic` 和公司事件**：仅当 `TushareHealthState.isAvailable = true` 时刷新；TuShare 不可用时这些读模型为空或保持上一次成功刷新结果，对应 freshness `status = "missing"` 或 `status = "stale"`，并附 `daily_basic_missing` / `events_missing` warning。Quotes 不提供这两类数据的 TDX 替代源。
- **交易日历**：默认通过本地推算获得（A 股周一至周五 工作日 + 内置中国法定假日 / 调休表，按年滚动维护）；当 `TushareHealthState.isAvailable = true` 时调用 TuShare `trade_cal` 校准本地推算结果，发现差异时以 TuShare 为准并记录修正日志。TuShare 不可用时使用纯本地推算结果，调用方可读但要意识到节假日特殊调整可能存在偏差。
- TuShare 任何路径失败都不得影响实时行情 / TDX K 线 / xdxr 刷新。

### 复权策略

复权用于消除分红、送股、转增、配股导致的历史价格断层。

| 模式 | 含义 | 用途 |
|---|---|---|
| `none` | 原始成交价，不复权 | 盘口附近、短线真实价格 |
| `qfq` | 前复权，当前价格不变，历史价格修正 | 图表展示、趋势、技术指标 |
| `hfq` | 后复权，早期价格不变，后续价格修正 | 长期收益率研究 |

规则：

- TDX K 线本地落库的 `adjust = none`（unadjusted 是真源行）。
- `qfq` / `hfq` 由 Quotes 基于本地 unadjusted K 线 + TDX xdxr 事件**现算**（见 §2 "本地复权计算"），不依赖 TuShare。
- K 线展示优先 `qfq`；本地有 unadjusted 但 xdxr 缺失 / 加载未完成时退化为 `none`。
- 趋势 / 技术指标判断优先 `qfq`。
- 只能用 `none` 时必须返回 `using_unadjusted_kline` warning：

```text
不复权，除权除息附近的跳空可能扭曲趋势和技术指标
```

- xdxr 事件本身的刷新策略：启动后预热关注标的的 xdxr；盘后随 K 线刷新一同补拉。xdxr 出错或缺失只影响复权 series，不影响 unadjusted 读取。

### 后台刷新

Quotes 提供 refresh use case；触发节奏和 scope 由模块外运行时传入，Quotes 不关心 scope 来源。下表是推荐默认值，实际调度权威写在 [agent-runtime-module.md](agent-runtime-module.md)。

| 数据 | 策略 |
|---|---|
| Cold-start seed | 进程启动时把 `BUILTIN_INSTRUMENTS` upsert 入 `quote_instruments`，保证 UI 第一帧非空 |
| 全市场列表 | 启动 + 每日 08:30：TDX 基础 universe；`TushareHealthState.isAvailable = true` 时 enrich |
| TuShare 健康探针 | 进程启动时首次 ping；`isAvailable = false` 时每 1 小时重试 |
| 实时行情（背景基线） | **唯一后台报价任务 = universe 滚动刷新**：把全市场切 80 只/批，**按固定周期（默认 30s，可配 10–60s）滚动轮刷**——每 ~`cycle/批数` 推一批、每只每 `cycle` 轮到一次（不再"每 60s 一次性全量扫"的锯齿）。只在 `is_in_quote_refresh_window` 内跑、占 ~1 连接、负载平滑。职责：屏外行 / 全列表排序基线 / **headless（agent 无前端）兜底**。每批 emit `market-quotes-refresh-progress` 驱动前端增量更新。读取 freshness 按 `detail = 30s`、`universe = 90s` 判断 stale |
| 实时行情（聚焦按需）| **前端驱动 pull：`refresh_quotes(tsCodes)`**（见 §前端命令）。前端把「可见 ∪ 自选 ∪ 核心指数 ∪ 选中」**取并集去重**后按自身节奏（~3s）调它 → 后端 TDX batch 拉这些 → 写 in-memory snapshot → emit progress。这是用户**实际在看**的那一小撮的实时路径（替代原 hot/subscribed 档与 hotset 机制）。agent / account pipeline 下单前也可调同一 use case 取即时报价。**新鲜度跳过**：`refresh_quotes` 对 cache 内 `capturedAt` 仍很新（< ~1.5s）的 code 跳过不重拉，天然去重 + 限流（无需 in-flight 合并队列）|
| 收盘快照 | 收盘后执行全市场 quote refresh，写入 `tradeDate = latestCompletedTradeDate` 的最终行情；失败时可低频重试直到获得最新已完成交易日快照，不做整夜持续刷新 |
| K 线（unadjusted） | 启动后预热关注标的；盘后 16:00 走 TDX 补日 / 周 / 月；TDX 单次根数不够且 TuShare 可用时按需扩展长历史段 |
| xdxr 事件 | 启动后预热关注标的；盘后随 K 线刷新一同补拉，按 `tsCode` 幂等 |
| `daily_basic` | 每个交易日盘后刷新，仅在 `TushareHealthState.isAvailable = true` 时触发；不可用时跳过并保留上次结果 |
| `company_events` | 每日低频刷新，覆盖未来 N 天事件窗口；仅在 `TushareHealthState.isAvailable = true` 时触发 |
| 交易日历 | 进程启动时使用内置推算结果；`TushareHealthState.isAvailable = true` 时每日校准一次 |

时段判定：

- **`is_trading_time`**：是否处于**可交易报价时段**（连续竞价 09:30–11:30 + 13:00–15:00）。用于 §2 quote 有效性（盘中 1h 硬过期 / 非盘中读最新已完成交易日）与 Account 成交时段判断。**不是** scheduler 刷新节奏的判据。
- **`is_in_quote_refresh_window`**：universe 滚动刷新的节奏判据 = **连续竞价 + 每个 session 收盘后 30min 尾窗**，即交易日的 **09:30–12:00 ∪ 13:00–15:30**（北京时）。窗内才滚动刷新并写 in-memory snapshot（读路径照常服务最新 snapshot），以捕获收盘后稍晚落定的最终价、并让行情不在 11:30 / 15:00 整点冻结。与 `is_trading_time`（可交易性）解耦——尾窗内**不**可交易、Account 仍 fail closed。15:30 收盘快照 / 16:00 K 线预热不变。前端 `refresh_quotes` pull **不受**此窗限制（用户任何时候打开都该拉到最新可得快照；非交易时段拉到的即当日/最近已完成交易日事实）。

- **冷启动 universe burst 首刷**：启动 catch-up 在刷新窗内**先跑一次性全量 universe burst**（用满连接池 ~2.5s 填满全市场），**之后转入 30s 滚动稳态**。burst 解决"冷启动全列表填充快"，滚动解决"稳态平滑、不占满连接"。与 close-snapshot catch-up（仅非刷新窗 / 快照不全时补最新已完成交易日收盘）互补、不重复。
- **`is_in_trading_session`**：是否处于**分时 / 分钟 K 数据可能变化的时段**（09:15 集合竞价开始 ~ 15:00 收盘集合竞价结束），用于 `refresh_intraday` / `refresh_minute_klines` 的盘前 / 盘后 guard。15:00:00 整点视为**仍在 session 内**（避免与最后一个分钟 K bar 写入冲突）。

规则：

- Quotes 不读取其他 bounded context 的内部实现。
- Quotes refresh scope 是 use case 入参。
- 收盘快照用于维护展示 / 分析可用的最后行情事实，不表示可交易。Account 仍必须按交易日历和交易时段规则禁止即时成交。
- 收盘快照 refresh 按 `tradeDate` 幂等；完成状态至少记录 `tradeDate`、完成时间、覆盖总数和成功数，供启动 catch-up 和 diagnostics 判断是否已有最新已完成交易日快照。
- **refresh_state 读写契约**：每个 refresh use case 完成时（含 partial failure）写入本地表 `quote_refresh_state` 一行 `(refresh_kind, trade_date, total, success, failed, completed_at)`，按 `(refresh_kind, trade_date)` upsert 幂等。
  - `refresh_kind` 枚举值：`"close"` / `"intraday"` / `"kline"` / `"minute_kline"` / `"daily_basic"` / `"events"` / `"xdxr"`。Quotes 不接受自由字符串；新增 kind 必须先扩展 spec。
  - 启动 catch-up：通过 `read_refresh_state("close", latestCompletedTradeDate)` 配合 `close_snapshot_complete(trade_date)` 判断是否需要补当天收盘快照；缺失或失败比例过高都视为需要补。
  - diagnostics：可按 `refresh_kind` 查最近一次完成情况（`completed_at` / `success` / `failed`），用于 UI 健康检查面板和 drift 审计。
  - 表本身不承载业务读取语义；调用方读 quote / kline / events 时不查这张表，只走对应读模型表。
- 非交易时段不为了维持 `capturedAt < 1h` 持续刷新 quote；只要 quote 的 `tradeDate` 等于最新已完成交易日，就可用于读取。
- 如果 app 暂停、网络不可用或 provider 失败导致缺少最新已完成交易日 quote，非交易时段读取接口按 `snapshot_expired` / `quote_missing` 返回空 quote。
- `market-quotes-refreshed` 只表示 snapshot 已更新；payload 使用 [shared-types.md](shared-types.md) 定义的 `MarketQuotesRefreshedPayload`，其中 `purpose = "close"` 表示收盘快照，`purpose = "intraday"` 表示盘中 / 手动常规刷新；下游重建和事件路由由模块外编排处理。
- `MarketQuotesRefreshedPayload.affectedTsCodes` 在 `subscribed` / `manual` scope 下必须尽量填写成功写入 snapshot 的标的集合；`universe` scope 数据量过大时可以省略。消费者看到 `affectedTsCodes` 缺失时必须按 `scope` 做全量重读 / 重建。
- `failedBatches > 0` 表示本轮 quote refresh 部分失败；事件仍可 emit，但消费者必须把本次读取视为 partial，不得把缺失标的解释为确定无数据。
- **universe scope 的两段 `refreshed`**：universe 采用 TDX 主批同步 + fallback 异步（见下「全市场执行契约」）。因此 universe 会 emit **两条** `market-quotes-refreshed`：
  1. 同步首条：TDX 主批结束即 emit，`success` = TDX 命中数，`failedBatches` = 延后进 fallback 的标的数（BJ + TDX 失败/不完整）。TDX 整体故障时 `failedBatches` ≈ total，消费者据此知道本轮 partial，**禁止**误判为全成功。
  2. fallback 完成后由后台任务 emit 修正条：`success` / `failedBatches` 含 腾讯（EM 退出报价、新浪已移除） fallback 结果，作为本轮最终汇总。
  消费者必须容忍同一轮多条 refreshed（以最后一条为准，或按 progress 增量重读）。`subscribed` / `manual` scope 仍只 emit 一条同步 refreshed。

### 全市场 quote 刷新执行契约

`refresh_market_quotes(scope = "universe")` 一次需要扫 ~7500 标的，是吞吐敏感路径。Quotes 对该路径**强制契约**如下：

1. **批量 RPC 强制**：universe scope 实现必须用 TDX `get_security_quotes` 批量调用（每批 ≤ 80 标的），不允许逐只串行；其他 provider fallback 沿用原逐只路径。理由：TDX 协议层已经支持批量并自带 ≥80 ms/批节流，串行方案在节流下吞吐 ~6 只/秒，批量 ~80×80ms/秒 ≈ 1000 只/秒。
   - **接受门槛**：universe batch 路径用 `is_display_complete`（只要 `price` 非空就接受为最终值），**不**用 `is_quote_complete`。理由：指数 / 基金 TDX 不返回 bid/ask 五档，且 腾讯也无；用 `is_quote_complete` 会把所有指数 / 基金错误地推进 fallback chain，每只浪费 ~600ms。`is_quote_complete` 是 fallback chain 多 provider 之间挑选的"字段完整度优先"裁判，不是 universe batch 主源的接受门槛。
   - **fallback 触发面**：只在 TDX `Err` 或 `Ok` 但 `price` 为空时，把该标的推入 fallback queue（腾讯（EM 退出报价、Sina 已移除））。
2. **fallback 非阻塞（async）**：universe scope 的 fallback queue **不得阻塞主流程**。TDX 主批跑完即视为本轮"主体完成" —— 立即写 `quote_refresh_state`（TDX 主批成功数）+ emit `market-quotes-refreshed`，然后把 fallback queue 丢到后台任务并发处理（腾讯（EM 退出报价、新浪已移除），建议并发上限 8），完成一个补一个 cache / `close_snapshot` 并 emit progress，结束后**重写** `quote_refresh_state` 反映含 fallback 的最终成功数。
   - 理由：universe 里 TDX 失败的多是退市 / 停牌 / 北交所，少量但每只串行跑 3 个 HTTP provider（~440ms）会把一轮拖到 ~58s 吃满 60s 周期，导致刷新近乎连续、与交互请求抢 TDX 连接。异步化后一轮同步耗时 = TDX 主批（~20s）。
   - **例外**：`subscribed` / `manual` scope（用户显式关注 / 点击的具体标的）的 fallback 仍**同步**等待——这些场景调用方需要拿到确定结果。"非阻塞"只对 universe scope 生效。
3. **吞吐目标**：在 TDX 健康、网络正常的前提下，universe scope 一轮**主体完成**（TDX 主批 + emit refreshed）时间 **≤ 30s**（universe ~7500）；后台 fallback 不计入主体完成时间。超出视为 provider 或 IO 异常，写入 `quote_refresh_state.failed`。
4. **执行顺序**：universe scope 标的按 `InstrumentCategory` 排序进入批次队列 —— **`Stock` → `Index` → `Fund`**；同 category 内顺序不约束。理由：用户首屏感知优先级是 A 股票，索引和基金次之；按类别交付让"看得见的部分"先就绪。
5. **进度事件**：universe scope 必须 emit `market-quotes-refresh-progress`，payload 见 [shared-types.md](shared-types.md) `MarketQuotesRefreshProgressPayload`。emit 触发节奏由实现决定，最低粒度 **N = 80**（TDX transport batch size），上限 `N = 200`；推荐实现用 streaming 模式 —— pipeline 按 80 一批驱动 TDX，每批完成后 write DB + emit progress，让首次安装期间 UI 持续流式填充。后台 fallback 也复用同一事件按完成进度补发。前端订阅该事件做**增量 UI 刷新**（市场列表 / 核心指数卡 / 标的详情头 / 账户自选等所有行情视图），**不要靠 polling 撞数据**；各视图读同一 quote cache、按同一 progress 节奏刷新，保证同一标的的最新价跨视图一致。允许保留低频 polling（如 30s）作为无 progress 时段（非交易时段 / 事件丢失）的兜底，但交易时段以 progress 驱动为主。注意：报价（quote snapshot）与 K 线序列是**不同读模型**，K 线某根 bar 的收盘价与实时报价天然存在 tick-vs-聚合 的秒级差异，不要求逐值相同。终态仍以 `market-quotes-refreshed` 为准（注意：universe scope 下 refreshed 在 TDX 主批后即发，后台 fallback 的增量只通过 progress + cache 体现）；progress 是中间态，消费者不得用其覆盖 `quote_refresh_state` 最终行。
6. **resume 语义**：catch-up（`purpose = "close"`）必须先查 `list_close_snapshot_ts_codes(tradeDate)` 过滤已有标的，重启后从断点接续，不重跑已成功条目。`skip_existing` 行为只对 `purpose = "close"` 生效；`intraday` 仍刷全 universe（覆盖盘中变化）。

### 核心指数集合

Quotes 拥有默认 headline 核心指数集合，并通过 `core_indexes()` 暴露给外部调度：

```text
000001.SH  上证指数
399001.SZ  深证成指
399006.SZ  创业板指
000300.SH  沪深300
```

规则：

- 外部调度只调用 `core_indexes()` 合并 refresh scope，不内嵌指数列表。
- 这组指数是系统默认市场背景，不是用户偏好；用户自定义关注指数属于 Agent Runtime / preferences，不改变 Quotes 的默认集合。
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
- 对外读取路径默认不直接请求 TDX / EM / TuShare / 腾讯。
- TDX 是 Quotes 数据主源；腾讯是 TDX 实时行情 fallback；Eastmoney 作 BJ universe + K线/分时/日线兜底；TuShare 仅在 `TushareHealthState.isAvailable = true` 时作为 enrich 调用。
- TuShare token 缺失 / 健康检查失败时，Quotes 仍能基于 TDX 提供 universe、实时行情、K 线、xdxr、复权、分时、分钟 K 的完整能力；只是 `daily_basic` / 公司事件 / TuShare-only 字段为空。
- `qfq` / `hfq` K 线在本地基于 TDX xdxr 现算，不依赖 TuShare `adj_factor`。
- 进程启动时执行 cold-start seed（`BUILTIN_INSTRUMENTS`），保证 UI 第一帧非空。
- `list_market({ includeQuote: true })` 只读取 `MarketInstrument` 本地读模型 + `MARKET_SNAPSHOT`，缺实时字段时 `quote` 为空，不触发远端补拉。
- 连续竞价时段超过 1 小时的当日 quote 不得返回；非交易时段可返回最新已完成交易日 quote；`tradeDate` 不匹配 eligible trade date 的 snapshot 不得返回。
- `fetch_data({ tsCodes, include })` 只读本地 DB / snapshot；需要远端刷新必须走显式 refresh / 后台任务。
- `fetch_data.tsCodes` 必须校验格式、数量上限和请求顺序。
- `scan_market` 返回候选排名结果和 snapshot 覆盖率；需要详情时再调用 `fetch_data({ tsCodes })`。
- `market_breadth` 仅统计 `category == stock`；涨停 / 跌停判定与 `compute_limit_band` 阈值一致（主板 10% / 创业板 / 科创板 20% / 北交所 30% / ST 5%）。
- `industry_heatmap` 按 `MarketInstrument.sector` 聚合 stock 标的；sector 缺失 / 空串归入"未分类"且不参与 top 列表。
- `MARKET_SNAPSHOT` item 带 `category/tradeDate/capturedAt/source`，对外 freshness 由 query facade 派生；breadth 只统计 `category == stock`。
- 分钟 K / 分时通过 series-level freshness 表达，不在每个点位重复 freshness。
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

---

## 修订记录

**2026-05-28** — Quotes 数据主源全面切换至 TDX：

- universe / 实时行情 / 日 / 周 / 月 K / 分钟 K / 分时 / xdxr / 复权 全部以 TDX 为主源（TDX 协议层已具备 xdxr `0x000f` 和 minute_time `0x0fb4` 能力）。
- 新增 `TushareHealthState` 机制：启动 ping + 每 1 小时重试 + 连续失败熔断；所有 TuShare 路径调用前 gate 在 `isAvailable` flag 上。
- TuShare 改为可选 enrich：仅在 token 配置且健康检查通过时拉 universe enrich、`daily_basic`、公司事件、交易日历校准。（**K 线已于 2026-06-02 移出 TuShare** —— 全程 TDX-only 分页；`fetch_kline` 仅留作准确性测试 oracle。）
- 复权计算改为本地基于 TDX xdxr 现算（unadjusted K 线 + xdxr 事件），不再依赖 TuShare `adj_factor`。
- 新增 cold-start seed：启动时把内置 `BUILTIN_INSTRUMENTS` upsert 入 `quote_instruments`，保证 UI 第一帧非空。
- 交易日历改为本地推算 default + TuShare 校准 optional。

**2026-05-28 (三)** — universe scope 刷新执行契约：

- §5 后台刷新新增 "全市场 quote 刷新执行契约" 段，规定批量 RPC 强制、≤ 1 分钟吞吐目标、`Stock → Index → Fund` 执行顺序、每 200 只 emit 进度事件、`purpose=close` 的 resume 语义。
- shared-types.md 新增 `MarketQuotesRefreshProgressPayload`，对应新 event `market-quotes-refresh-progress`；前端不再靠 30s polling 撞 universe catch-up 数据，订阅该事件做增量列表刷新。
- 触发原因：冷启动测试发现 universe close catch-up 串行调用 TDX，~6 只/秒，7500 只需 ~19 分钟；TDX 协议层 `get_security_quotes` 批量已存在，pipeline 没用上。

**2026-05-28 (二)** — Spec drift 补正文（实现已落，spec 落后于代码）：

- §2 "本地复权计算"：补 xdxr 缺失三态语义（A 全局未刷新 / B 标的天然无除权 / C 部分历史缺失），以及联合 `quote_refresh_state` 判态的方法。
- §2 "TuShare 健康状态"：补熔断细节（计数器在每次业务调用累加 / 任意成功重置 / 跨重启不持久化）、`recheck_interval` 默认 1 小时、`max_consecutive_failures` 默认 3、探针绕过计数器。
- §5 universe step 0：把 cold-start seed 描述从一行扩成完整段落，落字段构成、source = "tdx"、覆盖窗口 ~5–10 s、size 下限 60 runtime 断言、ts_code 覆盖规则。
- §5 后台刷新：新增 "refresh_state 读写契约" 段，列出 `refresh_kind` 枚举、启动 catch-up 流程、diagnostics 用途。
- §5 实时行情 fallback：补 "可用性判定与候选选取" 4 步流程；明确 `is_quote_complete` 与 `is_display_complete` 各自的准入语义和取舍顺序，挂实现锚 `pipeline/quotes/service.rs::pick_fallback_quote`。
- §5 日 / 周 / 月 K：补增量回溯路径（`max(trade_date)` + 1 → today；空 DB 拉 ~365 天）；更深历史走 `refresh_klines_full` TDX 全量分页（K 线 TDX-only，不调 TuShare）。
- §5 分钟 K：补 trading-session 感知的增量回溯（盘内直拉、盘后查 `max_minute_kline_ts_ms` 决定 skip / catch-up；skip 不计 total）。
