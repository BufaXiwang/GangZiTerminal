# News 模块 Spec

> 本文档是 News 模块的设计契约。模块边界 / 依赖方向以 `docs/architecture.md` 为准。
>
> 所有实现均以本文档的最新设计为准；不保留旧接口、旧表结构或兼容路径作为设计目标。

## 一句话定位

**资讯本地读模型**：后台任务持续从多源资讯 provider 拉取新闻和正文，落入 SQLite；UI 和 Agent 只读取本地 news snapshot / DB。

News 只负责“获取、存储、检索资讯”。新闻分析、影响判断、消费状态、交易决策都属于 Agent。

---

## 1. 责任边界

News 模块负责：

- 多源资讯拉取。
- 资讯去重、入库、更新时间维护。
- 正文抽取与正文缓存。
- 基础查询：列表、按 ID 获取、全文搜索、按股票/关键词过滤。
- 保留期清理。
- 刷新完成事件通知。

News 模块不负责：

- 判断资讯重要性。
- 判断资讯利好 / 利空。
- 自动识别影响哪些股票。
- 管理 pending / processing / consumed 等分析状态。
- 触发 Agent run。
- 调用 Quotes / Account / Agent 代码。

---

## 2. 数据模型

### `news_items`

资讯主表只存原始资讯和基础索引字段，不存分析状态。

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

正文缓存按 URL 去重。

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

全文检索索引由 `news_items` 同步维护。

```sql
create virtual table news_fts using fts5(
    news_id unindexed,
    title,
    summary,
    source,
    tokenize = 'trigram'
);
```

### 可选：`news_mentions`

如果后续需要按股票过滤新闻，可新增“轻量 mention 索引”。这仍然不是影响判断，只是文本命中。

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

## 3. UI Command 目标

### `fetch_news`

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
```

```ts
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

默认行为：

- 只读本地 DB / cache。
- 不在 UI 热路径直接请求远端资讯 provider。
- `includeArticle = true` 时优先读 `article_contents`，缺失则返回 warning；是否 lazy fetch 由单独参数或后台任务决定。

### `refresh_news`

手动触发刷新。它是维护入口，不是常规 UI 读取路径。

```ts
type RefreshNewsRequest = {
  sources?: string[];
  force?: boolean;
};
```

```ts
type RefreshNewsResponse = {
  fetchedCount: number;
  savedCount: number;
  failedCount: number;
  firstFailure?: string;
};
```

---

## 4. Agent 工具目标

Agent 不直接使用多组 news command。建议暴露一个工具：

### `fetch_news`

和 UI 的 `fetch_news` 复用同一套本地 query，但默认输出更精简，避免 token 爆炸。

```ts
type FetchNewsToolInput = {
  query?: string;
  tsCodes?: string[];
  ids?: string[];
  includeArticle?: boolean;
  limit?: number;
};
```

Agent 只拿资讯内容。是否重要、影响哪些持仓、是否需要交易动作，都由 Agent 自己判断，并落到 Agent 自己的 episode / memory / analysis 表中。

---

## 5. Provider 策略

News provider 是 infrastructure 细节，不进入 UI / Agent API。

目标策略：

- 支持多个 provider 并行或顺序拉取。
- 每个 provider 输出统一 `NewsItem` wire-independent domain 类型。
- 入库以稳定 ID 去重。
- 单个 provider 失败不影响其他 provider。
- provider 原始 payload 保存在 `payload_json`，便于审计和后续补字段。

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

---

## 6. 后台刷新策略

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

---

## 7. 保留期策略

默认保留 30 天资讯。

保留期清理目标：

- 删除过期 `news_items`。
- 删除孤儿 `article_contents`。
- 删除孤儿 `news_mentions`。
- 不删除 Agent 自己的 episode / analysis 记录；Agent 若需要长期引用新闻，应保存引用摘要或快照。

---

## 8. 依赖约束

News BC 必须保持独立：

- `domain/news` 不依赖 Tauri / SQLite / HTTP / infrastructure / pipeline / adapters。
- `infrastructure/news` 可以依赖 DB / HTTP provider，但不依赖 pipeline / adapters。
- `pipeline/news` 负责编排 refresh / retention，不依赖 adapters。
- `adapters/news_commands` 只做 IPC DTO 转换。
- News 任一层不 import Agent / Account / Quotes 代码。

跨模块交互：

- UI 通过 Tauri command 读 news。
- Agent 通过 adapter tool 读 news。
- 下游监听 `news-refreshed` 后自行决定是否处理。
- News 不反向调用下游。

---

## 9. 暂不纳入

这些能力不属于 News 模块：

- 新闻重要性评分。
- 利好 / 利空情绪判断。
- 自动生成股票标签或行业标签。
- 持仓影响判断。
- 分析状态机。
- Agent run 调度。

这些属于 Agent 或独立 research pipeline，不属于 News 数据模块。
