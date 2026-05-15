//! 学习画像 builder——从 records / positions / quotes 推导出给 prompt 用的状态摘要。
//!
//! Rust 端口自 src/lib/learning.ts。

use crate::agent_io::{
    LearningProfile, NameCount, SimulatedPosition, StoredAnalysisRecord, TextCount,
};
use crate::domain::quotes::StockQuote;
use crate::risk::calculate_simulation_account;

pub fn build_learning_profile(
    records: &[StoredAnalysisRecord],
    positions: &[SimulatedPosition],
    quotes: &[StockQuote],
    initial_cash: f64,
) -> LearningProfile {
    let reviewed: Vec<&StoredAnalysisRecord> =
        records.iter().filter(|r| r.review.is_some()).collect();

    let mut validated = 0;
    let mut invalidated = 0;
    let mut watching = 0;
    let mut inconclusive = 0;
    for r in &reviewed {
        match r.review.as_ref().map(|v| v.thesis_status.as_str()) {
            Some("validated") => validated += 1,
            Some("invalidated") => invalidated += 1,
            Some("watching") => watching += 1,
            _ => inconclusive += 1,
        }
    }

    let account = calculate_simulation_account(initial_cash, positions, quotes);

    // top themes：themes + sectors + 非 6 位代码的 relatedStocks（公司名/概念）
    let mut theme_pool: Vec<String> = Vec::new();
    for record in records {
        theme_pool.extend(record.result.themes.iter().cloned());
        theme_pool.extend(record.result.sectors.iter().cloned());
        for stock in &record.result.related_stocks {
            if !is_6digit(stock) {
                theme_pool.push(stock.clone());
            }
        }
    }
    let top_themes: Vec<NameCount> = rank_text(&theme_pool)
        .into_iter()
        .take(6)
        .map(|(name, count)| NameCount { name, count })
        .collect();

    let mistake_pool: Vec<String> = reviewed
        .iter()
        .flat_map(|r| {
            r.review
                .as_ref()
                .map(|v| v.mistakes.clone())
                .unwrap_or_default()
        })
        .collect();
    let common_mistakes: Vec<TextCount> = rank_text(&mistake_pool)
        .into_iter()
        .take(5)
        .map(|(text, count)| TextCount { text, count })
        .collect();

    let total = records.len() as f64;
    let reviewed_count = reviewed.len() as f64;
    let review_rate = if total > 0.0 {
        reviewed_count / total
    } else {
        0.0
    };
    let decisive = (validated + invalidated) as f64;
    let validation_rate = if decisive > 0.0 {
        validated as f64 / decisive
    } else {
        0.0
    };
    let pnl_score = (account.total_pnl / 100.0).clamp(-10.0, 12.0);
    let raw_score = total * 1.4 + review_rate * 28.0 + validation_rate * 18.0 + pnl_score;
    let score = (raw_score.round() as i32).clamp(0, 100);
    let level = ((score / 8) + 1).clamp(1, 20);

    LearningProfile {
        level,
        score,
        total_records: records.len() as i32,
        reviewed_records: reviewed.len() as i32,
        review_rate,
        validated_count: validated,
        invalidated_count: invalidated,
        watching_count: watching,
        inconclusive_count: inconclusive,
        top_themes,
        common_mistakes: common_mistakes.clone(),
        focus_suggestions: build_focus_suggestions(
            records,
            review_rate,
            validation_rate,
            &common_mistakes,
        ),
    }
}

fn is_6digit(s: &str) -> bool {
    s.len() == 6 && s.chars().all(|c| c.is_ascii_digit())
}

fn rank_text(items: &[String]) -> Vec<(String, i32)> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, i32> = HashMap::new();
    for raw in items {
        let key = raw.trim();
        if key.is_empty() {
            continue;
        }
        *counts.entry(key.to_string()).or_insert(0) += 1;
    }
    let mut v: Vec<(String, i32)> = counts.into_iter().collect();
    // 按 count desc，再按 name asc（稳定）
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

fn build_focus_suggestions(
    records: &[StoredAnalysisRecord],
    review_rate: f64,
    validation_rate: f64,
    mistakes: &[TextCount],
) -> Vec<String> {
    let mut out = Vec::new();
    if records.is_empty() {
        out.push("先积累 5 条事件分析，建立可复盘样本。".into());
    }
    if !records.is_empty() && review_rate < 0.35 {
        out.push("提高复盘完成率，把验证清单逐条回看。".into());
    }
    if review_rate >= 0.35 && validation_rate < 0.45 {
        out.push("减少过度映射，优先验证公告、成交量和板块强弱。".into());
    }
    if let Some(top_mistake) = mistakes.first() {
        out.push(format!("重点修正：{}", top_mistake.text));
    }
    if out.is_empty() {
        out.push("继续扩大主题样本，比较不同事件类型的验证表现。".into());
    }
    out.truncate(4);
    out
}
