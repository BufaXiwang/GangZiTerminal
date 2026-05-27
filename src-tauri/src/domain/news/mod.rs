//! News BC domain — 类型和纯规则。
//!
//! Spec: docs/design/news-module.md
//!
//! 铁律（architecture.md §2 / news-module.md §5 依赖约束）：
//! - domain/news 不依赖 Tauri / SQLite / HTTP / infrastructure / pipeline / adapters。
//! - 不 import 其他 bounded context。

pub mod canonical_url;
pub mod errors;
pub mod events;
pub mod ids;
pub mod source;
pub mod types;

pub use canonical_url::{canonicalize_url, CanonicalUrlError};
pub use errors::{NewsErrorCode, WarmArticlesError};
pub use events::{NewsFailure, NewsRefreshStage, NewsRefreshWarning, NewsRefreshedPayload};
pub use ids::{compute_stable_id, IdInput};
pub use source::{NewsSource, NewsSourceLastError, NewsSourceRef};
pub use types::{
    ArticleContent, ArticleSnippet, FetchNewsItem, FetchNewsRequest, FetchNewsResponse,
    ListNewsSourcesResponse, NewsItem, NewsItemFreshness, ProviderNewsItem, WarmArticlesRequest,
    WarmArticlesResponse, WarmArticlesResult,
};
