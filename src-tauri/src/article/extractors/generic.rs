use crate::article::extractors::extract_from_selectors;
use crate::article::model::{ArticleExtractor, ExtractContext, ExtractedArticle};
use scraper::Html;

pub struct GenericExtractor;

impl ArticleExtractor for GenericExtractor {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn matches(&self, _context: &ExtractContext<'_>) -> bool {
        true
    }

    fn extract(&self, document: &Html, _context: &ExtractContext<'_>) -> ExtractedArticle {
        extract_from_selectors(
            document,
            self.name(),
            &[
                "article",
                "main",
                ".article-content",
                ".detail-content",
                ".post-content",
                ".news-content",
                ".content",
                "[class*='article']",
                "[class*='detail']",
            ],
        )
    }
}
