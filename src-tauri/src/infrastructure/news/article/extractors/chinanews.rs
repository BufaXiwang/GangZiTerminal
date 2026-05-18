use crate::infrastructure::news::article::extractors::{extract_from_selectors, matches_any};
use crate::infrastructure::news::article::model::{
    ArticleExtractor, ExtractContext, ExtractedArticle,
};
use scraper::Html;

pub struct ChinanewsExtractor;

impl ArticleExtractor for ChinanewsExtractor {
    fn name(&self) -> &'static str {
        "chinanews"
    }

    fn matches(&self, context: &ExtractContext<'_>) -> bool {
        matches_any(context, &["chinanews", "中新网", "中新经纬"])
    }

    fn extract(&self, document: &Html, _context: &ExtractContext<'_>) -> ExtractedArticle {
        extract_from_selectors(
            document,
            self.name(),
            &[
                ".left_zw",
                ".content",
                ".article",
                ".article-content",
                "[class*='article']",
                "article",
            ],
        )
    }
}
