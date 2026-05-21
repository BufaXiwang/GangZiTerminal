# NewsNow News Reference

> 本文档是 News 模块 NewsNow adapter 的渠道契约。模块级领域契约见 [../../news-module.md](../../news-module.md)。

## 定位

NewsNow 是高频聚合资讯来源，用于补充实时新闻列表。NewsNow adapter 只做拉取、normalize 和去重，不判断重要性或股票影响。

## 能力范围

| 数据 | 角色 | 输出 |
|---|---|---|
| 新闻标题 | 必填 | `ProviderNewsItem.title` |
| 新闻 URL | 去重 / 正文抽取入口 | `ProviderNewsItem.url` |
| 来源名 | source 或 payload 字段 | `source` / `payload` |
| 发布时间 | 可选 | `ProviderNewsItem.publishedAt` |
| 摘要 | 可选 | `ProviderNewsItem.summary` |
| 原始 payload | 审计 | `payload` |

## 获取方式

- 通过配置的 NewsNow endpoint 拉取列表。
- endpoint、鉴权和部署方式属于 adapter 配置。
- 支持按频道 / 分类配置 sources。
- refresh 可以按 source 并行，但必须限制并发。

默认限制：

| 项 | 默认 |
|---|---:|
| request timeout | 8s |
| retry | 1 次 |
| max concurrency | 4 |
| max items per source | 100 |

## Normalize 规则

- `source` 使用 NewsNow source id 或配置映射后的稳定 source 名。
- 如果 NewsNow item 有原始媒体名，保存在 `payload.media`，不替代 `source`。
- `id` 按 News spec 稳定 ID 规则生成。
- URL canonicalization 与 RSS 一致。
- 时间字段必须 normalize 为 ISO-8601；无法解析时为空。

## Fallback 和失败

- NewsNow 整体不可用不影响 RSS refresh。
- 单条 item 字段缺失或无法生成稳定 ID 时跳过该 item，计入 `NewsRefreshedPayload.warnings[]` 并累计 `skippedCount`。
- 解析失败计入 `NewsFailure(provider="newsnow", source?, stage="normalize")`。
- 保存失败计入 `NewsFailure(provider="newsnow", source?, stage="save")`，该批次可 partial success。

## 验收标准

- NewsNow 重复返回同一 URL 不生成重复主记录。
- NewsNow 失败不阻塞 RSS 入库。
- 原始 payload 必须保存，便于后续补字段。
- NewsNow adapter 不写 pending / consumed / analyzed 状态。
