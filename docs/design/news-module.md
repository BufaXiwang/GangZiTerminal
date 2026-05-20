# News 模块 Spec

> News 模块只提供**获取资讯的能力**。任何"分析" / 消费状态 / 调度都属于 Agent 模块。
> 本文档只记录**当前已实现**的功能。

## 一句话定位

**纯数据层**——定时从远端拉资讯入 SQLite，对外提供 list / search / get / fetch_article 同步只读 API。News 模块不知道下游消费者是谁，也不关心他们怎么用。

---

## 1. 责任清单

| 责任 | 实现 |
|---|---|
| **拉取** | NewsNow 多源 + 自定义 RSS（chinanews 等），scheduler 每 N 秒一刷 |
| **入库** | upsert `news_items` 表（id 去重） |
| **基础查询** | `list_news_items` / `get_news_items_by_ids` / `search_news_items`（FTS5 trigram + LIKE 回退） |
| **正文抽取** | `article::fetch_article_remote` + `article_contents` 缓存 |
| **保留期清理** | 30 天前的 news_items 自动 purge（含级联 article_contents 孤儿，并清 agent_news_analysis_state 孤儿） |
| **通知** | 刷新成功后 emit `"news-refreshed"` Tauri Event（不知道谁听） |

**明确不做**：
- ❌ 关键词分类 / Importance 分级
- ❌ 自动识别影响哪些股票
- ❌ 任何分析状态机（pending / processing / consumed / ...）
- ❌ 直接调用任何下游模块

---

## 2. DB schema（news 模块拥有的表）

```sql
-- 资讯主表——只存 news 内容，无任何消费/分析字段
create table news_items (
    id text primary key,
    source text not null,
    published text,
    payload_json text not null,            -- 完整 NewsItem JSON
    created_at text not null,
    updated_at text not null
);
create index idx_news_items_published on news_items(published desc);

-- 正文缓存（按 url）
create table article_contents (
    url text primary key,
    item_id text,
    payload_json text not null,
    fetched_at text not null
);

-- FTS5 全文索引（trigram tokenizer 支持中文 LIKE 加速）
create virtual table news_fts using fts5(
    news_id UNINDEXED, title, summary, source,
    tokenize = 'trigram'
);
-- news_items 写入/修改/删除时触发器同步 news_fts
```

完整 schema 单一来源：`src-tauri/src/infrastructure/db/migrations.rs::SCHEMA_SQL`。

---

## 3. 对外接口

### Rust API（同模块 import 使用）

```rust
// infrastructure/news/repository.rs
pub fn list_news_items(app, limit) -> Result<Vec<NewsItem>, String>;
pub fn save_news_items(app, items) -> Result<usize, String>;
pub fn get_news_items_by_ids(app, ids) -> Result<Vec<NewsItem>, String>;
pub fn search_news_items(app, query, limit) -> Result<Vec<NewsItem>, String>;
pub fn load_article_content(app, url) -> Result<Option<Value>, String>;
pub fn save_article_content(app, item_id, article) -> Result<(), String>;
pub fn purge_old_news(app, cutoff_rfc3339) -> Result<u64, String>;

// pipeline/news/refresh.rs
pub async fn run_news_refresh(app) -> Result<NewsRefreshResult, String>;
```

### Tauri Event

| Event | 何时 emit | Payload |
|---|---|---|
| `news-refreshed` | 一轮拉取完成（成功或部分成功）| `{ fetchedCount, failedCount, firstFailure }` |

下游消费者监听这个事件自行决策——News 模块不知道有谁在听。

### Tauri Command（前端 IPC）

| Command | 用途 |
|---|---|
| `news_commands::*` | 前端 NewsPage 渲染列表 / 搜索 / 看详情 |

---

## 4. 配置（用户可调）

| 配置 | Key | 默认 | 范围 | UI |
|---|---|---|---|---|
| 自动刷新 | `gangzi-terminal.auto-refresh` | true | — | SettingsPage › 资讯 |
| 刷新间隔 | `gangzi-terminal.refresh-interval` | 60s | 15s / 30s / 1m / 5m | SettingsPage › 资讯 |
| 保留期 | `NEWS_RETENTION_DAYS` 常量 | 30 天 | — | 代码常量 |

---

## 5. 文件结构

```
domain/news/
  types.rs               NewsItem / NewsId / ArticleContent
  errors.rs              NewsError
  mod.rs

infrastructure/news/
  newsnow/               NewsNow API client
  rss/                   RSS feed parser
  article/               文章正文抽取
  repository.rs          news_items + article_contents CRUD + 搜索
  mod.rs

pipeline/news/
  refresh.rs             编排：feed pull → save_news_items → emit news-refreshed
  mod.rs

adapters/
  news_commands.rs       Tauri IPC（前端调用）
```

**硬约束**（grep 自检全空）：
- `domain/news` / `infrastructure/news` / `pipeline/news` 任何文件**不 import** `crate::*agent*` / `crate::*account*` / `crate::*quotes*`
- News 模块**不直接调任何下游函数**——一切跨 BC 接口走 Tauri Event 或 Rust 函数被下游 import

---

## 6. 故意不做的设计

| 没做 | 理由 |
|---|---|
| News 分析状态机（pending / processing / ...）| 属于 Agent 范畴——见 `infrastructure/agent/news_analysis_repo.rs` |
| 攒批 / 定时调度（M / N）| 属于 Agent 范畴——见 `pipeline/agent/news_batch.rs` |
| Importance / sentiment 关键词分级 | 准确率低；让 agent 自判 |
| 模块内消费者注册 / 回调 | 全走 Tauri Event；消费者完全可替换 |

---

## 7. 与其他文档的关系

- 模块边界 / 依赖方向 → [architecture.md § 1](../architecture.md)
- News BC 在 4 模块中的定位 → [architecture.md § 2.3](../architecture.md)
- 完整 DB schema 单一来源 → `src-tauri/src/infrastructure/db/migrations.rs::SCHEMA_SQL`
- **Agent 怎么消费 news**（攒批 / 状态机 / news_review）→ [learning-loop.md § News 分析章节](./learning-loop.md)
- 本文档：News 模块自身的实现 spec
