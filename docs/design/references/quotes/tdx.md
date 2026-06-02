# TDX Quotes Reference

> 本文档是 Quotes 模块 TDX adapter 的渠道契约。模块级领域契约见 [../../quotes-module.md](../../quotes-module.md)。

## 定位

TDX 是 A 股 SH / SZ 实时报价主路径，也可提供快速展示用的不复权日 / 周 / 月 K 和部分分钟数据。

TDX adapter 只负责获取和 normalize 数据，不定义 Quotes 对外 API。

## 能力范围

| 数据 | 角色 | 输出 |
|---|---|---|
| SH / SZ 股票实时行情 | 主源 | `StockQuote` |
| SH / SZ 指数实时行情 | 主源 | `StockQuote` |
| 场内基金实时行情 | 可用时主源 | `StockQuote` |
| 日 / 周 / 月 K | 快速主源 | `KlineSeries(adjust=none)` |
| 分钟 K / 分时 | 优先源 | 分钟 K / 分时读模型行 |
| BJ 标的 | 不保证 | 失败后 fallback Eastmoney |
| 复权 K | 不支持 | fallback TuShare |

## 获取方式

TDX 通过通达信 HQ 行情 TCP 协议提供数据，wire format 兼容 pytdx / mootdx 的 HQ API。

连接流程：

```text
内置 HQ server 列表
  -> connect_bestip(timeout)：并发尝试多个 host
  -> TCP connect
  -> 3-step TDX handshake
  -> 得到 TdxHqClient
  -> 发送 HQ command
  -> 解码二进制 response
  -> normalize 成 Quotes canonical model
```

规则：

- server 列表、host 选择和测速属于 adapter 配置，不进入 Quotes 模块 spec。
- client 是同步 TCP；异步 runtime 中必须通过 blocking wrapper 执行，不能阻塞 async executor。
- 实时报价可复用共享 client；K 线可以使用独立 client，避免低频 K 线请求污染高频报价连接。
- 任一请求失败后必须丢弃当前 client，下次重新 `connect_bestip()`。
- 单批失败后可以切换下一个 TDX server 重试；仍失败则进入 Quotes fallback。
- adapter 必须支持 batch 请求，避免逐标的串行拉取。

TDX HQ command 能力：

| HQ command 语义 | 用途 | 输出 |
|---|---|---|
| `security_count(market)` | 获取 SH / SZ 证券数量 | universe 分页上限 |
| `security_list(market, start)` | 分页获取证券列表 | `MarketInstrument` 最小档案 |
| `security_quotes([(market, code)])` | 批量获取实时 L1 行情和五档盘口 | `StockQuote` |
| `security_bars(category, market, code, start, count)` | 获取 K 线 / 分钟线 | K 线 / 分钟 K / 分时读模型行 |

默认限制：

| 项 | 默认 |
|---|---:|
| connect timeout | 5s |
| quote batch size | 80 |
| K 线单次上限 | 800 bars |
| server failover | 至少尝试可用内置 host |
| retry | 失败后重连；重试次数可配置 |

## Market 和代码范围

TDX HQ market 只支持：

| TDX market | 含义 | TsCode |
|---:|---|---|
| `0` | 深圳 | `*.SZ` |
| `1` | 上海 | `*.SH` |

规则：

- BJ 不在 TDX HQ market 范围内；BJ 标的必须跳过 TDX 并 fallback Eastmoney。
- TDX adapter 必须由已知 `TsCode` 派生 TDX market 参数；不得接收非 `TsCode` 标识作为路由输入。
- `security_list` 只能提供最小档案，不是权威行业 / 状态 / 基本面来源。

## Normalize 规则

实时行情映射到 `StockQuote`：

| TDX 字段语义 | Canonical 字段 |
|---|---|
| 代码 | `tsCode`，由 Quotes universe 确定市场 |
| 名称 | `name` |
| 最新价 | `price` |
| 昨收 | `previousClose` |
| 今开 | `open` |
| 最高 / 最低 | `high` / `low` |
| 成交量 | `volume`，normalize 为股 / 份 |
| 成交额 | `amount`，normalize 为 CNY |
| 买一到买五 | `bid[]` |
| 卖一到卖五 | `ask[]` |
| 采集时间 | `capturedAt` |
| 数据源 | `source = "tdx"` |

规则：

- 不能用昨收、开盘价或 0 值伪造 `price`。
- 盘口缺失时返回 `depth_missing` warning。
- 成交量 / 成交额单位必须 normalize 到 [shared-types.md](../../shared-types.md)。
- TDX 不直接给出可靠 `tradeStatus`；adapter 只 normalize 可用原始状态，最终对外 `tradeStatus` 由 Quotes query facade 按 `MarketTimeContext`、instrument status 和 quote eligibility 派生。
- TDX `SecurityQuote` 不包含可靠名称时，`name` 可为空；展示层必须从 `MarketInstrument` 补名。
- TDX 价格、盘口、成交量字段为 0 或非法值时按 missing 处理，不能转成有效 0。
- **价格小数位缩放（关键）**：TDX `security_quotes`（实时报价）的价格整数按**该标的的小数位**编码（integer = 真值 × 10^小数位），但协议层 `cal_price` 统一 `/100`（假设 2 位）。实测各类小数位不同：
  - **股票 / 指数 = 2 位**（×100）→ 协议 `/100` 已正确 → 不校正。
  - **场内基金 / ETF = 3 位**（×1000）→ 协议 `/100` 余 ×10 → adapter `×0.1`（如 510300 实际 4.868，协议出 48.68）。
  - **债券（可转债等）= 4 位**（×10000）→ 协议 `/100` 余 ×100 → adapter `×0.01`（实测 110073 协议出 raw.price=10694 ⇔ 真值 106.94，与腾讯一致）。

  adapter `map_security_quote` 按 `is_bond` / `category` 推导小数位做对应 `scale`（debond=0.01、Fund=0.1、其余=1.0）。债券判定 `universe::is_bond(market, code)`（SH 1xxxxx、SZ 1xxxxx 除 159 ETF）。这样**按需取债券**（universe 外，见 [quotes-module.md](../../quotes-module.md) §2）报价正确。注：`security_bars`（K 线）协议层用 `/1000`，对各类标的都正确，不需校正。

## K 线规则

- TDX `security_bars` 支持 `1m / 5m / 15m / 30m / 60m / day / week / month`。
- TDX K 线只输出 `adjust = "none"`。
- TDX K 线不能作为 qfq / hfq 来源。
- 指标计算优先使用 TuShare qfq；只有缺 qfq 时才能使用 TDX none，并返回 `using_unadjusted_kline`。
- TDX 单次 bars 有服务端上限，长历史必须分页或由 TuShare 补。
- TDX 分时使用分钟 bars 转成 `MinutePoint`：`price = close`，`average` 可由累计成交额 / 成交量派生。

## Fallback 条件

出现以下情况时 Quotes 可以 fallback 到 Eastmoney / TuShare：

- TDX server 连接失败或超时。
- 目标标的不在 TDX 支持范围。
- 返回价格缺失、盘口缺失且调用场景需要盘口。
- BJ 标的。
- 需要 qfq / hfq / 长历史。
- `security_bars` 返回 0 根或解码失败。

## 验收标准

- TDX quote adapter 输出必须能 normalize 成 `StockQuote`，并包含 `source = "tdx"`、`capturedAt`、`freshness`。
- 缺盘口时返回 `depth_missing`，不能让 Account 假装成交。
- TDX K 线只写 `adjust = "none"`。
- TDX 失败不让整批 Quotes 读取失败；对应 item 进入 fallback 或返回 item error。
- BJ 标的不得发送到 TDX HQ；必须直接 fallback Eastmoney。
- TDX client 请求失败后必须丢弃连接并允许下次重连。
