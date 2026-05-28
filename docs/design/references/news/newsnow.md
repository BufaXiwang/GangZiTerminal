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
- 上游：[ourongxing/newsnow](https://github.com/ourongxing/newsnow)。公开实例 `https://newsnow.busiyi.world/api/s?id=<channel>&latest`。
- endpoint、鉴权和部署方式属于 adapter 配置。Spec §2 NewsSource 是 compile-time 常量；当前默认 source 直接走公开实例，要换自部署改 registry 即可。
- 支持按频道 / 分类配置 sources。
- refresh 可以按 source 并行，但必须限制并发。

### 默认 channel（compile-time）

| source_id | display_name | channel |
|---|---|---|
| `newsnow:cls-telegraph` | 财联社电报 | `cls-telegraph` |
| `newsnow:wallstreetcn-quick` | 华尔街见闻快讯 | `wallstreetcn-quick` |
| `newsnow:jin10` | 金十数据 | `jin10` |

支持的全量 channel 见 NewsNow upstream `getters.ts`，覆盖财联社 / 华尔街见闻 / 金十 / 36 氪快讯 / 格隆汇 / 知乎 / V2EX / 微博 / 抖音 等。新增默认 source 须改 `infrastructure/news/registry.rs::DEFAULT_SOURCES` 并同步更新这张表。

默认限制：

| 项 | 默认 |
|---|---:|
| request timeout | 8s |
| retry | 1 次 |
| max concurrency | 4 |
| max items per source | 100 |

## Normalize 规则

- `source` 使用 `newsnow:<channel>` 或配置映射后的稳定 `namespace:channel` source 名。
- 如果 NewsNow item 有原始媒体名，保存在 `payload.media`，不替代 `source`。
- `id` 按 News spec 稳定 ID 规则生成。
- URL canonicalization 与 RSS 一致。
- 时间字段必须 normalize 为 ISO-8601；无法解析时为空。

## Fallback 和失败

- NewsNow 整体不可用不影响 RSS refresh。
- 单条 item 字段缺失或无法生成稳定 ID 时跳过该 item，计入 `NewsRefreshedPayload.warnings[]` 并累计 `skippedCount`。
- 解析失败计入 `NewsFailure(provider="newsnow", source?, code="invalid_input", stage="normalize")`。
- 保存失败计入 `NewsFailure(provider="newsnow", source?, code="db_error", stage="save")`，该批次可 partial success。

## 验收标准

- NewsNow 重复返回同一 URL 不生成重复主记录。
- NewsNow 失败不阻塞 RSS 入库。
- 原始 payload 必须保存，便于后续补字段。
- NewsNow adapter 不写 pending / consumed / analyzed 状态。
