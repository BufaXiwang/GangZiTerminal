//! 资讯抓取共用 helper——HTML 标签剥离、HTML entity 解码。
//!
//! newsnow / rss 两个 fetcher 都需要把"description" 字段里的 HTML 片段还原成纯文本
//! 给 prompt / 前端展示用。

/// 把 HTML 片段简化成纯文本——剥所有 `<...>` 标签，还原常见 entity。
///
/// 故意不引 html5ever 这种重量级 parser——资讯摘要短、容错要求低，简单 state machine
/// 已经够用。trim 掉首尾空白避免段间出现空行。
pub fn strip_html(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;

    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }

    output
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_string()
}
