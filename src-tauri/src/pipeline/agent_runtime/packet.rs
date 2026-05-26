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
    /// spec §564 PacketAccount.positions：open / closed positions 投影。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub positions: Vec<serde_json::Value>,
    /// spec §564 PacketAccount.orders：pending / 活跃订单投影。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orders: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watchlist: Vec<PacketWatchlistItem>,
    /// spec §564 PacketAccount.triggers：未处理 AccountTrigger 投影。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<serde_json::Value>,
    /// spec §564 PacketAccount.freshness：整体账户估值新鲜度（取 snapshot.valuationFreshness）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<crate::domain::shared::Freshness>,
    /// spec §564 PacketAccount.warnings
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<crate::domain::shared::WarningCode>,
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
    /// spec §655 PacketQuotes.snapshotAt：packet 构造时间戳。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_at: Option<String>,
    /// spec §655 PacketQuotes.items：派生自 market_snapshot 的 PacketQuoteItem。
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
        // spec §564 PacketAccount.freshness：取 snapshot.valuationFreshness
        account.freshness = snap.valuation_freshness.clone();
        account.warnings = snap.warnings.clone();
        // spec §564 PacketAccount.positions：open positions 投影（spec PacketPosition shape）
        account.positions = snap
            .open_positions
            .iter()
            .filter_map(|p| {
                scope.packet_position_ids.insert(p.id.as_str().to_string());
                scope.packet_ts_codes.insert(p.code.as_str().to_string());
                serde_json::to_value(p).ok()
            })
            .collect();
        account.snapshot = serde_json::to_value(&snap).ok();
    }
    // spec §564 PacketAccount.orders：未完成订单投影
    if let Ok(orders) =
        crate::infrastructure::account::orders_repo::list_active(app, 200, 0)
    {
        account.orders = orders
            .into_iter()
            .filter_map(|o| serde_json::to_value(o).ok())
            .collect();
    }
    // spec §564 PacketAccount.triggers：未处理 trigger 投影
    if let Ok(triggers) =
        crate::infrastructure::account::trigger_repo::list_filtered(app, Some(false), 100, 0)
    {
        account.triggers = triggers
            .iter()
            .filter_map(|t| serde_json::to_value(t).ok())
            .collect();
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

    // spec §655 PacketQuotes：snapshotAt + freshness + items[]。
    // 当前阶段从 market_snapshot 派生 packet_ts_codes 的 quote 摘要；spec 允许的
    // klines/indicators/scan 为 optional，按 spec §5 默认由 fetch_quotes 工具拉。
    let mut quotes = PacketQuotes::default();
    {
        use crate::infrastructure::quotes::snapshot::market_snapshot;
        let now_iso = chrono::Utc::now().to_rfc3339();
        let mut items: Vec<serde_json::Value> = Vec::new();
        for ts in &scope.packet_ts_codes {
            if let Some(q) = market_snapshot::get(ts) {
                items.push(serde_json::json!({
                    "tsCode": ts,
                    "name": q.name,
                    "category": q.category.as_str(),
                    "price": q.price.as_ref().map(|p| p.value()),
                    "change": q.change.as_ref().map(|p| p.value()),
                    "changePercent": q.change_percent,
                    "volume": q.day_volume.as_ref().map(|v| v.value()),
                    "amount": q.day_amount.as_ref().map(|a| a.value()),
                    "source": q.source.as_str(),
                    "freshness": serde_json::to_value(&q.freshness).unwrap_or(serde_json::Value::Null),
                }));
            }
        }
        quotes.snapshot_at = Some(now_iso);
        quotes.items = items;
    }

    // spec PacketNews：注入最近 N 条新闻摘要供 agent 复用，避免每次 fetch_news 拉一遍。
    let mut news = PacketNews::default();
    {
        use crate::infrastructure::news::repository::list_news_items;
        if let Ok(items) = list_news_items(app.clone(), Some(20)) {
            news.items = items
                .into_iter()
                .map(|n| {
                    serde_json::json!({
                        "newsId": n.id,
                        "title": n.title,
                        "source": n.source,
                        "publishedAt": n.published,
                        "summary": n.summary,
                    })
                })
                .collect();
            for item in &news.items {
                if let Some(id) = item.get("newsId").and_then(|v| v.as_str()) {
                    scope.packet_news_ids.insert(id.to_string());
                }
            }
        }
    }

    put_scope(run_id, scope);

    let packet = RealtimeDecisionPacket {
        run_id: run_id.to_string(),
        profile_id: profile_str.to_string(),
        trigger_kind: trigger_kind.to_string(),
        account,
        quotes,
        news,
        strategies,
        recent_episodes,
        recent_reviews,
        user_preferences: PacketUserPreferences::default(),
    };

    // spec §2/§3「profile.requiredPacketSections」校验。缺必需 section 必须 fail
    // closed（旧实现仅 tracing::warn 后 Ok(packet)，违反 spec §3「packet 必须满足
    // profile 要求」）。
    use crate::domain::agent_runtime::tools::{required_packet_sections, PacketSection};
    let required = required_packet_sections(profile);
    let mut missing: Vec<&'static str> = Vec::new();
    for sec in required {
        let ok = match sec {
            PacketSection::Account => packet.account.snapshot.is_some(),
            PacketSection::Strategies => !packet.strategies.is_empty(),
            PacketSection::RecentEpisodes => !packet.recent_episodes.is_empty(),
            PacketSection::Quotes => packet.quotes.snapshot_at.is_some(),
            PacketSection::News => true, // PacketNews 可空（agent 用 fetch_news 拉）
            PacketSection::UserPreferences => true, // 默认值视为已注入
        };
        if !ok {
            missing.push(sec.as_str());
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "invalid_state: profile `{profile_str}` 的 RealtimeDecisionPacket 缺必需 section {missing:?}（spec §3 fail closed）"
        ));
    }

    Ok(packet)
}
