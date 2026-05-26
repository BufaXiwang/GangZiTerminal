//! Per-run packet scope cache —— spec `agent-runtime-module.md §2 EvidenceRef`。
//!
//! 用途：EvidenceRef hydrate 时要校验「declared evidence 是否来自本 run 的
//! packet / tool results / active strategy / recent episodes 等」。把
//! packet build 时注入的 ids 集合落到这里，hydrate 时查白名单。
//!
//! - `packet_news_ids`：本 run packet 注入的 newsId（用于 source=packet 校验）
//! - `packet_ts_codes`：本 run packet 注入的 tsCode（watchlist / quotes section）
//! - `packet_position_ids`：本 run packet 注入的 positionId（account.positions）
//! - `packet_strategy_ids`：本 run packet 注入的 strategyId（active strategy cards）
//! - `recent_episode_ids`：本 run packet 注入的 recent episode summary ids
//! - `replay_episode_ids`：manual_replay 显式注入的 ref episode ids
//!
//! 进程内存 OnceLock + HashMap；run 结束时 drop 释放。

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Default)]
pub struct RunScope {
    pub packet_news_ids: HashSet<String>,
    pub packet_ts_codes: HashSet<String>,
    pub packet_position_ids: HashSet<String>,
    pub packet_strategy_ids: HashSet<String>,
    pub recent_episode_ids: HashSet<String>,
    pub recent_review_ids: HashSet<String>,
    pub replay_episode_ids: HashSet<String>,
    pub trigger_kind: String,
}

fn store() -> &'static Mutex<HashMap<String, RunScope>> {
    static S: OnceLock<Mutex<HashMap<String, RunScope>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 写入或合并 —— 若 run_id 已有 scope，新 scope 的所有集合做 union；trigger_kind
/// 若新值非空则覆盖。这样 packet builder / scheduler / router 可以各自 put 自己
/// 看到的 ids，不会互相覆盖。
pub fn put(run_id: &str, scope: RunScope) {
    if let Ok(mut g) = store().lock() {
        let entry = g
            .entry(run_id.to_string())
            .or_insert_with(RunScope::default);
        entry.packet_news_ids.extend(scope.packet_news_ids);
        entry.packet_ts_codes.extend(scope.packet_ts_codes);
        entry.packet_position_ids.extend(scope.packet_position_ids);
        entry.packet_strategy_ids.extend(scope.packet_strategy_ids);
        entry.recent_episode_ids.extend(scope.recent_episode_ids);
        entry.recent_review_ids.extend(scope.recent_review_ids);
        entry.replay_episode_ids.extend(scope.replay_episode_ids);
        if !scope.trigger_kind.is_empty() {
            entry.trigger_kind = scope.trigger_kind;
        }
    }
}

pub fn get(run_id: &str) -> Option<RunScope> {
    store().lock().ok().and_then(|g| g.get(run_id).cloned())
}

pub fn remove(run_id: &str) {
    if let Ok(mut g) = store().lock() {
        g.remove(run_id);
    }
}
