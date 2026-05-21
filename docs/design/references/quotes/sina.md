# Sina Quotes Reference

> 本文档是 Quotes 模块 Sina quote adapter 的渠道契约。模块级领域契约见 [../../quotes-module.md](../../quotes-module.md)。

## 定位

Sina 是实时报价最后 fallback，只用于基础展示，不能作为交易成交模拟的可靠盘口来源。

## 能力范围

| 数据 | 角色 | 输出 |
|---|---|---|
| 股票 / 指数基础实时行情 | 最后 fallback | `StockQuote` 子集 |
| 五档盘口 | 不保证 | 通常返回 `depth_missing` |
| K 线 / daily_basic / events | 不支持 | 不写入 |

## 获取方式

- 使用 Sina quote Web 接口。
- adapter 必须设置短 timeout，避免拖慢 refresh。
- 非官方接口变更时返回 provider failure，不污染 snapshot。

默认限制：

| 项 | 默认 |
|---|---:|
| timeout | 3s |
| retry | 0-1 次 |

## Normalize 规则

- Sina 市场前缀必须映射成标准 `TsCode`。
- 价格、成交量、成交额必须 normalize。
- 缺盘口时返回 `depth_missing`。
- `source = "sina"`。

## 使用限制

- Sina quote 可用于列表展示 fallback。
- Account 即时成交不得依赖缺盘口的 Sina quote。
- Sina 不覆盖已有更新鲜且字段更完整的 snapshot。

## 验收标准

- Sina adapter 输出必须标明 `source = "sina"`。
- 缺盘口时 Account market order 必须被拒绝或等待更高质量 quote。
- Sina 失败不影响已存在本地 snapshot 的读取。
