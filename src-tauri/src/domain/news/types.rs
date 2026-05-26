//! News 域纯类型 —— spec `news-module.md §2`。
//!
//! News 只提供"拉取 + 存储 + 查询"；任何分析 / 消费状态由 Agent Runtime 自管。

use serde::{Deserialize, Serialize};

/// `NewsItem` —— spec §2 canonical 字段集。
///
/// 持久化约束：
/// - `id` 是新闻主记录唯一身份；按 spec §2 稳定 ID 规则（SHA-256 of canonical URL
///   / provider item_id / fingerprint）派生
/// - `source` 是 feed / channel 稳定 ID，形如 `namespace:channel`
/// - `payload` 保存 provider 原始信息，便于审计和后续补字段
/// - `url` 必须保存 canonical URL（去 tracking / 排序 query / lowercased host）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewsItem {
    pub id: String,
    pub source: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// canonical URL（spec §2 稳定 ID 派生输入）；保留旧字段名 `link` 作为 alias。
    #[serde(rename = "url", alias = "link", skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// 发布时间（ISO-8601 with timezone）；保留旧 `published` 作为 alias。
    #[serde(
        rename = "publishedAt",
        alias = "published",
        skip_serializing_if = "Option::is_none"
    )]
    pub published: Option<String>,
    /// provider 原始 payload，便于审计 / 后续补字段。
    #[serde(default)]
    pub payload: serde_json::Value,
    /// 首次入库时间（RFC3339）。
    #[serde(default)]
    pub created_at: String,
    /// 最近更新时间（RFC3339）。
    #[serde(default)]
    pub updated_at: String,
}

/// `ArticleContent` —— spec §2。
///
/// `url` 必须为 canonical URL；多条 NewsItem 指向同一 canonical URL 时只保留
/// 一份 `ArticleContent`，`first_news_id` 仅审计。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArticleContent {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_news_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 抽取后的纯文本正文（spec canonical 字段）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// provider 原始 payload（原 paragraphs / images / extraction 等放这里）。
    #[serde(default)]
    pub payload: serde_json::Value,
    pub fetched_at: String,
    /// 抽取失败 / 部分成功 warning code（spec §2 WarningCode）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,

    // ---------- 旧字段：保留为序列化别名，便于现存前端继续读 ----------
    /// @deprecated 改用 payload.source；保留兼容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paragraphs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    #[serde(default)]
    pub extraction: String,
}
