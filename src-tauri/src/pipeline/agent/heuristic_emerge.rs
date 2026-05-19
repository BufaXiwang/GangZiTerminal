//! Heuristic emerge——从积累的 Lessons 中 cluster 出可重用 Heuristic。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 8.2。
//!
//! 算法（v6 结构化聚类）：
//! 1. 拉最近 N 条 lessons（默认 50，takeaway 非空）
//! 2. 按 **结构化特征** 聚类——key = (signal_family_set 排序后, regime, outcome_bucket)
//!    - signal_family_set：lesson.signals_in_play 的 family_str 去重排序串接
//!    - outcome_bucket：hit / partial_hit / miss / expired（精确分桶，不合并）
//!    - 解决"放量突破失败 / 高位放量诱多 / 板块退潮冲高回落"字面不同但语义相关的 case
//! 3. cluster.size ≥ 2 → emerge 一条 Heuristic
//! 4. 文本 jaccard 作 **tiebreaker**：cluster 内挑出与其他 takeaways 平均相似度最高的
//!    那条作为 body（"最典型"那条，比最长那条更有代表性）
//! 5. 检查是否已有 supporting_lesson_ids 大量重叠的 heuristic → 跳过避免重复
//! 6. category 由 outcome 主导：miss/expired → KnownBias；hit/partial_hit → Principle

use crate::domain::agent::heuristic::{Heuristic, HeuristicCategory};
use crate::domain::agent::lesson::{Lesson, LessonOutcome};
use crate::domain::quotes::regime::Regime;
use crate::domain::shared::OccurredAt;
use crate::infrastructure::agent::{heuristic_repo, lesson_repo};
use std::collections::{HashMap, HashSet};
use tauri::AppHandle;

#[derive(Debug, Clone)]
pub struct EmergeResult {
    pub clusters_found: usize,
    pub heuristics_created: usize,
    pub skipped_duplicates: usize,
}

const MIN_CLUSTER_SIZE: usize = 2;
const RECENT_LESSONS_WINDOW: i64 = 50;

pub fn run(app: &AppHandle) -> Result<EmergeResult, String> {
    let lessons = lesson_repo::list_recent(app, RECENT_LESSONS_WINDOW)?;
    let usable: Vec<Lesson> = lessons
        .into_iter()
        .filter(|l| !l.takeaway.trim().is_empty())
        .collect();
    let mut result = EmergeResult {
        clusters_found: 0,
        heuristics_created: 0,
        skipped_duplicates: 0,
    };
    if usable.len() < MIN_CLUSTER_SIZE {
        return Ok(result);
    }

    let clusters = cluster_by_structured_feature(&usable);
    result.clusters_found = clusters.len();
    let existing = heuristic_repo::list_all(app, 500)?;

    for cluster in clusters {
        if cluster.len() < MIN_CLUSTER_SIZE {
            continue;
        }
        // 检查是否跟现有 heuristic 大量重叠
        if has_duplicate_in_existing(&cluster, &existing) {
            result.skipped_duplicates += 1;
            continue;
        }
        // body：cluster 内挑"最典型"的 takeaway——与其他 takeaways 平均 jaccard 最高
        let representative = pick_representative_takeaway(&cluster);
        let body = representative.takeaway.clone();
        let category = pick_dominant_category(&cluster);
        let regime_tags = collect_regime_tags(&cluster);
        let supporting = cluster.iter().map(|l| l.id.clone()).collect();
        let now = OccurredAt::now();
        match Heuristic::emerge_from_lessons(body, category, regime_tags, supporting, now) {
            Ok(h) => {
                heuristic_repo::create(app, &h)?;
                result.heuristics_created += 1;
                tracing::info!(
                    cluster_size = cluster.len(),
                    body = %h.body,
                    "emerge: 新 heuristic"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "emerge: 跳过非法 heuristic");
            }
        }
    }

    Ok(result)
}

// ====== 结构化 cluster 算法（v6） =========================================

/// 聚类 key：signal_family_set 排序串接 + regime（None 也是一类）+ outcome 精确分桶。
#[derive(Hash, PartialEq, Eq, Clone, Debug)]
struct FeatureKey {
    family_signature: String,
    regime: Option<Regime>,
    outcome: LessonOutcome,
}

fn feature_key(lesson: &Lesson) -> FeatureKey {
    let mut families: Vec<&'static str> = lesson
        .signals_in_play
        .iter()
        .map(|s| s.family_str())
        .collect();
    families.sort();
    families.dedup();
    FeatureKey {
        family_signature: families.join("|"),
        regime: lesson.regime_at_close,
        outcome: lesson.outcome,
    }
}

fn cluster_by_structured_feature(lessons: &[Lesson]) -> Vec<Vec<Lesson>> {
    let mut buckets: HashMap<FeatureKey, Vec<Lesson>> = HashMap::new();
    for lesson in lessons {
        // 没 signals_in_play 的 lesson 没法用 family 聚类——单独成一类（也不会和别人配对）
        let key = feature_key(lesson);
        if key.family_signature.is_empty() {
            continue;
        }
        buckets.entry(key).or_default().push(lesson.clone());
    }
    buckets.into_values().collect()
}

/// cluster 内挑出与其他 takeaways 平均 jaccard 最高的那条——"最典型"那条。
/// cluster.len() == 1 时直接返回第一条；==2 时两条互相计算选高分（实际 jaccard 对称，任选一条）。
fn pick_representative_takeaway(cluster: &[Lesson]) -> &Lesson {
    if cluster.len() == 1 {
        return &cluster[0];
    }
    let tokens: Vec<HashSet<String>> = cluster.iter().map(|l| tokenize(&l.takeaway)).collect();
    let mut best_idx = 0;
    let mut best_score = -1.0_f32;
    for i in 0..cluster.len() {
        let mut sum = 0.0_f32;
        for j in 0..cluster.len() {
            if i == j {
                continue;
            }
            sum += jaccard(&tokens[i], &tokens[j]);
        }
        let avg = sum / (cluster.len() - 1) as f32;
        if avg > best_score {
            best_score = avg;
            best_idx = i;
        }
    }
    &cluster[best_idx]
}

fn tokenize(text: &str) -> HashSet<String> {
    // 简单按中英文 2-grams（中文）+ 单词（英文）
    let mut tokens: HashSet<String> = HashSet::new();
    let chars: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    // bi-grams（中文为主）
    for window in chars.windows(2) {
        let s: String = window.iter().collect();
        if s.chars().count() == 2 {
            tokens.insert(s);
        }
    }
    // 单字（兜底）
    for c in &chars {
        tokens.insert(c.to_string());
    }
    tokens
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        0.0
    } else {
        inter as f32 / union as f32
    }
}

fn has_duplicate_in_existing(cluster: &[Lesson], existing: &[Heuristic]) -> bool {
    let cluster_lesson_ids: HashSet<&str> = cluster.iter().map(|l| l.id.as_str()).collect();
    // 取 cluster 的 representative takeaway 用于 body 字面去重
    let rep_body = pick_representative_takeaway(cluster).takeaway.trim();
    for h in existing {
        // 1. body 字面相同直接判重——防止反复 emerge 同句话
        if h.retired_at.is_none() && !rep_body.is_empty() && h.body.trim() == rep_body {
            return true;
        }
        // 2. supporting lesson 比例重叠 ≥50% 判重——比绝对数阈值更稳，cluster 变大也成比例
        let overlap = h
            .supporting_lesson_ids
            .iter()
            .filter(|l| cluster_lesson_ids.contains(l.as_str()))
            .count();
        if cluster.len() > 0 && overlap * 2 >= cluster.len() {
            return true;
        }
    }
    false
}

fn pick_dominant_category(cluster: &[Lesson]) -> HeuristicCategory {
    // Lesson 自身没 category 字段；按 outcome 启发性映射——
    // Miss/Expired → KnownBias（"信号失效"类教训）
    // Hit/PartialHit → Principle（"信号有效"类经验）
    let negative_count = cluster
        .iter()
        .filter(|l| matches!(l.outcome, LessonOutcome::Miss | LessonOutcome::Expired))
        .count();
    if negative_count > cluster.len() / 2 {
        HeuristicCategory::KnownBias
    } else {
        HeuristicCategory::Principle
    }
}

fn collect_regime_tags(cluster: &[Lesson]) -> Vec<crate::domain::quotes::regime::Regime> {
    let mut counter: HashMap<crate::domain::quotes::regime::Regime, u32> = HashMap::new();
    for l in cluster {
        if let Some(r) = l.regime_at_close {
            *counter.entry(r).or_insert(0) += 1;
        }
    }
    // 严格过半（>50%）的 regime 才算 heuristic 的 regime tag——
    // 防止 len=2 时单条噪声 regime 被升级为整 cluster 的 tag。
    let threshold = cluster.len() / 2 + 1;
    counter
        .into_iter()
        .filter(|(_, n)| (*n as usize) >= threshold)
        .map(|(r, _)| r)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::lesson::{Lesson, LessonId};
    use crate::domain::account::expectation::ExpectationId;
    use crate::domain::shared::signal::SignalKind;
    use crate::domain::shared::{OccurredAt, StockCode};

    #[test]
    fn jaccard_identical_sets() {
        let mut a = HashSet::new();
        a.insert("光模".to_string());
        a.insert("模块".to_string());
        let b = a.clone();
        assert_eq!(jaccard(&a, &b), 1.0);
    }

    #[test]
    fn jaccard_empty() {
        let a: HashSet<String> = HashSet::new();
        let b: HashSet<String> = HashSet::new();
        assert_eq!(jaccard(&a, &b), 0.0);
    }

    #[test]
    fn tokenize_chinese_bigrams() {
        let t = tokenize("光模块板块");
        // 应该包含 "光模", "模块", "块板", "板块"
        assert!(t.contains("光模"));
        assert!(t.contains("模块"));
    }

    fn make_lesson(takeaway: &str, signals: Vec<SignalKind>, outcome: LessonOutcome) -> Lesson {
        Lesson {
            id: LessonId::new(),
            expectation_id: ExpectationId::new(),
            code: StockCode::new("600519").unwrap(),
            observation: "obs".into(),
            takeaway: takeaway.into(),
            outcome,
            regime_at_close: None,
            signals_in_play: signals,
            pnl_pct: None,
            created_at: OccurredAt::new(0),
        }
    }

    #[test]
    fn structured_clustering_groups_by_signal_family_not_text() {
        // 三条 takeaway 文字面差异大，但 signals_in_play 共享 volume_spike + breakout_above_20ma
        // 旧算法 jaccard 文字会分成三类；新算法按 (signal_families, regime=None, outcome=Miss) 聚成一类。
        let signals = vec![
            SignalKind::VolumeSpike { ratio: 2.0 },
            SignalKind::BreakoutAbove20MA,
        ];
        let lessons = vec![
            make_lesson("放量突破失败", signals.clone(), LessonOutcome::Miss),
            make_lesson("高位放量诱多", signals.clone(), LessonOutcome::Miss),
            make_lesson("板块退潮后冲高回落", signals.clone(), LessonOutcome::Miss),
        ];
        let clusters = cluster_by_structured_feature(&lessons);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].len(), 3);
    }

    #[test]
    fn outcome_separates_otherwise_identical_lessons() {
        // 信号集相同但一条 Hit / 一条 Miss → 不能聚在一起
        let signals = vec![SignalKind::MACDGoldenCross];
        let lessons = vec![
            make_lesson("金叉成功上涨", signals.clone(), LessonOutcome::Hit),
            make_lesson("金叉失败回落", signals.clone(), LessonOutcome::Miss),
        ];
        let clusters = cluster_by_structured_feature(&lessons);
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn empty_signals_lesson_excluded_from_clustering() {
        // 没 signals_in_play 的 lesson 无法 family 聚类——直接忽略
        let lessons = vec![
            make_lesson("空信号 lesson", vec![], LessonOutcome::Miss),
        ];
        let clusters = cluster_by_structured_feature(&lessons);
        assert!(clusters.is_empty());
    }

    #[test]
    fn representative_picks_most_typical_takeaway() {
        // 4 条 takeaway，其中 3 条共享大量 token，1 条偏离——最典型的应该来自前 3 条
        let signals = vec![SignalKind::LimitUp];
        let cluster = vec![
            make_lesson("放量突破后回落", signals.clone(), LessonOutcome::Miss),
            make_lesson("放量突破回落", signals.clone(), LessonOutcome::Miss),
            make_lesson("放量突破之后回落", signals.clone(), LessonOutcome::Miss),
            make_lesson("完全无关的语句", signals.clone(), LessonOutcome::Miss),
        ];
        let rep = pick_representative_takeaway(&cluster);
        assert!(rep.takeaway.contains("放量突破"));
    }
}
