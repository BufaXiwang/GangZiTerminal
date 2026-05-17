mod chinanews;
mod cls;
mod gelonghui;
mod generic;
mod jin10;
mod wallstreetcn;
mod xueqiu;

use crate::infrastructure::news::article::cleanup::{collect_paragraphs, has_quality, parse_selector};
use crate::infrastructure::news::article::metadata::{extract_description, extract_embedded_paragraphs};
use crate::infrastructure::news::article::model::{ArticleExtractor, ExtractContext, ExtractedArticle};
use scraper::Html;

pub fn extract_article(document: &Html, context: &ExtractContext<'_>) -> ExtractedArticle {
    let extractors: Vec<Box<dyn ArticleExtractor>> = vec![
        Box::new(wallstreetcn::WallstreetcnExtractor),
        Box::new(cls::ClsExtractor),
        Box::new(jin10::Jin10Extractor),
        Box::new(chinanews::ChinanewsExtractor),
        Box::new(gelonghui::GelonghuiExtractor),
        Box::new(xueqiu::XueqiuExtractor),
    ];

    for extractor in extractors {
        if !extractor.matches(context) {
            continue;
        }
        let extracted = extractor.extract(document, context);
        if has_quality(&extracted.paragraphs) {
            return extracted;
        }
    }

    let generic = generic::GenericExtractor;
    let extracted = generic.extract(document, context);
    if has_quality(&extracted.paragraphs) {
        return extracted;
    }

    let embedded = extract_embedded_paragraphs(document, context);
    if has_quality(&embedded) {
        return ExtractedArticle::new("embedded-json", embedded);
    }

    if let Some(description) = extract_description(document).or_else(|| {
        context
            .fallback_summary
            .filter(|summary| summary.chars().count() >= 12)
            .map(str::to_string)
            .or_else(|| {
                context
                    .fallback_title
                    .filter(|title| title.chars().count() >= 8)
                    .map(str::to_string)
            })
    }) {
        return ExtractedArticle::new("metadata", vec![description]);
    }

    ExtractedArticle::empty("none")
}

pub fn matches_any(context: &ExtractContext<'_>, needles: &[&str]) -> bool {
    let text = format!(
        "{} {}",
        context.url.to_lowercase(),
        context.source.unwrap_or_default().to_lowercase()
    );
    needles.iter().any(|needle| text.contains(needle))
}

pub fn extract_from_selectors(
    document: &Html,
    extractor: &str,
    selectors: &[&str],
) -> ExtractedArticle {
    let mut best = Vec::new();
    let mut best_score = 0usize;

    for selector in selectors {
        let Some(selector) = parse_selector(selector) else {
            continue;
        };
        for element in document.select(&selector) {
            let paragraphs = collect_paragraphs(element);
            let score = score_paragraphs(&paragraphs);
            if score > best_score {
                best_score = score;
                best = paragraphs;
            }
            if has_quality(&best) {
                return ExtractedArticle::new(extractor, best);
            }
        }
    }

    ExtractedArticle::new(extractor, best)
}

fn score_paragraphs(paragraphs: &[String]) -> usize {
    let total = paragraphs
        .iter()
        .map(|text| text.chars().count())
        .sum::<usize>();
    total + paragraphs.len() * 16
}
