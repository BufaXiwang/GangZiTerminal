# TuShare Quotes Reference

> 本文档是 Quotes 模块 TuShare Pro adapter 的渠道契约。模块级领域契约见 [../../quotes-module.md](../../quotes-module.md)。

## 定位

TuShare 是 Quotes 的 **enrich 层**（基本面 / 公司事件 / 交易日历 / universe enrich），**不是** K 线 / 复权 / 实时报价主源。

- **K 线主源是 TDX**（全量分页覆盖历史）；TuShare **不再**提供产品 K 线（2026-06-02 决策）。`fetch_kline` 仅保留作**准确性测试 oracle**（跨源校验 TDX 日 K），不进产品路径。
- **复权（qfq/hfq）走本地 TDX xdxr 算法**，不用 TuShare `adj_factor`。
- TuShare 不作为盘中实时报价主路径；盘中交易判断依赖 `MARKET_SNAPSHOT` 中的 fresh quote。

## 能力范围（enrich-only）

| 数据 | TuShare 接口语义 | 输出 |
|---|---|---|
| 股票 universe enrich | `stock_basic` | `market_instruments(category=stock)` 行业/状态/上市日期 |
| 指数 universe enrich | `index_basic` | `market_instruments(category=index)` |
| 基金 universe enrich | `fund_basic` | `market_instruments(category=fund)` 基金分类/管理人 |
| 每日基础指标 | `daily_basic` | `DailyBasic`（PE/PB/换手/市值） |
| 公司事件 | `dividend` / `suspend_d` / `namechange` / `forecast` / `share_float` | `CompanyEvent` |
| 交易日历 | `trade_cal` | Shared trading calendar |
| ~~K 线 / 复权因子~~ | ~~已移除~~ | K 线走 TDX 全量分页；复权走本地 xdxr |
| （仅测试 oracle）日 K | `daily` (`fetch_kline`) | 跨源校验 TDX 日 K，不进产品 |

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

K 线（仅 `fetch_kline` 测试 oracle 用；产品 K 线在 TDX）：

| TuShare 字段 | Canonical 字段 |
|---|---|
| `open` / `close` / `high` / `low` | `open` / `close` / `high` / `low` |
| `vol` | `volume`，normalize 为股 / 份（`vol` 手 × 100） |
| `amount` | `amount`，normalize 为 CNY（千元 × 1000） |
| `trade_date` | `date` |

复权：**不在 TuShare**。`qfq` / `hfq` 由本地 TDX xdxr 事件现算（见 quotes-module.md §2 本地复权）；TuShare `adj_factor` / `apply_adjust` 已移除。

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
