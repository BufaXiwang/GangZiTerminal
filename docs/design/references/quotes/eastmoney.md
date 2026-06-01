# Eastmoney Quotes Reference

> 本文档是 Quotes 模块 Eastmoney adapter 的渠道契约。模块级领域契约见 [../../quotes-module.md](../../quotes-module.md)。

## 定位

Eastmoney 在 Quotes 里承担两类**不可替代 / 低风险**的角色：**BJ universe 枚举**（列出北交所标的，其他源做不到）+ **K 线 / 分时 / 日线兜底**（CSV 真值解析）。

**Eastmoney 不参与实时报价**（2026-06-01 起）：其 push2 `stock/get` 价格按 `10^f59` 缩放（曾硬编码 `/100`，对 3 位小数 ETF 产生 10× 错价），且五档字段映射 / BJ secid 前缀无法在受限环境实测确认。为避免数据源错误，实时报价收敛到 TDX → 腾讯，EM 退出报价路径。

Eastmoney 不是对外 API；所有输出必须 normalize 到 Quotes canonical model。

## 能力范围

| 数据 | 角色 | 输出 |
|---|---|---|
| BJ universe 枚举 | **主源（独有）** | `(code6, name)` → `MarketInstrument` |
| 分钟 K | fallback / 补充源 | 分钟 K 读模型行 |
| 分时 | fallback / 补充源 | 分时读模型行 |
| 日线 K | fallback（TuShare 不可用时） | 日 K 读模型行 |
| ~~SH / SZ / BJ 实时行情~~ | ~~已移除~~ | 改由 TDX → 腾讯 |

## 获取方式

- 使用 Eastmoney Web quote / kline 接口。
- endpoint、query 参数和 host 属于 adapter 细节，写在实现配置，不进入模块 spec。
- adapter 必须设置 User-Agent 和 timeout，避免 UI / Agent 热路径卡死。
- 对非官方接口变更必须 fail closed，并返回 `provider_unavailable` 或 item warning。

默认限制：

| 项 | 默认 |
|---|---:|
| quote timeout | 5s |
| kline timeout | 8s |
| retry | 1 次 |
| batch size | adapter 自定，必须可配置 |

## Normalize 规则

| Eastmoney 字段语义 | Canonical 字段 |
|---|---|
| 市场代码 + 证券代码 | `tsCode` |
| 名称 | `name` |
| 最新价 | `price` |
| 涨跌额 / 涨跌幅 | `change` / `changePercent` |
| 昨收 / 今开 / 高 / 低 | `previousClose` / `open` / `high` / `low` |
| 成交量 / 成交额 | `volume` / `amount`，normalize 单位 |
| 换手率 / 量比 | `turnoverRate` / `volumeRatio` |
| PE / PB / 市值 | `DailyBasic` 子集 |
| 时间 | `capturedAt` / `exchangeTime` |
| 数据源 | `source = "eastmoney"` |

规则：

- 所有百分比字段使用百分点。
- 所有金额字段 normalize 为 CNY。
- Eastmoney 返回占位值、横线、空字符串时视为 missing，不转为 0。
- BJ 标的必须优先由 Eastmoney 处理；失败后返回 item error，不再回退到 SH / SZ provider 路由。

## Fallback 条件

Eastmoney 可以在以下场景被调用：

- TDX 连接失败或缺字段。
- 标的是 BJ。
- TDX 不支持的分钟 K / 分时场景。
- 显式 `refresh_market_quotes({ scope, purpose })` 的 scope 包含对应 `TsCode`，且本地 snapshot 缺失、过期，或 TDX 本轮不可用。

Eastmoney 失败后：

- 实时行情可继续尝试 Tencent / Sina。
- K 线缺失返回 item warning / error。
- 不允许把 stale Eastmoney 响应标成 fresh。

## 验收标准

- BJ quote 请求必须走 Eastmoney 或返回明确 `provider_unavailable`；provider 路由只能基于标准 `TsCode`。
- Eastmoney 缺失字段不能变成 0。
- Eastmoney quote 输出必须包含 `source = "eastmoney"` 和 freshness。
- provider 失败只影响对应 item / batch，不破坏 Quotes 读取接口契约。
