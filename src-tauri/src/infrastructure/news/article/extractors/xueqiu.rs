use crate::infrastructure::news::article::extractors::{extract_from_selectors, matches_any};
use crate::infrastructure::news::article::model::{
    ArticleExtractor, ExtractContext, ExtractedArticle,
};
use scraper::Html;

pub struct XueqiuExtractor;

impl ArticleExtractor for XueqiuExtractor {
    fn name(&self) -> &'static str {
        "xueqiu"
    }

    fn matches(&self, context: &ExtractContext<'_>) -> bool {
        matches_any(context, &["xueqiu", "雪球"])
    }

    fn extract(&self, document: &Html, _context: &ExtractContext<'_>) -> ExtractedArticle {
        extract_from_selectors(
            document,
            self.name(),
            &[
                ".article__bd",
                ".status-content",
                ".article-content",
                ".content",
                "[class*='article']",
                "article",
            ],
        )
    }
}
