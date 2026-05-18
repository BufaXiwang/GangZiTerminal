use crate::infrastructure::news::article::extractors::{extract_from_selectors, matches_any};
use crate::infrastructure::news::article::model::{
    ArticleExtractor, ExtractContext, ExtractedArticle,
};
use scraper::Html;

pub struct WallstreetcnExtractor;

impl ArticleExtractor for WallstreetcnExtractor {
    fn name(&self) -> &'static str {
        "wallstreetcn"
    }

    fn matches(&self, context: &ExtractContext<'_>) -> bool {
        matches_any(context, &["wallstreetcn", "华尔街见闻"])
    }

    fn extract(&self, document: &Html, _context: &ExtractContext<'_>) -> ExtractedArticle {
        extract_from_selectors(
            document,
            self.name(),
            &[
                "article",
                ".article-content",
                ".rich-text",
                ".node-article-content",
                "[class*='article']",
                ".content",
            ],
        )
    }
}
