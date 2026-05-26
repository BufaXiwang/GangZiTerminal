//! Source registry — 启动时加载默认 source 配置 + 提供 RSS feed_url 等运行时元信息。
//!
//! Spec: docs/design/news-module.md §2 (NewsSource)；references/news/rss.md
//!
//! 设计：
//! - 启动时把内置默认 source upsert 到 `news_sources`；用户后续可通过另外的 IPC（暂不在 News 范围）
//!   修改 enabled / feed_url。
//! - registry 在内存中维护从 source_id 到 `NewsSourceRef` 的映射，供 provider 选择使用。

use crate::domain::news::source::NewsSourceRef;
use crate::infrastructure::news::NewsRepository;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::RwLock;

/// 默认 source 配置。
///
/// 当前阶段全部 disabled，避免无网络环境下后台 refresh 反复失败。
/// 后续在 News 模块外（系统设置 BC 或 onboarding）落地 enable 入口。
const DEFAULT_SOURCES: &[(&str, &str, &str, Option<&str>, bool)] = &[
    // (source_id, provider, display_name, feed_url, enabled)
    (
        "rss:sample",
        "rss",
        "Sample RSS feed",
        Some("https://example.com/feed.xml"),
        false,
    ),
    (
        "newsnow:hot",
        "newsnow",
        "NewsNow hot channel",
        None,
        false,
    ),
];

pub struct SourceRegistry {
    by_id: RwLock<HashMap<String, NewsSourceRef>>,
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self {
            by_id: RwLock::new(HashMap::new()),
        }
    }

    /// 启动时把默认 source 同步到 DB；并把 DB 中已知 source 加载进 registry。
    pub fn bootstrap(&self, repo: &NewsRepository<'_>) -> rusqlite::Result<()> {
        let now = Utc::now();
        for (sid, provider, display, feed_url, enabled) in DEFAULT_SOURCES {
            // 不要覆盖既有 enabled / feed_url，只在不存在时插入
            if repo.get_source(sid)?.is_none() {
                repo.upsert_source(sid, provider, Some(display), *enabled, *feed_url, now)?;
            }
        }
        let mut map = self.by_id.write().expect("registry poisoned");
        map.clear();
        for s in repo.list_sources()? {
            // feed_url 不在 NewsSource DTO 中（spec §2 不暴露），需要单独读
            let feed_url = repo.get_feed_url(&s.source_id)?;
            map.insert(
                s.source_id.clone(),
                NewsSourceRef {
                    source_id: s.source_id,
                    provider: s.provider,
                    feed_url,
                    display_name: s.display_name,
                    enabled: s.enabled,
                },
            );
        }
        Ok(())
    }

    pub fn list(&self) -> Vec<NewsSourceRef> {
        self.by_id
            .read()
            .expect("registry poisoned")
            .values()
            .cloned()
            .collect()
    }

    pub fn get(&self, source_id: &str) -> Option<NewsSourceRef> {
        self.by_id
            .read()
            .expect("registry poisoned")
            .get(source_id)
            .cloned()
    }

    pub fn contains(&self, source_id: &str) -> bool {
        self.by_id
            .read()
            .expect("registry poisoned")
            .contains_key(source_id)
    }

    /// 列出所有 enabled 的 source。
    pub fn enabled(&self) -> Vec<NewsSourceRef> {
        self.list().into_iter().filter(|s| s.enabled).collect()
    }
}

impl Default for SourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// repository 需要 get_feed_url 方法；在 repository 上补一个查询。
impl NewsRepository<'_> {
    pub fn get_feed_url(&self, source_id: &str) -> rusqlite::Result<Option<String>> {
        use rusqlite::OptionalExtension;
        self.db_for_test_only().with(|c| {
            c.query_row(
                "SELECT feed_url FROM news_sources WHERE source_id = ?1",
                [source_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|x| x.flatten())
        })
    }
}

// 为了避免在 registry 中暴露 repository 内部，提供 trait 形式的访问。
// 这里直接在 repository 文件加 db 引用 getter 太啰嗦——我们用另一种方式：
// 把 `db` 字段改为 pub(crate)，但目前签名是 `&'a AppDb` 私有字段。
// 简化做法：让 NewsRepository 暴露一个 pub(crate) fn 返回 &AppDb。
//
// (该 impl 在 repository.rs 中补，避免 visibility 问题。)
