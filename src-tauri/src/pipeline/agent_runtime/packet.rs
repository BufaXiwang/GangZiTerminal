//! `RealtimeDecisionPacket` builder —— spec `agent-runtime-module.md §3`。
//!
//! 把账户、自选、最近 decision episode、active strategy card 等 application
//! 状态拼装成一份 packet 投影，注入 Agent run。
//!
//! 第一阶段 packet：
//! - **account section**：账户 snapshot + 自选（行情字段为空，Agent 用 fetch_quotes 拉）
//! - **strategies section**：当前 active StrategyCard
//! - **recent_episodes section**：最近 N 条 DecisionEpisode 摘要
//! - **quotes / news section**：留空，Agent 按需通过工具拉取
//!
//! Quotes / News 的 packet 字段（PacketQuotes / PacketNews 完整结构）未来接入。

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::domain::agent_runtime::decisions::{StrategyCard, StrategyStatus};
use crate::domain::agent_runtime::runs::AgentRunProfileId;
use crate::infrastructure::account::watchlist;
use crate::infrastructure::agent_runtime::{episodes_repo, reviews_repo, strategy_cards_repo};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PacketWatchlistItem {
    pub ts_code: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PacketAccount {
    /// 来自 `crate::domain::account::types::AccountSnapshot`；为减少耦合用 JSON value。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watchlist: Vec<PacketWatchlistItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PacketEpisodeSummary {
    pub episode_id: String,
    pub created_at: String,
    pub symbols: Vec<String>,
    pub action: String,
    pub action_status: String,
    pub thesis: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PacketReviewSummary {
    pub review_id: String,
    pub episode_id: String,
    pub trigger: String,
    pub conclusion: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PacketUserPreferences {
    #[serde(default)]
    pub risk_tolerance: Option<String>,
    #[serde(default)]
    pub default_holding_horizon: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PacketQuotes {
    /// 关注集 ts_code → 最近 snapshot 摘要；空 map 表示按需走 fetch_quotes。
    #[serde(default)]
    pub items: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PacketNews {
    /// 最近 N 条新闻摘要；空表示按需走 fetch_news。
    #[serde(default)]
    pub items: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeDecisionPacket {
    pub run_id: String,
    pub profile_id: String,
    pub trigger_kind: String,
    pub account: PacketAccount,
    pub quotes: PacketQuotes,
    pub news: PacketNews,
    pub strategies: Vec<StrategyCard>,
    pub recent_episodes: Vec<PacketEpisodeSummary>,
    pub recent_reviews: Vec<PacketReviewSummary>,
    pub user_preferences: PacketUserPreferences,
}

impl RealtimeDecisionPacket {
    /// 渲染为给 Agent system context 用的中文摘要。
    pub fn to_summary_text(&self) -> String {
        let mut buf = String::new();
        buf.push_str(&format!(
            "Agent Run profile = `{}`，trigger = `{}`。\n\n",
            self.profile_id, self.trigger_kind
        ));
        if !self.strategies.is_empty() {
            buf.push_str("Active StrategyCard：\n");
            for card in &self.strategies {
                buf.push_str(&format!(
                    "- {} (id={}, v{}): {}\n",
                    card.name, card.strategy_id, card.version, card.description
                ));
            }
            buf.push('\n');
        }
        if let Some(snap) = &self.account.snapshot {
            buf.push_str("Account snapshot（packet 时刻，详细字段以 fetch_account 实时为准）：\n");
            buf.push_str(&format!("```json\n{}\n```\n\n", snap));
        }
        if !self.account.watchlist.is_empty() {
            let codes: Vec<&str> = self
                .account
                .watchlist
                .iter()
                .map(|w| w.ts_code.as_str())
                .collect();
            buf.push_str(&format!("自选标的：{}\n\n", codes.join(", ")));
        }
        if !self.recent_episodes.is_empty() {
            buf.push_str("最近 DecisionEpisode（供回顾纪律）：\n");
            for ep in &self.recent_episodes {
                buf.push_str(&format!(
                    "- [{}] {}/{} symbols={} · {}\n",
                    ep.created_at,
                    ep.action,
                    ep.action_status,
                    ep.symbols.join(","),
                    truncate(&ep.thesis, 80)
                ));
            }
            buf.push('\n');
        }
        if !self.recent_reviews.is_empty() {
            buf.push_str("最近 DecisionReview（episode 复盘）：\n");
            for rv in &self.recent_reviews {
                buf.push_str(&format!(
                    "- [{}] episode={} trigger={} · {}\n",
                    rv.created_at,
                    rv.episode_id,
                    rv.trigger,
                    truncate(&rv.conclusion, 80)
                ));
            }
            buf.push('\n');
        }
        buf
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// 构造 packet（spec §9 `build_realtime_decision_packet`）。
///
/// 当前阶段同步实现 —— AccountService.snapshot 是同步的；watchlist 是内存 read；
/// repos 是 SQLite。
pub fn build(
    app: &AppHandle,
    run_id: &str,
    profile: AgentRunProfileId,
    trigger_kind: &str,
) -> Result<RealtimeDecisionPacket, String> {
    use crate::pipeline::agent_runtime::run_scope::{put as put_scope, RunScope};

    let mut account = PacketAccount::default();
    let mut scope = RunScope::default();
    scope.trigger_kind = trigger_kind.to_string();

    let svc = crate::pipeline::account::AccountService::new(app.clone());
    if let Ok(snap) = svc.snapshot() {
        for p in &snap.open_positions {
            scope.packet_position_ids.insert(p.id.as_str().to_string());
            scope.packet_ts_codes.insert(p.code.as_str().to_string());
        }
        account.snapshot = serde_json::to_value(snap).ok();
    }
    let watchlist_codes = watchlist::list_strings();
    for ts in &watchlist_codes {
        scope.packet_ts_codes.insert(ts.clone());
    }
    account.watchlist = watchlist_codes
        .into_iter()
        .map(|ts| PacketWatchlistItem { ts_code: ts })
        .collect();

    let strategies: Vec<StrategyCard> = strategy_cards_repo::list(app, Some("active"))
        .unwrap_or_default()
        .into_iter()
        .filter(|c| matches!(c.status, StrategyStatus::Active))
        .collect();
    for s in &strategies {
        scope.packet_strategy_ids.insert(s.strategy_id.clone());
    }

    let recent_episodes: Vec<PacketEpisodeSummary> = episodes_repo::list_recent(app, 10)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| {
            Some(PacketEpisodeSummary {
                episode_id: v.get("episodeId")?.as_str()?.to_string(),
                created_at: v.get("createdAt")?.as_str()?.to_string(),
                symbols: v
                    .get("symbols")?
                    .as_array()?
                    .iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect(),
                action: v.get("action")?.as_str()?.to_string(),
                action_status: v.get("actionStatus")?.as_str()?.to_string(),
                thesis: v
                    .get("thesis")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect();
    for e in &recent_episodes {
        scope.recent_episode_ids.insert(e.episode_id.clone());
    }

    // spec §2 `source=recent_review`：注入 recent reviews 摘要 + 把 review_id 灌进 scope
    let recent_reviews: Vec<PacketReviewSummary> = reviews_repo::list_recent(app, 20)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| {
            Some(PacketReviewSummary {
                review_id: v.get("reviewId")?.as_str()?.to_string(),
                episode_id: v.get("episodeId")?.as_str()?.to_string(),
                trigger: v.get("trigger")?.as_str()?.to_string(),
                conclusion: v
                    .get("conclusion")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                created_at: v.get("createdAt")?.as_str()?.to_string(),
            })
        })
        .collect();
    for r in &recent_reviews {
        scope.recent_review_ids.insert(r.review_id.clone());
    }

    let profile_str = match profile {
        AgentRunProfileId::UserChat => "user_chat",
        AgentRunProfileId::NewsAnalysis => "news_analysis",
        AgentRunProfileId::AccountTriggerResponse => "account_trigger_response",
        AgentRunProfileId::ScheduledReview => "scheduled_review",
        AgentRunProfileId::ManualReplay => "manual_replay",
    };

    put_scope(run_id, scope);

    let packet = RealtimeDecisionPacket {
        run_id: run_id.to_string(),
        profile_id: profile_str.to_string(),
        trigger_kind: trigger_kind.to_string(),
        account,
        quotes: PacketQuotes::default(),
        news: PacketNews::default(),
        strategies,
        recent_episodes,
        recent_reviews,
        user_preferences: PacketUserPreferences::default(),
    };

    // spec §2/§3「profile.requiredPacketSections」校验：缺必需 section 时告警。
    use crate::domain::agent_runtime::tools::{required_packet_sections, PacketSection};
    let required = required_packet_sections(profile);
    let mut missing: Vec<&'static str> = Vec::new();
    for sec in required {
        let ok = match sec {
            PacketSection::Account => packet.account.snapshot.is_some(),
            PacketSection::Strategies => !packet.strategies.is_empty(),
            PacketSection::RecentEpisodes => !packet.recent_episodes.is_empty(),
            // Quotes / News / UserPreferences 当前由按需 fetch 工具承担，packet
            // 内允许空容器；spec L192「user_chat 账户/行情/新闻按工具调用实时读取」
            // 即默认行为，不视为缺失。
            PacketSection::Quotes
            | PacketSection::News
            | PacketSection::UserPreferences => true,
        };
        if !ok {
            missing.push(sec.as_str());
        }
    }
    if !missing.is_empty() {
        tracing::warn!(
            target = "agent_runtime.packet",
            run_id = %run_id,
            profile = %profile_str,
            missing = ?missing,
            "RealtimeDecisionPacket 缺 spec §2 requiredPacketSections"
        );
    }

    Ok(packet)
}
