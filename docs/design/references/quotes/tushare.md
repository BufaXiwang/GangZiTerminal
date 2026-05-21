# TuShare Quotes Reference

> 本文档是 Quotes 模块 TuShare Pro adapter 的渠道契约。模块级领域契约见 [../../quotes-module.md](../../quotes-module.md)。

## 定位

TuShare 是 Quotes 的历史、复权、基本面、公司事件和 universe enrich 主源。

TuShare 不作为盘中实时报价主路径；盘中交易判断依赖 `MARKET_SNAPSHOT` 中的 fresh quote。

## 能力范围

| 数据 | TuShare 接口语义 | 输出 |
|---|---|---|
| 股票 universe | `stock_basic` | `market_instruments(category=stock)` |
| 指数 universe | `index_basic` | `market_instruments(category=index)` |
| 基金 universe | `fund_basic` | `market_instruments(category=fund)` |
| 股票日 / 周 / 月 K | `daily` / `weekly` / `monthly` | `KlineSeries(adjust=none)` |
| 指数 K | `index_daily` / `index_weekly` / `index_monthly` | `KlineSeries(adjust=none)` |
| 基金 K | `fund_daily` | `KlineSeries(adjust=none)` |
| 复权因子 | `adj_factor` | 生成 `qfq` / `hfq` |
| 每日基础指标 | `daily_basic` | `DailyBasic` |
| 公司事件 | `dividend` / `suspend_d` / `namechange` / `forecast` / `share_float` | `CompanyEvent` |
| 交易日历 | `trade_cal` | Shared trading calendar |

能力清单来源见 [tushare-capabilities.md](tushare-capabilities.md)。

基金 universe 只纳入有交易所 `TsCode`、可作为场内标的展示 / 刷新的基金；场外基金不进入 `MarketInstrument` 主 universe。

## 获取方式

- 使用 TuShare Pro token。
- token 缺失时，TuShare adapter 不参与 refresh，并返回可解释 warning。
- refresh 应支持按日期 / 标的增量拉取。
- 受限于 TuShare 积分和频率时，adapter 必须退避并保留本地旧数据。

默认限制：

| 项 | 默认 |
|---|---:|
| request timeout | 10s |
| retry | 1 次 |
| rate limit | 按 token 配置 |
| long history batch | 按接口上限分页 |

## Normalize 规则

通用规则：

- `ts_code` 原样作为 `TsCode`。
- `trade_date` normalize 为 `TradeDate(YYYYMMDD)`。
- TuShare 数量和金额单位必须转换为 shared canonical 单位。
- 原始 payload 可保留在 provider cache / debug，不进入对外 DTO。

K 线：

| TuShare 字段 | Canonical 字段 |
|---|---|
| `open` / `close` / `high` / `low` | `open` / `close` / `high` / `low` |
| `vol` | `volume`，normalize 为股 / 份 |
| `amount` | `amount`，normalize 为 CNY |
| `trade_date` | `date` |

复权：

- `adjust = none` 来自原始 K。
- `qfq` / `hfq` 由 `adj_factor` 和原始 K 派生。
- 复权结果必须保持 `source = "tushare"`。
- 复权计算失败时，不能写入错误 qfq；返回 `qfq_missing` 或 `using_unadjusted_kline`。

DailyBasic：

- `pe_ttm` -> `peTtm`。
- `turnover_rate` -> `turnoverRate`。
- `total_mv` / `circ_mv` normalize 为 CNY。

CompanyEvent：

- 分红、停复牌、ST / 曾用名、业绩预告、限售解禁统一写入 `CompanyEvent`。
- 无法结构化的字段放入 `payload`，但必须保留 `eventType`、`source`、`fetchedAt`。

## Fallback 条件

- TuShare token 缺失：跳过 enrich / 历史 / 复权 refresh，返回 warning。
- TuShare quota / rate limit：保留旧数据并记录 heartbeat；不清空读模型。
- TuShare 某接口不可用：该数据域 partial failure，不影响其他数据域 refresh。

## 验收标准

- TuShare universe enrich 不得删除 TDX 已发现但 TuShare 暂缺的标的。
- 复权 K 失败时不得写入错误 qfq / hfq。
- `daily_basic` 缺失时扫描不能把缺失指标当作 0。
- TuShare 失败不影响实时行情 snapshot 读取。
