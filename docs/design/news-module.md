# News 模块 Spec

> 本文档是 News bounded context 的领域模型契约。模块边界 / 依赖方向以 `docs/design/architecture.md` 为准。
>
> 本文档只描述最新设计目标，作为实现依据。

## 一句话定位

**资讯本地读模型**：后台任务持续从多源资讯 provider 拉取新闻和正文，落入 SQLite；UI 和 Agent 只读取本地 news DB / cache。

News 只负责“获取、存储、检索资讯”。新闻分析、影响判断、消费状态、交易决策都属于 Agent。

---

## 1. 责任边界

News 负责：

- 多源资讯拉取。
- 资讯去重、入库、更新时间维护。
- 正文抽取与正文缓存。
- 基础查询：列表、按 ID 获取、全文搜索、按股票/关键词过滤。
- 保留期清理。
- 刷新完成事件通知。

News 不负责：

- 判断资讯重要性。
- 判断资讯利好 / 利空。
- 自动识别影响哪些股票。
- 管理 pending / processing / consumed 等分析状态。
- 触发 Agent run。
- 调用 Quotes / Account / Agent 代码。

---

## 2. 领域模型

### 核心概念

| 概念 | 含义 | 主键 / 身份 |
|---|---|---|
| `NewsItem` | 一条资讯的标准化内容 | `id` |
| `ArticleContent` | 某条资讯 URL 的正文抽取结果 | `url` |
| `NewsMention` | 新闻文本对某个标的的轻量 mention 命中 | `(news_id, ts_code, mention)` |
| `NewsRefreshResult` | 一轮资讯刷新结果 | 查询派生 |

### 不变量

- `news_items` 只存资讯内容和基础索引字段，不存分析状态。
- `article_contents` 按 URL 去重；正文抽取失败不影响 `news_items` 入库。
- `news_mentions` 只是文本命中索引，不代表影响判断。
- News 不持有 Agent 消费状态。Agent 若需要长期引用新闻，应保存引用摘要或快照。
- provider 原始 payload 保存在 `payload_json`，便于审计和后续补字段。

### `news_items`

```sql
create table news_items (
    id text primary key,
    source text not null,
    title text not null,
    summary text,
    url text,
    published_at text,
    payload_json text not null,
    created_at text not null,
    updated_at text not null
);

create index idx_news_items_published_at on news_items(published_at desc);
create index idx_news_items_source on news_items(source);
```

### `article_contents`

```sql
create table article_contents (
    url text primary key,
    news_id text,
    title text,
    content text,
    payload_json text not null,
    fetched_at text not null
);

create index idx_article_contents_news_id on article_contents(news_id);
```

### `news_fts`

```sql
create virtual table news_fts using fts5(
    news_id unindexed,
    title,
    summary,
    source,
    tokenize = 'trigram'
);
```

### `news_mentions`

如果需要按股票过滤新闻，使用轻量 mention 索引：

```sql
create table news_mentions (
    news_id text not null,
    ts_code text not null,
    mention text not null,
    source text not null,        -- rule / provider
    created_at text not null,
    primary key (news_id, ts_code, mention)
);

create index idx_news_mentions_ts_code on news_mentions(ts_code);
```

---

## 3. 数据流

### 写入流

```text
provider fetch
  -> infrastructure/news provider adapter
  -> normalize to NewsItem
  -> upsert news_items
  -> update news_fts / news_mentions
  -> optional article warm -> article_contents
  -> emit news-refreshed
```

### 读取流

```text
UI / Agent
  -> adapters command/tool DTO
  -> news query facade
  -> news_items / news_fts / article_contents / news_mentions
  -> response with freshness/warnings
```

规则：

- UI / Agent 默认只读本地 DB / cache。
- `includeArticle = true` 优先读 `article_contents`；缺失时返回 warning。
- News 不在读取路径触发 Agent 分析。
- News 不反向调用 Quotes / Account / Agent。

---

## 4. 对外接口

### 前端展示接口

#### `fetch_news`

统一资讯读取入口。UI 不需要知道底层 provider。

```ts
type FetchNewsRequest = {
  ids?: string[];
  query?: string;
  tsCodes?: string[];
  sources?: string[];
  publishedFrom?: string;
  publishedTo?: string;
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
    publishedAt?: string;
    article?: {
      title?: string;
      content: string;
      fetchedAt: string;
    };
    mentions?: Array<{
      tsCode: string;
      mention: string;
    }>;
    freshness?: {
      ageMs?: number;
      articleFetchedAt?: string;
    };
    warnings?: string[];
    errors?: string[];
  }>;
  page: {
    limit: number;
    offset: number;
    hasMore: boolean;
  };
};
```

#### `refresh_news`

手动触发刷新。它是维护入口，不是常规 UI 读取路径。

```ts
type RefreshNewsRequest = {
  sources?: string[];
  force?: boolean;
};

type RefreshNewsResponse = {
  fetchedCount: number;
  savedCount: number;
  failedCount: number;
  firstFailure?: string;
};
```

### Agent 调用方法

Agent 只暴露一个 news 工具：

```ts
type FetchNewsToolInput = {
  query?: string;
  tsCodes?: string[];
  ids?: string[];
  includeArticle?: boolean;
  limit?: number;
};
```

工具名：`fetch_news`

约束：

- 复用 `fetch_news` 的本地 query 能力。
- 默认输出应比 UI DTO 更精简，避免 token 爆炸。
- Agent 只拿资讯内容；重要性、影响、交易动作由 Agent 自己判断。

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

News provider 是 infrastructure 细节，不进入 UI / Agent API。

- 支持多个 provider 并行或顺序拉取。
- 每个 provider 输出统一 `NewsItem` wire-independent domain 类型。
- 入库以稳定 ID 去重。
- 单个 provider 失败不影响其他 provider。
- provider 原始 payload 保存在 `payload_json`。

Provider 输出至少包含：

```ts
type ProviderNewsItem = {
  id: string;
  source: string;
  title: string;
  summary?: string;
  url?: string;
  publishedAt?: string;
  payload: unknown;
};
```

### 后台刷新

资讯刷新由 News 自己的 scheduler 维护。

| 任务 | 频率 | 说明 |
|---|---:|---|
| news refresh | 用户配置，默认 60s | 多源拉取、去重、入库 |
| article warm | 低频 / 按需队列 | 对重点新闻或最近新闻抽正文 |
| retention | 每日 | 清理过期 news / article cache |

刷新完成后 emit 事件：

| Event | Payload |
|---|---|
| `news-refreshed` | `{ fetchedCount, savedCount, failedCount, firstFailure }` |

事件只表示数据变化。News 不关心谁监听，也不直接调用下游模块。

### 保留期

默认保留 30 天资讯。

保留期清理目标：

- 删除过期 `news_items`。
- 删除孤儿 `article_contents`。
- 删除孤儿 `news_mentions`。
- 不删除 Agent 自己的 episode / analysis 记录。

### 依赖约束

- `domain/news` 不依赖 Tauri / SQLite / HTTP / infrastructure / pipeline / adapters。
- `infrastructure/news` 可以依赖 DB / HTTP provider，但不依赖 pipeline / adapters。
- `pipeline/news` 负责编排 refresh / retention，不依赖 adapters。
- `adapters/news_commands` 只做 IPC DTO 转换。
- News 任一层不 import Agent / Account / Quotes 代码。

---

## 6. 验收标准 / 例子

- `fetch_news` 是前端读取 News 的统一入口。
- `fetch_news` 默认只读本地 DB / article cache，不触发远端 provider 拉取。
- `refresh_news` 是显式刷新入口；后台刷新也只进入 News 自己的 refresh use case。
- `fetch_news({ includeArticle: true })` 遇到缺正文时返回 item 级 warning，不让整批失败。
- `news-refreshed` 只表示数据变化，不直接调用 Agent / Quotes / Account。
- News 入库以稳定 ID 去重，同一条新闻不会因为 provider 重复返回而生成多条主记录。
- News 任一层不 import Agent / Account / Quotes 代码。

---

## 7. 模块边界外

这些能力不属于 News 模块：

- 新闻重要性评分。
- 利好 / 利空情绪判断。
- 自动生成股票标签或行业标签。
- 持仓影响判断。
- 分析状态机。
- Agent run 调度。

这些属于 Agent 或独立 research pipeline，不属于 News 数据模块。
