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

/// 默认 source 配置（spec §2：NewsSource 编译期常量；不提供运行时配置入口）。
///
/// 新增 / 删除 / 修改 source 必须改这里并重新部署；`enabled` 字段同样在代码中固化。
/// `feed_url` 为 None 的 source 在 provider 拉取时会立即失败（写入 NewsFailure），
/// 但仍然出现在 `list_news_sources` 中，避免 UI 把"未配置"误认为"无新闻"。
/// NewsNow 公开实例（参考 https://github.com/ourongxing/newsnow）。
/// 自部署后可改环境变量替换，但 spec §2 source 是 compile-time。
const NEWSNOW_BASE: &str = "https://newsnow.busiyi.world/api/s";

/// 默认 NewsNow channel。channel id 经 newsnow.busiyi.world 实测有数据返回。
/// 新增只需在此追加 (source_id, "newsnow", 显示名, endpoint, enabled)。
macro_rules! newsnow_source {
    ($channel:literal, $name:literal) => {
        (
            concat!("newsnow:", $channel),
            "newsnow",
            $name,
            Some(concat!(
                "https://newsnow.busiyi.world/api/s?id=",
                $channel,
                "&latest"
            )),
            true,
        )
    };
}

const DEFAULT_SOURCES: &[(&str, &str, &str, Option<&str>, bool)] = &[
    // (source_id, provider, display_name, feed_url, enabled)
    newsnow_source!("cls-telegraph", "财联社电报"),
    newsnow_source!("jin10", "金十数据"),
    newsnow_source!("zaobao", "联合早报"),
    newsnow_source!("36kr-quick", "36氪快讯"),
    newsnow_source!("gelonghui", "格隆汇"),
    newsnow_source!("cls-depth", "财联社深度"),
    newsnow_source!("wallstreetcn", "华尔街见闻"),
    newsnow_source!("fastbull-news", "法布财经"),
    newsnow_source!("cankaoxiaoxi", "参考消息"),
    newsnow_source!("sputniknewscn", "卫星通讯社"),
];

#[allow(dead_code)] // 留作未来 add-source UI 落地时的 endpoint base 引用
const _: &str = NEWSNOW_BASE;

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
    ///
    /// 同步策略（spec §2 "NewsSource 编译期常量"）：
    /// - DEFAULT_SOURCES 中的每条都做 upsert（强制覆盖 feed_url / display_name / enabled），
    ///   因为 source 是 compile-time 真源，DB 行只是衍生缓存。
    /// - DB 中存在但不在 DEFAULT_SOURCES 的 source 直接删除（清理旧 stub / 已下线 channel）。
    pub fn bootstrap(&self, repo: &NewsRepository<'_>) -> rusqlite::Result<()> {
        let now = Utc::now();
        let default_ids: std::collections::HashSet<&str> =
            DEFAULT_SOURCES.iter().map(|(sid, ..)| *sid).collect();
        for s in repo.list_sources()? {
            if !default_ids.contains(s.source_id.as_str()) {
                repo.delete_source(&s.source_id)?;
            }
        }
        for (sid, provider, display, feed_url, enabled) in DEFAULT_SOURCES {
            repo.upsert_source(sid, provider, Some(display), *enabled, *feed_url, now)?;
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
