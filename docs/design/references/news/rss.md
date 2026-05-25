# RSS News Reference

> 本文档是 News 模块 RSS adapter 的渠道契约。模块级领域契约见 [../../news-module.md](../../news-module.md)。

## 定位

RSS 是 News 的稳定低成本来源，用于定期获取公开资讯列表。RSS adapter 输出 `ProviderNewsItem`，不做重要性判断、情绪判断或股票影响判断。

## 能力范围

| 数据 | 角色 | 输出 |
|---|---|---|
| RSS item 标题 | 主字段 | `ProviderNewsItem.title` |
| RSS item 链接 | 去重依据 | `ProviderNewsItem.url` |
| 发布时间 | 排序 / freshness | `ProviderNewsItem.publishedAt` |
| 摘要 / description | 可选 | `ProviderNewsItem.summary` |
| 原始 item | 审计 | `payload` |

## 获取方式

- 每个 RSS source 由配置声明：`source_id`、`feed_url`、可选 `display_name` / `enabled`。
- `source_id` 创建后不可变；`feed_url` 和展示名可以修改。
- refresh 按 source 拉取 feed。
- 支持 conditional request 时应使用 ETag / Last-Modified；不支持时按本地 stable ID 去重。
- 单个 RSS source 失败不影响其他 source。

默认限制：

| 项 | 默认 |
|---|---:|
| request timeout | 8s |
| retry | 1 次 |
| max items per feed | 100 |

## Normalize 规则

```ts
type ProviderNewsItem = {
  id: string;
  source: string;
  title: string;
  summary?: string;
  url?: string;
  publishedAt?: string;
  payload: JsonValue;
};
```

规则：

- `source` 使用 `rss:<source_id>`，不能用 URL 当 source。
- `id` 按 News spec 稳定 ID 规则生成。
- URL 必须 canonicalize 后写入。
- RSS HTML summary 可以保留文本摘要；清洗失败时 summary 可为空，但 title 不得为空。
- `publishedAt` 缺失时允许为空；News 查询按 `created_at` 兜底排序。

## Fallback 和失败

- 网络失败：该 source 计入 `NewsFailure(provider="rss", source, code="provider_unavailable", stage="fetch", retryable=true)`。
- 解析失败：该 source 计入 `NewsFailure(provider="rss", source, code="invalid_input", stage="normalize")`。
- 单条 item 缺 title 或无法生成稳定 ID：跳过该 item，计入 `NewsRefreshedPayload.warnings[]` 并累计 `skippedCount`，不让整源失败。

## 验收标准

- 同一 RSS item 重复刷新不会生成重复 `news_items`。
- 单个 RSS source 失败不影响其他 source 入库。
- URL tracking query 和 fragment 不影响稳定 ID。
- RSS adapter 不写分析状态。
