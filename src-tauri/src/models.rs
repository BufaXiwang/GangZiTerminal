use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct NewsItem {
    pub id: String,
    pub title: String,
    pub link: Option<String>,
    pub source: String,
    pub published: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArticleContent {
    pub url: String,
    pub title: String,
    pub source: Option<String>,
    pub published: Option<String>,
    pub author: Option<String>,
    pub paragraphs: Vec<String>,
    pub images: Vec<String>,
    pub fetched_at: String,
    pub extraction: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseInfo {
    pub path: String,
    pub schema_version: i64,
}
