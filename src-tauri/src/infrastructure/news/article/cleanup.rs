use scraper::{ElementRef, Selector};

const NOISE_KEYWORDS: &[&str] = &[
    "打开APP",
    "下载APP",
    "APP阅读",
    "微信扫码",
    "扫码分享",
    "扫一扫",
    "分享至",
    "发表评论",
    "我要反馈",
    "免责声明",
    "风险提示及免责声明",
    "责任编辑",
    "举报",
    "返回顶部",
    "网站地图",
    "联系我们",
    "Copyright",
    "版权所有",
];

pub fn parse_selector(selector: &str) -> Option<Selector> {
    Selector::parse(selector).ok()
}

pub fn normalize_text(input: &str) -> Option<String> {
    let text = input
        .replace('\u{00a0}', " ")
        .replace('\u{3000}', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

pub fn is_content_text(text: &str) -> bool {
    let char_count = text.chars().count();
    char_count >= 10
        && !NOISE_KEYWORDS.iter().any(|keyword| text.contains(keyword))
        && !looks_like_nav(text)
}

pub fn dedupe(items: Vec<String>) -> Vec<String> {
    let mut output = Vec::new();
    for item in items {
        if !output.iter().any(|existing| existing == &item) {
            output.push(item);
        }
    }
    output.into_iter().take(80).collect()
}

pub fn has_quality(paragraphs: &[String]) -> bool {
    let total = paragraphs
        .iter()
        .map(|text| text.chars().count())
        .sum::<usize>();
    total >= 80 || (paragraphs.len() >= 2 && total >= 50)
}

pub fn collect_paragraphs(element: ElementRef<'_>) -> Vec<String> {
    if let Some(selector) = parse_selector("p, h2, h3") {
        let paragraphs = element
            .select(&selector)
            .filter_map(|node| normalize_text(&node.text().collect::<Vec<_>>().join("")))
            .filter(|text| is_content_text(text))
            .collect::<Vec<_>>();
        if has_quality(&paragraphs) {
            return dedupe(paragraphs);
        }
    }

    let text = element.text().collect::<Vec<_>>().join("\n");
    dedupe(
        text.lines()
            .filter_map(normalize_text)
            .filter(|text| is_content_text(text))
            .collect(),
    )
}

fn looks_like_nav(text: &str) -> bool {
    let short = text.chars().count() <= 20;
    short
        && matches!(
            text,
            "首页"
                | "电报"
                | "话题"
                | "盯盘"
                | "VIP"
                | "FM"
                | "全部"
                | "加红"
                | "公司"
                | "看盘"
                | "港美股"
                | "基金"
                | "提醒"
                | "帮助"
        )
}
