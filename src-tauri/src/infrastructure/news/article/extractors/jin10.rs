use crate::infrastructure::news::article::extractors::{extract_from_selectors, matches_any};
use crate::infrastructure::news::article::model::{
    ArticleExtractor, ExtractContext, ExtractedArticle,
};
use scraper::Html;

pub struct Jin10Extractor;

impl ArticleExtractor for Jin10Extractor {
    fn name(&self) -> &'static str {
        "jin10"
    }

    fn matches(&self, context: &ExtractContext<'_>) -> bool {
        matches_any(context, &["jin10", "金十"])
    }

    fn extract(&self, document: &Html, _context: &ExtractContext<'_>) -> ExtractedArticle {
        extract_from_selectors(
            document,
            self.name(),
            &[
                ".jin-flash_b",
                ".article-content",
                ".news-content",
                ".content",
                "[class*='article']",
                "article",
            ],
        )
    }
}
