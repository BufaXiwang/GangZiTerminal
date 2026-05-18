//! Heuristic emerge——从积累的 Lessons 中 cluster 出可重用 Heuristic。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 8.2。
//!
//! 算法（Phase 1 简化）：
//! 1. 拉最近 N 条 lessons（默认 50）
//! 2. 按 takeaway 文本 token jaccard 相似度（≥0.6）聚类
//! 3. cluster.size ≥ 2 且 takeaway 不为空 → emerge 一条 Heuristic
//! 4. 检查是否已有 supporting_lesson_ids 大量重叠的 heuristic → 跳过避免重复
//! 5. category 取该 cluster lessons 涉及最多的（默认 Principle）；regime_tags 来自 lessons.regime_at_close

use crate::domain::agent::heuristic::{Heuristic, HeuristicCategory};
use crate::domain::agent::lesson::Lesson;
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

const JACCARD_THRESHOLD: f32 = 0.6;
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

    let clusters = cluster_by_takeaway(&usable);
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
        // 生成 heuristic body：取 cluster 里 takeaway 最长的那条（信息最多）
        let representative = cluster
            .iter()
            .max_by_key(|l| l.takeaway.chars().count())
            .unwrap();
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

// ====== cluster 算法 ====================================================

fn cluster_by_takeaway(lessons: &[Lesson]) -> Vec<Vec<Lesson>> {
    let mut clusters: Vec<Vec<Lesson>> = Vec::new();
    for lesson in lessons {
        let tokens = tokenize(&lesson.takeaway);
        let mut joined = false;
        for cluster in clusters.iter_mut() {
            // 跟 cluster 第一条比相似度（粗暴但够用 Phase 1）
            let head_tokens = tokenize(&cluster[0].takeaway);
            if jaccard(&tokens, &head_tokens) >= JACCARD_THRESHOLD {
                cluster.push(lesson.clone());
                joined = true;
                break;
            }
        }
        if !joined {
            clusters.push(vec![lesson.clone()]);
        }
    }
    clusters
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
    for h in existing {
        let overlap = h
            .supporting_lesson_ids
            .iter()
            .filter(|l| cluster_lesson_ids.contains(l.as_str()))
            .count();
        // 一半以上 lesson 已经被某个 heuristic 收录 → 算重复，跳过
        if overlap >= cluster.len().max(1) / 2 + 1 {
            return true;
        }
    }
    false
}

fn pick_dominant_category(cluster: &[Lesson]) -> HeuristicCategory {
    // Phase 1：Lesson 自身没 category 字段；按 outcome 启发性映射——
    // miss outcome 倾向 KnownBias / RiskPreference；hit outcome 倾向 Principle
    let miss_count = cluster
        .iter()
        .filter(|l| matches!(l.outcome, crate::domain::agent::lesson::LessonOutcome::Miss))
        .count();
    if miss_count > cluster.len() / 2 {
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
    // 出现 ≥ cluster size 一半的 regime 算这个 heuristic 的 regime tag
    let threshold = (cluster.len() / 2).max(1);
    counter
        .into_iter()
        .filter(|(_, n)| *n as usize >= threshold)
        .map(|(r, _)| r)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
