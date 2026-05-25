# Article Extractor Reference

> 本文档是 News 模块正文抽取 adapter 的渠道契约。模块级领域契约见 [../../news-module.md](../../news-module.md)。

## 定位

Article extractor 根据 `news_items.url` 获取正文并写入 `article_contents`。它只抽取和缓存正文，不做摘要、情绪、重要性或投资影响判断。

## 输入输出

输入：

```ts
type ArticleExtractRequest = {
  newsId: string;
  url: string;
  force?: boolean;
};
```

输出：

```ts
type ArticleExtractResult = {
  url: string;
  firstNewsId?: string;
  title?: string;
  content?: string;
  payload: JsonValue;
  fetchedAt: OccurredAt;
  warning?: WarningCode;
  error?: ErrorCode;
};
```

## 获取方式

- URL 必须先 canonicalize。
- extractor 使用 HTTP GET 获取 HTML 或文本内容。
- 必须设置 User-Agent、timeout、最大响应体大小。
- Content-Type 不可解析时返回 `article_missing` warning。
- 网络不可用 / 非 2xx / 上游拒绝访问映射为 `provider_unavailable`；请求超时映射为 `tool_timeout`；URL 非法映射为 `invalid_input`。

默认限制：

| 项 | 默认 |
|---|---:|
| request timeout | 10s |
| retry | 1 次 |
| max body size | 2MB |
| max concurrency | 4 |

## 抽取规则

- 优先抽取正文主内容，去除导航、广告、脚本、样式。
- 标题可来自 HTML title、OpenGraph title 或正文 parser。
- `content` 必须是纯文本或轻量 Markdown，不保存原始 HTML 作为正文。
- 原始 HTML 不进入 `article_contents.content`；必要审计信息进入 `payload`，持久化层可存为 `payload_json`。
- 正文为空或过短时返回 `article_missing` warning。

## 缓存和失败

- `article_contents.url` 是 canonical URL 主键。
- 抽取成功后同步更新所有同 canonical URL 新闻的全文搜索读模型。
- 抽取失败也应记录 `fetchedAt` 和失败 payload，避免短时间内反复抓取。
- 抽取失败缓存的 `content` 必须为空；它只用于审计和抑制短期重试，不代表可用正文。
- `includeArticle = true` 的读取路径不触发 extractor；抽取只发生在 refresh / warm / 显式维护路径。
- 读取路径缺正文或只有失败缓存时，`fetch_news` 不返回 `article` 字段，并返回 `article_missing` warning；`article_missing` 是机器可读 warning code，不是事件。

## 验收标准

- 同一 canonical URL 只保存一份正文。
- 正文抽取失败不影响 `news_items` 入库。
- 读取路径缺正文不返回 `article` 字段，不直接请求远端。
- 正文更新后全文搜索能命中文章内容。
