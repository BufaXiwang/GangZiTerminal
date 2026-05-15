use crate::article::extractors::{extract_from_selectors, matches_any};
use crate::article::model::{ArticleExtractor, ExtractContext, ExtractedArticle};
use scraper::Html;

pub struct GelonghuiExtractor;

impl ArticleExtractor for GelonghuiExtractor {
    fn name(&self) -> &'static str {
        "gelonghui"
    }

    fn matches(&self, context: &ExtractContext<'_>) -> bool {
        matches_any(context, &["gelonghui", "格隆汇"])
    }

    fn extract(&self, document: &Html, _context: &ExtractContext<'_>) -> ExtractedArticle {
        extract_from_selectors(
            document,
            self.name(),
            &[
                ".article-content",
                ".news-content",
                ".detail-content",
                ".content",
                "[class*='article']",
                "article",
            ],
        )
    }
}
