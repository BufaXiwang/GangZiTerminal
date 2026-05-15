pub struct ExtractContext<'a> {
    pub url: &'a str,
    pub source: Option<&'a str>,
    pub fallback_title: Option<&'a str>,
    pub fallback_summary: Option<&'a str>,
}

pub struct ExtractedArticle {
    pub paragraphs: Vec<String>,
    pub extractor: String,
}

impl ExtractedArticle {
    pub fn empty(extractor: &str) -> Self {
        Self {
            paragraphs: Vec::new(),
            extractor: extractor.to_string(),
        }
    }

    pub fn new(extractor: &str, paragraphs: Vec<String>) -> Self {
        Self {
            paragraphs,
            extractor: extractor.to_string(),
        }
    }
}

pub trait ArticleExtractor {
    fn name(&self) -> &'static str;
    fn matches(&self, context: &ExtractContext<'_>) -> bool;
    fn extract(&self, document: &scraper::Html, context: &ExtractContext<'_>) -> ExtractedArticle;
}
