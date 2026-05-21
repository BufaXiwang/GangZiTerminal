# Tencent Quotes Reference

> 本文档是 Quotes 模块 Tencent quote adapter 的渠道契约。模块级领域契约见 [../../quotes-module.md](../../quotes-module.md)。

## 定位

Tencent 是实时报价的低优先级 fallback，用于 TDX / Eastmoney 不可用时补充基础 quote 字段。

它不作为 K 线、复权、基本面或公司事件来源。

## 能力范围

| 数据 | 角色 | 输出 |
|---|---|---|
| 股票 / 指数 / 基金基础实时行情 | fallback | `StockQuote` 子集 |
| 五档盘口 | 可用时补充 | `bid[]` / `ask[]` |
| K 线 / daily_basic / events | 不支持 | 不写入 |

## 获取方式

- 使用 Tencent quote Web 接口。
- adapter 必须设置 timeout、retry 和字段完整性校验。
- 非官方接口变更时返回 `provider_unavailable`，不污染 snapshot。

默认限制：

| 项 | 默认 |
|---|---:|
| timeout | 5s |
| retry | 1 次 |

## Normalize 规则

- Tencent 市场前缀必须映射成标准 `TsCode`。
- 价格、成交量、成交额、涨跌幅必须 normalize 到 shared canonical 单位。
- 缺失盘口时返回 `depth_missing`。
- `source = "tencent"`。

## Fallback 条件

Quotes 只在以下场景调用 Tencent：

- TDX 失败。
- Eastmoney 失败或缺基础 quote。
- 读取路径显式允许 refresh / 后台 refresh 需要兜底。

Tencent 失败后可以继续尝试 Sina。

## 验收标准

- Tencent adapter 不写 K 线、基本面或事件读模型。
- Tencent 缺字段不能覆盖已有更新鲜的 TDX / Eastmoney snapshot。
- Tencent 输出必须标明 `source = "tencent"`。
