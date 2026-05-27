# News 模块 Spec

> 本文档是 News bounded context 的领域模型契约。模块边界 / 依赖方向以 `docs/design/architecture.md` 为准。
>
> 本文档只描述最新设计目标，作为实现依据。

## 一句话定位

**资讯本地读模型**：后台任务持续从多源资讯 provider 拉取新闻和正文，落入本地读模型；对外读取只访问本地读模型 / cache。

News 只负责“获取、存储、检索资讯”。新闻分析、影响判断、消费状态、交易决策不属于 News。

契约强度：

- `NewsItem`、`ArticleContent`、`fetch_news`、`warm_articles`、`news-refreshed` 是 `Spec-as-source`。
- provider 列表、正文抽取策略、article warm 频率、scheduler 频率是 `Spec-anchored`。
- 重要性、情绪、消费状态不属于 News，不能写入 News 表。

共享类型见 [shared-types.md](shared-types.md)。

---

## 1. 责任边界

News 负责：

- 多源资讯拉取。
- 资讯去重、入库、更新时间维护。
- 正文抽取与正文缓存。
- 基础查询：列表、按 ID 获取、按来源 / 时间过滤、FTS 相关性搜索。
- 查询分页和批量上限控制。
- 刷新完成事件通知。

News 不负责：

- 判断资讯重要性。
- 判断资讯利好 / 利空。
- 自动识别影响哪些股票。
- 产出“相关标的”“受影响标的”或行业标签。
- 管理 pending / processing / consumed 等分析状态。
- 启动下游分析或管理跨模块调度。
- 按产品保留期主动删除历史新闻或正文缓存。
- 调用其他 bounded context 的代码。

---

## 2. 领域模型

### 核心概念

| 概念 | 含义 | 主键 / 身份 |
|---|---|---|
| `NewsItem` | 一条资讯的标准化内容 | `id` |
| `ArticleContent` | 某条资讯 URL 的正文抽取结果 | `url` |
| `NewsSource` | 可用于过滤 / 刷新的 feed 或 channel | `source_id` |
| `NewsRefreshResult` | 一轮资讯刷新结果 | 查询派生 |

### 不变量

- `NewsItem` 只存资讯内容和基础索引字段，不存分析状态。
- `ArticleContent` 按 URL 去重；正文抽取失败不影响 `NewsItem` 入库。
- News 不持有下游消费状态。需要长期引用新闻的消费者应保存引用摘要或快照。
- 对外 / domain 字段名使用 `payload`；持久化层可以使用 `payload_json` 作为列名。两者表示同一份 provider 原始 payload，不得在 DTO 中同时暴露两套字段。
- 稳定 ID 必须由 News provider adapter 生成；同一条新闻重复刷新不能生成多条主记录。
- 正文和全文搜索索引都是 News 读模型；News 不按默认保留期主动删除历史 `NewsItem` 或 `ArticleContent`。

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
- `source` 创建后不可变；展示名、URL、启用状态可以变，但不能通过改名迁移历史新闻。
- `provider` 是 adapter 类型，例如 `newsnow`、`rss`、`article_extractor`，只出现在 refresh failure / warning 和 payload 审计信息中。
- 原始媒体名、站点名或作者信息保存在 `payload.media` / `payload.publisher`，不能替代 `source`。
- `payload` 保存 provider 原始信息，便于审计和后续补字段。
- 查询必须支持按发布时间倒序和 source 过滤；具体索引属于实现细节。
- `url` 必须保存 canonical URL；provider 原始 URL 如需审计，保存到 `payload.originalUrl`。

稳定 ID 规则：

```text
1. 有 URL -> source:url:sha256(canonical_url)
2. 无 URL，provider 提供稳定 item id / guid -> source:item:sha256(provider_item_id)
3. 无 URL 且无稳定 item id -> source:fingerprint:sha256(normalized_title + normalized_published_at? + normalized_summary?)
```

规则：

- URL canonicalization 至少包括：scheme / host 小写、去除 fragment、去除默认端口、路径去除重复斜杠和尾部斜杠差异、query 参数按 key 排序。
- tracking query 必须去除 `utm_*`、`spm`、`from`、`source`、`ref`、`refer`、`share`、`isappinstalled`、`nsukey`；provider reference 可以追加 source-specific tracking key，但不能减少这组默认规则。
- hash 算法固定为 SHA-256，输入为 UTF-8 字符串，输出为 lowercase hex，不截断；`url` / `item` / `fingerprint` 前缀用于避免不同输入类型碰撞。
- `publishedAt` 必须 normalize 为带 timezone 的 ISO-8601；参与 fingerprint 时按 UTC 秒级精度。若某 source 的时间精度不稳定，adapter 必须降到该 source 稳定精度或跳过该时间字段，不能因为 provider 二次返回精度变化生成新 ID。
- fingerprint 不能包含 fetch time / createdAt / batchId 这类刷新时刻字段；如果 title / summary / payload 中没有足够稳定信息，adapter 必须跳过该 item 并记录 refresh warning，不能生成不稳定随机 ID。
- 同一 source 下 ID 冲突时更新 `updated_at` 和 `payload`，不新建记录。
- 不同 source 的同一 URL 暂不强制合并；跨源合并属于后续 research 能力。

### `ArticleContent`

```ts
type ArticleContent = {
  url: string;
  firstNewsId?: string;
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
- 多条 `NewsItem` 指向同一 canonical URL 时只保存一份 `ArticleContent`；`firstNewsId` 仅用于审计，不作为外键或读取路径。
- 正文抽取失败必须保存失败 payload 或 warning，避免同一 URL 在短时间内反复失败重试。
- `content` 缺失的 `ArticleContent` 只表示失败缓存 / 抑制短期重试；读取路径不能把它当作可用正文返回。
- `ArticleContent.warning` 可为任意与正文抓取相关的 `WarningCode`：典型为 `article_missing`，provider 部分失败时可能为 `provider_partial_failure`；不限定为单一值。

### `NewsSource`

`NewsSource` 是当前可用资讯 source 的发现读模型。它只描述 source 配置和健康状态，不承载新闻内容。

```ts
type NewsSource = {
  sourceId: string;
  provider: "newsnow" | "rss" | string;
  displayName?: string;
  enabled: boolean;
  dynamic?: boolean;
  lastRefreshAt?: OccurredAt;
  lastError?: {
    code: ErrorCode;
    message?: string;
    occurredAt: OccurredAt;
  };
};
```

规则：

- `sourceId` 使用 `namespace:channel` 形式，并与 `NewsItem.source` 完全一致。
- **NewsSource 列表是编译期常量**：第一阶段不提供运行时配置入口、不提供管理 UI；新增 / 删除 / 修改 source 必须改代码并重新部署。`enabled` 字段同样在代码中固化。
- RSS source 在编译期固定，`sourceId` 必须 normalize 为 `rss:<channel>`；创建后不可变。
- `list_news_sources()` 返回当前编译期 source 集合及其运行时刷新状态，供 UI / 调用方构造 `sources` 过滤条件。

### 全文搜索读模型

全文搜索是 News 的读能力，不规定具体 FTS 实现。

规则：

- `query` 同时检索 title / summary / article。
- `article` 缺失时只索引 title / summary。
- 更新 `ArticleContent` 后必须同步更新所有 `NewsItem.url == ArticleContent.url` 的搜索读模型。

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
    articleExcerpt?: string;
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

- `fetch_news` **不提供按 ID 列表查询**：调用方无法预知具体 `NewsItem.id`，按 ID 检索不是产品需求。需要定位特定新闻，按 `query` / `sources` / `publishedFrom-publishedTo` 组合检索。
- `query` 是唯一文本查询条件，使用全文搜索读模型做相关性搜索；无正文时仍可命中 title / summary。
- `query` 匹配前必须 trim、折叠连续空白；英文大小写不敏感，中文按全文搜索 tokenizer 规则处理。多词 query 的 AND / OR / phrase 行为由 News FTS 读模型统一定义，不能由调用方或不同 adapter 各自解释。
- `query`、`sources`、时间范围同时出现时按 AND 组合。
- `publishedFrom` / `publishedTo` 是闭区间；`publishedAt` 缺失的新闻不命中发布时间范围过滤。
- 有 `query` 时默认按 FTS relevance 排序，并以 `publishedAt desc, createdAt desc, id asc` 作为稳定 tie-breaker；无 `query` 时按 `publishedAt desc, createdAt desc, id asc` 排序。`publishedAt` 缺失时用 `createdAt` 参与第一排序位。
- `limit` 默认 50，最大 200；`offset` 默认 0；`hasMore` 必须基于同一查询条件计算。
- `includeArticle = true` 时不触发远端抽取；缺正文、正文失败缓存或 `ArticleContent.content` 为空时，不返回 `article` 字段，并必须返回 `article_missing` warning。
- `articleExcerpt` 是面向 Agent / 列表摘要的短正文摘录；当本地存在 `ArticleContent.content` 时必须由 News query facade 生成并返回，默认取清洗后正文前 500 个字符（清洗后空白已折叠为单空格，不保留段落分隔）。`includeArticle = false` 时也可以返回已有 `articleExcerpt`，但不得触发远端抽取。
- News 不生成行业标签、相关标的或影响判断；`query` 只是资讯文本搜索条件。

#### `list_news_sources`

列出当前可用的资讯来源，用于前端 source 过滤和手动 refresh 选择。

```ts
type ListNewsSourcesResponse = {
  items: NewsSource[];
};
```

规则：

- `list_news_sources` 只读本地 source 配置 / refresh 状态，不触发远端请求。
- `fetch_news.sources` 包含未知 source 时返回 `invalid_input`，避免把拼写错误误读为"无新闻"。

> **没有 `refresh_news` 命令**：News 不提供手动刷新 IPC 命令。所有刷新由 §5 的 scheduler 独占触发。如果需要"立即刷新"调试入口，由开发者直接调内部 facade（见下方"内部 Rust API"），不暴露为 Tauri command。

### 内部 Rust API

内部 API 以 query facade 为主，refresh 由 scheduler 独占触发：

```rust
// Public Tauri commands
fetch_news(request) -> FetchNewsResponse;
list_news_sources() -> ListNewsSourcesResponse;
warm_articles(request) -> WarmArticlesResponse;

// Internal facade（不暴露为 Tauri command；scheduler / 调试用）
run_news_refresh(request) -> NewsRefreshedPayload | NewsRefreshError;
save_news_items(items);
save_article_content(article);
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
- `FetchNewsRequest.sources` 表示 feed / channel source ID，不表示 adapter provider 类型；scheduler 在内部按 source watermark 拉取，亦只用 source ID。
- 任何 provider 都不能写重要性、情绪、分析状态或交易影响。
- 单个 provider 失败只进入成功结果的 `NewsRefreshedPayload.failures`，不影响其他 provider 保存成功。

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

News refresh 由 scheduler 独占触发，不暴露为 Tauri command；warm 通过 `warm_articles` 命令暴露给前端和调试。

| 任务 | 频率 | 说明 |
|---|---:|---|
| news refresh | scheduler tick，默认 60s | 多源拉取、去重、入库 |
| article warm | `warm_articles` 命令 / 按需队列 | 对最近 N 条、外部指定 `newsIds`、或未抽取且有 URL 的新闻抽正文 |

#### Refresh 批次字段规则

每一轮 refresh / warm 产生 `NewsRefreshedPayload` 或 `WarmArticlesResult`，字段语义统一：

- `batchId` 是本轮的幂等和审计 ID，必须在 refresh / warm 开始时生成并贯穿 warnings / failures / emitted event；同一轮重试不得生成多个 batchId，不同轮不要求稳定复用。
- `fetchedCount` 表示 provider 返回的原始 item 数量；`skippedCount` 表示 normalize / validate 阶段跳过的 item 数量。
- `savedCount = newIds.length + updatedIds.length`。`savedCount` 只统计 `NewsItem` 主记录新增 / 更新；`ArticleContent` 写入或更新只计入 `articleUpdatedCount`，两者不重叠。
- `newIds` 表示本轮首次插入的 `NewsItem.id`；`updatedIds` 表示已有 `NewsItem` 的 title / summary / url / publishedAt / payload 等主记录字段发生变化。两者互斥。
- `articleUpdatedNewsIds` 表示本轮正文变化影响到的 `NewsItem.id`，包括共享同一 canonical URL 的多条新闻；仅正文变化时 `newIds` / `updatedIds` 可以为空。
- 同一 `NewsItem.id` 可以同时出现在 `articleUpdatedNewsIds` 和 `newIds` / `updatedIds` 中；Runtime 侧入队必须按 `newsId` 去重。
- 单个 provider 失败不影响其他 provider；失败写入 `failures`。
- 单条 item 缺必要字段、ID 不稳定、URL 不可解析等可跳过问题写入 `warnings`，必要时累计到 `skippedCount`；不把单条跳过提升为整源失败。
- scheduler 可以按 source watermark 增量拉取；watermark 是 source-level 状态，不暴露为对外接口。

#### Failure code 规则

| Stage | 条件 | `NewsFailure.code` | `NewsFailure.details.reason` |
|---|---|---|---|
| `fetch` | provider 网络不可用、超时、5xx 或返回不可用 | `provider_unavailable` | 可选：`network` / `timeout` / `http_5xx` |
| `fetch` | provider 明确限流、429 或等价响应 | `rate_limited` | 可选 |
| `normalize` | provider payload 无法解析、字段类型错误、正文 / 标题结构不可识别 | `parse_error` | 可选 |
| `save` | DB 写入或事务失败 | `db_error` | 可选 |
| `article` | 正文抽取器无法提取有效正文、正文解析失败或正文 provider 明确失败 | `article_extract_failed` | **必填**：`network` / `timeout` / `too_short` / `unsupported_content_type` / `http_status` / `parse_error` |

规则：
- `code` 是封闭集合（见 shared-types.md §5），不得为细分原因临时新增 code。
- `details.reason` 用于在 `code` 不变的前提下表达细分原因，方便前端展示和审计。
- `details` 整体仍是 `JsonValue`；除 `reason` 外还可放 `httpStatus` / `urlSample` 等 provider-specific 调试字段。

`NewsRefreshWarning.code` 用于可跳过或部分成功场景：缺正文用 `article_missing`，单条 item 无法稳定生成 ID 或字段不足用 `data_partial` / `provider_partial_failure`，不得把 warning 临时塞进 `ErrorCode`。

正文预热入口：

```ts
type WarmArticlesRequest = {
  newsIds?: string[];
  recentLimit?: number;
  force?: boolean;
};

type WarmArticlesError = {
  code: ErrorCode;
  field?: "newsIds" | "recentLimit";
  message?: string;
};

type WarmArticlesResult = {
  batchId: string;
  requestedCount: number;
  attemptedCount: number;
  articleUpdatedCount: number;
  articleUpdatedNewsIds: string[];
  warnings?: NewsRefreshWarning[];
  failures?: NewsFailure[];
};

type WarmArticlesResponse =
  | {
      ok: true;
      result: WarmArticlesResult;
    }
  | {
      ok: false;
      error: WarmArticlesError;
    };
```

规则：

- `warm_articles` 是维护入口，不是读取路径；`fetch_news(includeArticle = true)` 仍然只读本地 `ArticleContent`。
- `newsIds` 指定时只处理这些新闻；缺失时按最近 `recentLimit` 条、且有 canonical URL 的新闻选择候选。
- `recentLimit` 默认 50，最大 200；`newsIds` 最多 200 个。
- 未找到的 `newsIds` 返回 `ok = false` / `not_found`，不创建 `batchId`；没有 URL 的新闻跳过并进入 warning，不计入 `attemptedCount`。
- `force = false` 时，已有成功正文或近期失败缓存的 URL 不重复抽取；`force = true` 可重新抽取。
- 成功写入或更新 `ArticleContent` 时，`articleUpdatedNewsIds` 必须包含所有共享同一 canonical URL 且受影响的 `NewsItem.id`。
- `articleUpdatedCount > 0` 时必须 emit `news-refreshed`，其中 `savedCount = 0`，`articleUpdatedNewsIds` 表达正文变化影响范围。

刷新完成后 emit 事件：

| Event | Payload |
|---|---|
| `news-refreshed` | `NewsRefreshedPayload`，定义见 [shared-types.md](shared-types.md) |

事件以 `AppEventEnvelope<NewsRefreshedPayload>` 发布；`occurredAt` / `correlationId` 属于 envelope，不在 payload 内重复。

`news-refreshed` 只表示本地读模型发生变化；`savedCount = 0` 且 `articleUpdatedCount = 0` 的 refresh 不发布该事件，失败和 warning 只进入 refresh result / observability。Article warm 如果写入或更新 `ArticleContent`，也属于 News 读模型变化。News 不关心谁监听，也不直接调用下游模块。

### 查询规模限制

News 不定义默认数据保留期，也不主动删除历史 `NewsItem` 或 `ArticleContent`。历史数据清理如果未来需要，必须作为独立产品 / 维护策略另行定义，不能由 News 读取或刷新路径隐式执行。

读取和维护入口必须限制单次处理规模：

- `fetch_news.limit` 默认 50，最大 200；不允许无上限列表读取。
- `warm_articles.recentLimit` 默认 50，最大 200；`newsIds` 最多 200 个。
- Provider refresh 的分页、batch size、timeout 和 retry 写在 provider reference；adapter 不得一次性无界拉取。
- 调用方需要长期回看时，必须通过时间窗口、分页或明确 ID 集合分批读取。

### 依赖约束

- `domain/news` 不依赖 Tauri / SQLite / HTTP / infrastructure / pipeline / adapters。
- `infrastructure/news` 可以依赖 DB / HTTP provider，但不依赖 pipeline / adapters。
- `pipeline/news` 负责编排 refresh / article warm，不依赖 adapters。
- `adapters/news_commands` 只做 IPC DTO 转换。
- News 任一层不 import 其他 bounded context 代码。

---

## 6. 验收标准 / 例子

- `fetch_news` 是读取 News 的统一入口。
- `list_news_sources` 能列出当前编译期 source 集合及刷新状态。
- `fetch_news` 默认只读本地 DB / article cache，不触发远端 provider 拉取。
- 后台 scheduler 调用 News refresh use case 完成定时刷新；不提供手动 `refresh_news` 命令。
- `fetch_news({ query })` 能按 FTS 相关性搜索新闻，但不产出影响判断。
- `fetch_news({ includeArticle: true })` 遇到缺正文时不返回 `article` 字段，并返回 `article_missing` warning，不让整批失败。
- `news-refreshed` 只表示数据变化，不直接调用下游模块；scheduler refresh 和 `warm_articles` 在 `savedCount + articleUpdatedCount > 0` 时发布。
- 正文更新必须通过 `articleUpdatedNewsIds` 表达受影响新闻；`savedCount` 不统计正文变化。
- News 入库以稳定 ID 去重，同一条新闻不会因为 provider 重复返回而生成多条主记录。
- `article` stage failure 顶层 code 统一为 `article_extract_failed`，细分原因放 `details.reason`。
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
