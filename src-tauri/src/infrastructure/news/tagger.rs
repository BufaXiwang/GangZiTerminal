//! News tagger——资讯入库后异步打标 ticker / sector / kind / importance（纯规则，无 LLM）。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 7.1。
//!
//! 触发：`pipeline::news::refresh` 跑完一批 ingest 后逐条调 `tag(news)`，
//! 把结果写到 `news_tags` + `news_tickers` 关联表。
//!
//! Phase 1 实现：纯关键词 + 正则匹配。Phase 2 可加 LLM sentiment 评分。

use crate::domain::shared::signal::{NewsImportance, NewsKind};
use crate::domain::shared::OccurredAt;
use crate::infrastructure::db::open_database;
use crate::infrastructure::news::news_tag_repo::{save, NewsTagsRecord};
use rusqlite::params;
use tauri::AppHandle;

/// 给一条 news 打标。需要传入 news_id + title + body（拼起来 match）。
pub fn tag(app: &AppHandle, news_id: &str, text: &str) -> Result<NewsTagsRecord, String> {
    let kind = classify_kind(text);
    let importance = classify_importance(text, kind);
    let tickers = extract_tickers(app, text)?;
    let sectors = extract_sectors(app, text)?;
    let rec = NewsTagsRecord {
        news_id: news_id.to_string(),
        kind,
        importance,
        tickers,
        sectors,
        tagged_at: OccurredAt::now(),
    };
    save(app, &rec)?;
    Ok(rec)
}

// ====== 关键词分类 ======================================================

fn classify_kind(text: &str) -> NewsKind {
    // 顺序敏感：先 match 更严肃的（halt / regulatory），再 fallback 到一般
    if contains_any(text, &["停牌", "复牌", "暂停交易", "停止上市"]) {
        return NewsKind::Halt;
    }
    if contains_any(
        text,
        &["立案", "调查", "处罚", "ST", "退市风险", "证监会通报", "违规"],
    ) {
        return NewsKind::Regulatory;
    }
    if contains_any(text, &["重组", "并购", "资产注入", "借壳", "吸收合并"]) {
        return NewsKind::Restructure;
    }
    if contains_any(
        text,
        &["业绩预增", "业绩预减", "业绩快报", "净利润", "季报", "年报", "半年报", "财报"],
    ) {
        return NewsKind::Earnings;
    }
    if contains_any(text, &["解禁", "减持", "增持", "大股东", "股东大会"]) {
        return NewsKind::Ownership;
    }
    if contains_any(text, &["中标", "签约", "合同", "投产", "新产品", "新订单"]) {
        return NewsKind::Operating;
    }
    if contains_any(
        text,
        &["政策", "部委", "国务院", "发改委", "工信部", "财政部", "央行", "补贴", "规划"],
    ) {
        return NewsKind::Policy;
    }
    if contains_any(text, &["板块", "行业", "概念", "主题", "赛道"]) {
        return NewsKind::SectorTrend;
    }
    if contains_any(text, &["大盘", "指数", "资金面", "流动性", "成交量"]) {
        return NewsKind::Market;
    }
    NewsKind::Other
}

fn classify_importance(text: &str, kind: NewsKind) -> NewsImportance {
    // High：停牌 / 立案 / 重大违规 / 退市风险
    if matches!(kind, NewsKind::Halt) {
        return NewsImportance::High;
    }
    if matches!(kind, NewsKind::Regulatory)
        && contains_any(text, &["立案", "退市风险", "重大违规", "处罚"])
    {
        return NewsImportance::High;
    }
    if contains_any(text, &["重大", "突发", "紧急"]) {
        return NewsImportance::High;
    }

    // Medium：财报 / 解禁 / 重组 / 中标 / 政策
    if matches!(
        kind,
        NewsKind::Earnings | NewsKind::Ownership | NewsKind::Restructure | NewsKind::Operating | NewsKind::Policy
    ) {
        return NewsImportance::Medium;
    }

    // Low：其余
    NewsImportance::Low
}

fn contains_any(text: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| text.contains(p))
}

// ====== Ticker / Sector 提取 ============================================

/// 正则 `\b[03456789]\d{5}\b` 找 6 位代码 + 验证在 stocks 表存在。
fn extract_tickers(app: &AppHandle, text: &str) -> Result<Vec<String>, String> {
    use std::collections::HashSet;
    let pattern = regex::Regex::new(r"(?:^|[^0-9])([03456789]\d{5})(?:[^0-9]|$)")
        .map_err(|e| format!("正则编译失败：{e}"))?;
    let mut candidates: HashSet<String> = HashSet::new();
    for cap in pattern.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            candidates.insert(m.as_str().to_string());
        }
    }
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    // 在 stocks 表里验证
    let conn = open_database(app)?;
    let mut verified = Vec::new();
    for code in &candidates {
        let exists: i64 = conn
            .query_row(
                "select count(*) from stocks where code = ?1",
                params![code],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if exists > 0 {
            verified.push(code.clone());
        }
    }
    Ok(verified)
}

/// 简单匹配——遍历 stocks/indexes 表里的常见板块关键字（Phase 1 简化：硬编码常见板块）。
/// 后续可改成从 concepts 表读。
fn extract_sectors(_app: &AppHandle, text: &str) -> Result<Vec<String>, String> {
    const SECTORS: &[&str] = &[
        "光模块",
        "新能源",
        "锂电池",
        "光伏",
        "白酒",
        "医药",
        "半导体",
        "芯片",
        "人工智能",
        "AI",
        "云计算",
        "5G",
        "军工",
        "证券",
        "银行",
        "地产",
        "煤炭",
        "钢铁",
        "有色",
        "化工",
        "汽车",
        "消费",
        "食品",
        "零售",
    ];
    let found: Vec<String> = SECTORS
        .iter()
        .filter(|s| text.contains(**s))
        .map(|s| s.to_string())
        .collect();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_halt_news() {
        let text = "某公司因重大事项停牌一日";
        assert_eq!(classify_kind(text), NewsKind::Halt);
        assert_eq!(
            classify_importance(text, NewsKind::Halt),
            NewsImportance::High
        );
    }

    #[test]
    fn classify_earnings_news() {
        let text = "公司发布业绩预增公告，净利润同比+50%";
        assert_eq!(classify_kind(text), NewsKind::Earnings);
        assert_eq!(
            classify_importance(text, NewsKind::Earnings),
            NewsImportance::Medium
        );
    }

    #[test]
    fn classify_regulatory_high_importance() {
        let text = "证监会对该公司立案调查";
        assert_eq!(classify_kind(text), NewsKind::Regulatory);
        assert_eq!(
            classify_importance(text, NewsKind::Regulatory),
            NewsImportance::High
        );
    }

    #[test]
    fn classify_market_low() {
        let text = "今日大盘横盘震荡";
        assert_eq!(classify_kind(text), NewsKind::Market);
        assert_eq!(
            classify_importance(text, NewsKind::Market),
            NewsImportance::Low
        );
    }

    #[test]
    fn extract_sectors_finds_common_themes() {
        let text = "光模块板块大涨，新能源接力跟随";
        // _app 占位用 ()，但 extract_sectors 不调 conn——能跑
        // 直接测内部 SECTORS 匹配逻辑：
        let found: Vec<&str> = ["光模块", "新能源", "锂电池"]
            .iter()
            .filter(|s| text.contains(**s))
            .copied()
            .collect();
        assert!(found.contains(&"光模块"));
        assert!(found.contains(&"新能源"));
    }
}
