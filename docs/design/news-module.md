# News 模块 Spec

> 本文档是 News bounded context 的领域模型契约。模块边界 / 依赖方向以 `docs/design/architecture.md` 为准。
>
> 本文档只描述最新设计目标，作为实现依据。

## 一句话定位

**资讯本地读模型**：后台任务持续从多源资讯 provider 拉取新闻和正文，落入本地读模型；对外读取只访问本地读模型 / cache。

News 只负责“获取、存储、检索资讯”。新闻分析、影响判断、消费状态、交易决策不属于 News。

契约强度：

- `NewsItem`、`ArticleContent`、`fetch_news`、`refresh_news`、`news-refreshed` 是 `Spec-as-source`。
- provider 列表、正文抽取策略、article warm 频率是 `Spec-anchored`。
- 重要性、情绪、消费状态不属于 News，不能写入 News 表。

共享类型见 [shared-types.md](shared-types.md)。

---

## 1. 责任边界

News 负责：

- 多源资讯拉取。
- 资讯去重、入库、更新时间维护。
- 正文抽取与正文缓存。
- 基础查询：列表、按 ID 获取、按来源 / 时间过滤、FTS 相关性搜索。
- 保留期清理。
- 刷新完成事件通知。

News 不负责：

- 判断资讯重要性。
- 判断资讯利好 / 利空。
- 自动识别影响哪些股票。
- 产出“相关标的”“受影响标的”或行业标签。
- 管理 pending / processing / consumed 等分析状态。
- 启动下游分析或管理跨模块调度。
- 调用其他 bounded context 的代码。

---

## 2. 领域模型

### 核心概念

| 概念 | 含义 | 主键 / 身份 |
|---|---|---|
| `NewsItem` | 一条资讯的标准化内容 | `id` |
| `ArticleContent` | 某条资讯 URL 的正文抽取结果 | `url` |
| `NewsRefreshResult` | 一轮资讯刷新结果 | 查询派生 |

### 不变量

- `NewsItem` 只存资讯内容和基础索引字段，不存分析状态。
- `ArticleContent` 按 URL 去重；正文抽取失败不影响 `NewsItem` 入库。
- News 不持有下游消费状态。需要长期引用新闻的消费者应保存引用摘要或快照。
- 对外 / domain 字段名使用 `payload`；持久化层可以使用 `payload_json` 作为列名。两者表示同一份 provider 原始 payload，不得在 DTO 中同时暴露两套字段。
- 稳定 ID 必须由 News provider adapter 生成；同一条新闻重复刷新不能生成多条主记录。
- 正文和全文搜索索引都是 News 读模型；删除过期新闻时必须清理孤儿读模型。

### `NewsItem`

```ts
type NewsItem = {
  id: string;
  source: string;
  title: string;
  summary?: string;
  url?: string;
  publishedAt?: OccurredAt;
  payload: JsonValue;
  createdAt: OccurredAt;
  updatedAt: OccurredAt;
};
```

持久化契约：

- `id` 是新闻主记录唯一身份。
- `source` 是 feed / channel 的稳定 ID，例如 `newsnow:hot`、`rss:example`。它用于查询过滤、watermark 和去重 namespace。
- `source` 统一使用小写 `namespace:channel` 形式；匹配大小写敏感，provider reference 负责定义各自可用 channel。
- `provider` 是 adapter 类型，例如 `newsnow`、`rss`、`article_extractor`，只出现在 refresh failure / warning 和 payload 审计信息中。
- 原始媒体名、站点名或作者信息保存在 `payload.media` / `payload.publisher`，不能替代 `source`。
- `payload` 保存 provider 原始信息，便于审计和后续补字段。
- 查询必须支持按发布时间倒序和 source 过滤；具体索引属于实现细节。
- `url` 必须保存 canonical URL；provider 原始 URL 如需审计，保存到 `payload.originalUrl`。

稳定 ID 规则：

```text
1. 有 URL -> source:canonical_url_hash
2. 无 URL，provider 提供稳定 item id / guid -> source:item_id
3. 无 URL 且无稳定 item id -> source:normalized_title + normalized_published_at? + normalized_summary? fingerprint
```

规则：

- URL canonicalization 至少包括：scheme / host 小写、去除 fragment、去除默认端口、路径去除重复斜杠和尾部斜杠差异、query 参数按 key 排序。
- tracking query 必须去除 `utm_*`、`spm`、`from`、`source`、`ref`、`refer`、`share`、`isappinstalled`、`nsukey`；provider reference 可以追加 source-specific tracking key，但不能减少这组默认规则。
- `publishedAt` 必须 normalize 为带 timezone 的 ISO-8601；参与 fingerprint 时按 UTC 秒级精度。若某 source 的时间精度不稳定，adapter 必须降到该 source 稳定精度或跳过该时间字段，不能因为 provider 二次返回精度变化生成新 ID。
- fingerprint 不能包含 fetch time / createdAt / batchId 这类刷新时刻字段；如果 title / summary / payload 中没有足够稳定信息，adapter 必须跳过该 item 并记录 refresh warning，不能生成不稳定随机 ID。
- 同一 source 下 ID 冲突时更新 `updated_at` 和 `payload`，不新建记录。
- 不同 source 的同一 URL 暂不强制合并；跨源合并属于后续 research 能力。

### `ArticleContent`

```ts
type ArticleContent = {
  url: string;
  newsId?: string;
  title?: string;
  content?: string;
  payload: JsonValue;
  fetchedAt: OccurredAt;
  warning?: WarningCode;
};
```

规则：

- `ArticleContent.url` 使用 canonical URL。
- `fetch_news(includeArticle = true)` 按 `NewsItem.url` 的 canonical URL 查询 `ArticleContent`；不按 provider 原始 URL 查询。
- 多条 `NewsItem` 指向同一 canonical URL 时，`ArticleContent.newsId` 保存首次关联新闻；其他关联通过 canonical `NewsItem.url` 查询，不另建重复正文。
- 正文抽取失败必须保存失败 payload 或 warning，避免同一 URL 在短时间内反复失败重试。
- `content` 缺失的 `ArticleContent` 只表示失败缓存 / 抑制短期重试；读取路径不能把它当作可用正文返回。

### 全文搜索读模型

全文搜索是 News 的读能力，不规定具体 FTS 实现。

规则：

- `query` 同时检索 title / summary / article。
- `article` 缺失时只索引 title / summary。
- 更新 `ArticleContent` 后必须同步更新对应搜索读模型。

---

## 3. 数据流

### 写入流

```text
provider fetch
  -> infrastructure/news provider adapter
  -> normalize to NewsItem
  -> upsert NewsItem
  -> update search index
  -> optional article warm -> ArticleContent
  -> emit news-refreshed
```

### 读取流

```text
external read request
  -> adapters command DTO
  -> news query facade
  -> NewsItem / search index / ArticleContent
  -> response with freshness/warnings
```

规则：

- 读取路径默认只读本地 DB / cache。
- `includeArticle = true` 优先读 `ArticleContent`；缺失时不返回 `article` 字段，并必须返回 `article_missing` warning。
- News 不在读取路径触发下游分析。
- News 不反向调用其他 bounded context。

---

## 4. 对外接口

### 读取接口

#### `fetch_news`

统一资讯读取入口。调用方不需要知道底层 provider；`source` 作为 feed / channel ID 可以暴露给调用方做过滤。

```ts
type FetchNewsRequest = {
  ids?: string[];
  query?: string;
  sources?: string[];
  publishedFrom?: OccurredAt;
  publishedTo?: OccurredAt;
  includeArticle?: boolean;
  limit?: number;
  offset?: number;
};

type FetchNewsResponse = {
  items: Array<{
    id: string;
    source: string;
    title: string;
    summary?: string;
    url?: string;
    publishedAt?: OccurredAt;
    article?: {
      title?: string;
      content: string;
      fetchedAt?: OccurredAt;
    };
    freshness?: {
      ageMs?: number;
      articleFetchedAt?: OccurredAt;
    };
    warnings?: WarningCode[];
    errors?: ErrorCode[];
  }>;
  errors?: Array<{
    id?: string;
    field?: string;
    code: ErrorCode;
    message?: string;
  }>;
  page: {
    limit: number;
    offset: number;
    hasMore: boolean;
  };
};
```

规则：

- `ids` 查询保持输入顺序；返回的 `items` 只包含找到的新闻，缺失 ID 必须进入响应级 `errors[]`，`code = "not_found"`，不能用 warning 代替。
- `ids` 查询和分页同时出现时，先按 `ids` 过滤并保持输入顺序，再应用 `limit/offset`；未命中的 ID 仍进入 `errors[]`。
- `query` 是唯一文本查询条件，使用全文搜索读模型做相关性搜索；无正文时仍可命中 title / summary。
- `query` 匹配前必须 trim、折叠连续空白；英文大小写不敏感，中文按全文搜索 tokenizer 规则处理。多词 query 的 AND / OR / phrase 行为由 News FTS 读模型统一定义，不能由调用方或不同 adapter 各自解释。
- `query`、`sources`、时间范围同时出现时按 AND 组合。`ids` 出现时先限定 ID 集合，再应用其他过滤和分页。
- `publishedFrom` / `publishedTo` 是闭区间；`publishedAt` 缺失的新闻不命中发布时间范围过滤。
- 有 `query` 时默认按 FTS relevance 排序，并以 `publishedAt desc, createdAt desc, id asc` 作为稳定 tie-breaker；无 `query` 时按 `publishedAt desc, createdAt desc, id asc` 排序。`publishedAt` 缺失时用 `createdAt` 参与第一排序位。
- `limit` 默认 50，最大 200；`offset` 默认 0；`hasMore` 必须基于同一查询条件计算。
- `includeArticle = true` 时不触发远端抽取；缺正文、正文失败缓存或 `ArticleContent.content` 为空时，不返回 `article` 字段，并必须返回 `article_missing` warning。
- News 不生成行业标签、相关标的或影响判断；`query` 只是资讯文本搜索条件。

#### `refresh_news`

手动触发刷新。它是维护入口，不是常规读取路径。

```ts
type RefreshNewsRequest = {
  sources?: string[];
  force?: boolean;
};

type RefreshNewsResponse = NewsRefreshedPayload;
```

规则：

- `batchId` 是本轮 refresh 的幂等和审计 ID。
- `fetchedCount` 表示 provider 返回的原始 item 数量；`skippedCount` 表示 normalize / validate 阶段跳过的 item 数量。
- `savedCount = newIds.length + updatedIds.length`。
- 单个 provider 失败不影响其他 provider；失败写入 `failures`。
- 单条 item 缺必要字段、ID 不稳定、URL 不可解析等可跳过问题写入 `warnings`，必要时累计到 `skippedCount`；不把单条跳过提升为整源失败。
- `force = false` 时 provider adapter 可以按 source watermark 增量拉取。

### 内部 Rust API

内部 API 以 query / refresh facade 为主：

```rust
fetch_news(request) -> FetchNewsResponse;
refresh_news(request) -> RefreshNewsResponse;
save_news_items(items);
save_article_content(article);
purge_old_news(cutoff);
```

---

## 5. 模块独有功能

### Provider 策略

News provider 是 infrastructure 细节，不进入对外读取 API。

Provider reference：

- [RSS](references/news/rss.md)
- [NewsNow](references/news/newsnow.md)
- [Article Extractor](references/news/article-extractor.md)

- 支持多个 provider 并行或顺序拉取。
- 每个 provider 输出统一 `NewsItem` wire-independent domain 类型。
- 入库以稳定 ID 去重。
- 单个 provider 失败不影响其他 provider。
- provider 原始 payload 通过 domain 字段 `payload` 表达；持久化层可存为 `payload_json`。
- 当前 NewsNow / RSS / Article Extractor 是默认 provider 策略；后续可以继续扩展资讯渠道，但新增 provider 必须先补 reference 文档，并 normalize 到 `NewsItem` / `ArticleContent` canonical model。

默认 provider 使用策略：

| 数据 | 主路径 | fallback / 补充 |
|---|---|---|
| 资讯列表 | NewsNow | RSS |
| 稳定低频来源 | RSS | 无 |
| 正文 | Article Extractor | 缺失时返回 `article_missing` warning |

规则：

- 本 spec 定义 News canonical model、去重和读取契约。
- 具体 source URL、字段映射、timeout、retry、正文抽取算法写在 provider reference。
- `FetchNewsRequest.sources` 和 `RefreshNewsRequest.sources` 都表示 feed / channel source ID，不表示 adapter provider 类型。
- 任何 provider 都不能写重要性、情绪、分析状态或交易影响。
- 单个 provider 失败只进入 `RefreshNewsResponse.failures`，不影响其他 provider 保存成功。

Provider 输出至少包含：

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

### 后台刷新

News 提供 refresh / retention use case；触发节奏由模块外运行时配置。

| 任务 | 频率 | 说明 |
|---|---:|---|
| news refresh | 外部调度，默认 60s | 多源拉取、去重、入库 |
| article warm | 外部调度低频 / 按需队列 | 对最近 N 条、外部指定 `newsIds`、或未抽取且有 URL 的新闻抽正文 |
| retention | 外部调度每日 | 清理过期 news / article cache |

刷新完成后 emit 事件：

| Event | Payload |
|---|---|
| `news-refreshed` | `NewsRefreshedPayload`，定义见 [shared-types.md](shared-types.md) |

事件以 `AppEventEnvelope<NewsRefreshedPayload>` 发布；`occurredAt` / `correlationId` 属于 envelope，不在 payload 内重复。

`news-refreshed` 只表示本地读模型发生变化；`savedCount = 0` 且 `articleUpdatedCount = 0` 的 refresh 不发布该事件，失败和 warning 只进入 refresh result / observability。Article warm 如果写入或更新 `ArticleContent`，也属于 News 读模型变化。News 不关心谁监听，也不直接调用下游模块。

### 保留期

默认保留 30 天资讯。

保留期清理目标：

- 删除过期 `NewsItem`。
- 删除孤儿 `ArticleContent`。
- 只清理 News 拥有的表，不清理其他模块数据。

### 依赖约束

- `domain/news` 不依赖 Tauri / SQLite / HTTP / infrastructure / pipeline / adapters。
- `infrastructure/news` 可以依赖 DB / HTTP provider，但不依赖 pipeline / adapters。
- `pipeline/news` 负责编排 refresh / retention，不依赖 adapters。
- `adapters/news_commands` 只做 IPC DTO 转换。
- News 任一层不 import 其他 bounded context 代码。

---

## 6. 验收标准 / 例子

- `fetch_news` 是读取 News 的统一入口。
- `fetch_news` 默认只读本地 DB / article cache，不触发远端 provider 拉取。
- `refresh_news` 是显式刷新入口；后台刷新调用 News refresh use case。
- `fetch_news({ query })` 能按 FTS 相关性搜索新闻，但不产出影响判断。
- `fetch_news({ includeArticle: true })` 遇到缺正文时不返回 `article` 字段，并返回 `article_missing` warning，不让整批失败。
- `news-refreshed` 只表示数据变化，不直接调用下游模块。
- News 入库以稳定 ID 去重，同一条新闻不会因为 provider 重复返回而生成多条主记录。
- News 任一层不 import 其他 bounded context 代码。

---

## 7. 模块边界外

这些能力不属于 News 模块：

- 新闻重要性评分。
- 利好 / 利空情绪判断。
- 自动生成股票标签或行业标签。
- 相关标的 / 受影响标的判断。
- 持仓影响判断。
- 分析状态机。
- 下游分析调度。

这些属于下游决策或独立 research pipeline，不属于 News 数据模块。
