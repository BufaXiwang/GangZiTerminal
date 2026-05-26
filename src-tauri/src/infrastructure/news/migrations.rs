//! News schema migrations。
//!
//! Spec: docs/design/news-module.md §2 / §5
//!
//! 表前缀 `news_*`（AGENTS.md 分区约束）。
//!
//! 真源表：
//! - `news_items`            — `NewsItem` 主记录（spec §2）
//! - `article_contents`      — `ArticleContent`，按 canonical URL 主键（spec §2）
//! - `news_sources`          — `NewsSource` 配置 + 健康状态（spec §2）
//!
//! 读模型表：
//! - `news_search_fts`       — FTS5 虚拟表，覆盖 title / summary / article（spec §2 全文搜索）
//!
//! 列名约定：领域字段 `payload` 在持久化层使用列名 `payload_json`（spec §2 不变量）。

use rusqlite_migration::M;

/// 返回 News BC 的所有迁移，按版本顺序排列。
pub fn migrations() -> Vec<M<'static>> {
    vec![M::up(MIGRATION_001_INITIAL)]
}

const MIGRATION_001_INITIAL: &str = r#"
-- news_items: NewsItem 主记录
CREATE TABLE news_items (
    id              TEXT PRIMARY KEY,
    source          TEXT NOT NULL,
    title           TEXT NOT NULL,
    summary         TEXT,
    url             TEXT,
    published_at    TEXT,                              -- ISO-8601 UTC
    payload_json    TEXT NOT NULL,                     -- spec §2 不变量：领域字段 payload
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE INDEX idx_news_items_published_at
    ON news_items (published_at DESC, created_at DESC, id);

CREATE INDEX idx_news_items_source_published_at
    ON news_items (source, published_at DESC, created_at DESC, id);

CREATE INDEX idx_news_items_url
    ON news_items (url) WHERE url IS NOT NULL;

-- article_contents: ArticleContent，按 canonical URL 去重
CREATE TABLE article_contents (
    url             TEXT PRIMARY KEY,
    first_news_id   TEXT,
    title           TEXT,
    content         TEXT,
    payload_json    TEXT NOT NULL,
    fetched_at      TEXT NOT NULL,
    warning         TEXT
);

-- news_sources: 资讯来源配置 + 健康状态
CREATE TABLE news_sources (
    source_id           TEXT PRIMARY KEY,
    provider            TEXT NOT NULL,
    display_name        TEXT,
    enabled             INTEGER NOT NULL DEFAULT 1,
    dynamic             INTEGER,
    feed_url            TEXT,                          -- RSS feed URL；NewsNow channel 可空
    config_json         TEXT,                          -- provider-specific 配置（备用）
    last_refresh_at     TEXT,
    last_error_code     TEXT,
    last_error_message  TEXT,
    last_error_at       TEXT,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL
);

-- news_search_fts: FTS5 全文搜索读模型（spec §2 全文搜索）
-- title / summary / article 三列；article 列在 ArticleContent 更新后回写。
CREATE VIRTUAL TABLE news_search_fts USING fts5(
    news_id UNINDEXED,
    title,
    summary,
    article,
    tokenize = 'unicode61'
);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::db::run_migrations;
    use rusqlite::Connection;

    #[test]
    fn applies_initial_migration() {
        let mut conn = Connection::open_in_memory().unwrap();
        run_migrations(&mut conn, migrations()).unwrap();
        // smoke insert
        conn.execute(
            "INSERT INTO news_items (id, source, title, payload_json, created_at, updated_at)
             VALUES ('rss:x:url:abc', 'rss:x', 'hello', '{}', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO news_search_fts (news_id, title, summary, article) VALUES ('rss:x:url:abc', 'hello', '', '')",
            [],
        )
        .unwrap();
    }
}
