//! 资讯正文摘要 —— spec `news-module.md §4 articleExcerpt`：
//! 「摘要由 query facade 生成；上限 500 字符」。
//!
//! 抽公共 helper，避免 adapters/news_canonical 和 agent_tools/news 各持一份。

/// spec §4 摘要字符数上限。
pub const ARTICLE_EXCERPT_MAX_CHARS: usize = 500;

/// 把正文清洗为单行摘要并按字符数截取。
/// - 去多余空白行
/// - 按 char count 截取（不是 byte，避免拦腰截断中文）
/// - 超长时尾部加 `…` 提示
pub fn clean_excerpt(content: &str, max: usize) -> String {
    let trimmed = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if trimmed.chars().count() <= max {
        trimmed
    } else {
        let cut: String = trimmed.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_at_max_chars_with_ellipsis() {
        let s = "a".repeat(600);
        let out = clean_excerpt(&s, ARTICLE_EXCERPT_MAX_CHARS);
        assert_eq!(out.chars().count(), ARTICLE_EXCERPT_MAX_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn keeps_short_content_unchanged() {
        let out = clean_excerpt("hello", ARTICLE_EXCERPT_MAX_CHARS);
        assert_eq!(out, "hello");
    }

    #[test]
    fn collapses_blank_lines_and_trims() {
        let out = clean_excerpt(
            "  line1  \n\n   line2\n  \n line3",
            ARTICLE_EXCERPT_MAX_CHARS,
        );
        assert_eq!(out, "line1 line2 line3");
    }

    #[test]
    fn truncates_by_char_not_byte_for_chinese() {
        let chinese = "你好世界".repeat(200); // 800 个汉字
        let out = clean_excerpt(&chinese, ARTICLE_EXCERPT_MAX_CHARS);
        // 不应拦腰截断；末尾加 …
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), ARTICLE_EXCERPT_MAX_CHARS + 1);
    }
}
