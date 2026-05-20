//! News review pipeline——把一批 pending news 喂给 agent，让它结合自选股 + 持仓做决策。
//!
//! 触发：`infrastructure/news/batch.rs::claim_batch` 取走一批 + emit "news-batch-ready"
//! → `adapters/news_batch_listener.rs` 收到事件 → 调本 pipeline 的 `run`。
//!
//! 核心任务（数据驱动）：
//! 1. 把 batch news 全量喂 agent（不筛 importance；agent 自己判断）
//! 2. 关联当前 watchlist + 持仓——让 agent 评估「这些 news 是否影响我手里的标的」
//! 3. 决策路径：
//!    - 影响持仓 → close_position / adjust_position
//!    - 影响自选股 → open_position(kind=watch/live)
//!    - 提到新的潜力标的且不在自选 → add_to_watchlist
//!    - 看完无 actionable → 总结一句话即可
//!
//! 学习闭环：agent 决策 → 写入 PositionEvent / Heuristic application → 自然进入
//! auto_review + reflection 链路（无需特殊处理）。

use crate::domain::agent::types::{
    AgentEvent, AgentOptions, AgentRequest, Block, ContextBudget, Message, PipelineKind,
    ProviderKind, Role, ServerSideTool, SystemBlock, ToolDef,
};
use crate::domain::news::NewsId;
use crate::pipeline::agent::config::{build_provider_for_channel, read_agent_config};
use crate::pipeline::agent::observer;
use crate::pipeline::agent::prompt::AGENT_IDENTITY;
use crate::pipeline::agent::run_agent;
use crate::pipeline::agent::tools::{ToolContext, ToolRegistry};
use std::sync::Arc;
use tauri::AppHandle;
use tokio::sync::mpsc;

/// 给一批已 claimed 的 news 跑一次 agent review run。
///
/// 返回 (run_id, succeeded)——caller 据此 mark_consumed / mark_failed。
pub async fn run(
    app: AppHandle,
    registry: Arc<ToolRegistry>,
    batch_id: String,
    news_ids: Vec<NewsId>,
    queued_remaining: u64,
    trigger_reason: String,
) -> Result<String, String> {
    let cfg = read_agent_config(&app);
    let (channel_ref, model_ref) = cfg.resolve_pipeline(PipelineKind::Chat)?;
    let channel = channel_ref.clone();
    let model = model_ref.to_string();

    // 1. 拉 news 详情
    let id_strings: Vec<String> = news_ids.iter().map(|i| i.as_str().to_string()).collect();
    let news_items =
        crate::infrastructure::news::repository::get_news_items_by_ids(app.clone(), id_strings.clone())
            .map_err(|e| format!("拉 news 详情失败：{e}"))?;

    // 2. 构造 prompt 上下文
    let context_text = build_news_review_context(
        &app,
        &batch_id,
        &trigger_reason,
        queued_remaining,
        &news_items,
    );

    // 3. 工具 + web_search 注入（同 mini_scan 习惯）
    let mut tools = registry.to_tool_defs(true);
    let want_web_search = match channel.wire_format {
        ProviderKind::Anthropic => channel.enable_native_web_search,
        ProviderKind::OpenAIResponses => channel.enable_web_search,
        ProviderKind::OpenAIChatCompletions => false,
    };
    if want_web_search {
        tools.push(ToolDef::ServerSide(ServerSideTool::AnthropicWebSearch {
            name: "web_search".into(),
            max_uses: Some(cfg.agent.max_search_calls_per_run),
            allowed_domains: vec![],
            blocked_domains: vec![],
        }));
    }

    let req = AgentRequest {
        system: vec![
            SystemBlock {
                text: AGENT_IDENTITY.to_string(),
                cache_control: false,
            },
            SystemBlock {
                text: NEWS_REVIEW_INSTRUCTIONS.to_string(),
                cache_control: true,
            },
        ],
        tools,
        messages: vec![Message {
            role: Role::User,
            content: vec![Block::Text {
                text: context_text,
                cache_control: false,
            }],
        }],
        options: AgentOptions {
            model: model.clone(),
            max_tokens: 4096,
            temperature: Some(0.5),
            top_p: None,
            thinking: channel.thinking_config(),
            effort: channel.default_effort,
            max_turns: cfg.agent.max_turns_per_run.min(10),
            stop_sequences: vec![],
            tool_timeout_secs: Some(cfg.agent.tool_timeout_secs),
        },
        budget: ContextBudget {
            soft_limit_tokens: cfg.agent.context_soft_limit_tokens,
            hard_limit_tokens: cfg.agent.context_hard_limit_tokens,
            compact_keep_last_n: cfg.agent.compact_keep_last_n_turns,
            max_search_calls: cfg.agent.max_search_calls_per_run,
        },
        trigger_message_id: None,
        pipeline: PipelineKind::Chat,
    };

    let run_id = uuid::Uuid::new_v4().to_string();
    let trigger_ref = format!("news_batch:{}|n={}", batch_id, news_items.len());
    observer::start_episode(
        &app,
        &run_id,
        "news_review",
        Some(&trigger_ref),
        channel.wire_format.as_str(),
        &model,
        None,
        None,
    )?;

    let provider = build_provider_for_channel(&channel)
        .map_err(|e| format!("构建 provider 失败：{e}"))?;

    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let collector = tokio::spawn(async move {
        let mut answer = String::new();
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::TextDelta { delta, .. } = &ev {
                answer.push_str(delta);
            }
        }
        answer
    });

    let ctx = ToolContext {
        run_id: run_id.clone(),
    };
    let summary_result = run_agent(provider, None, registry.clone(), req, ctx, tx).await;
    let answer = collector
        .await
        .map_err(|e| format!("collector join 失败：{e}"))?;

    match summary_result {
        Ok(summary) => {
            let outcome = answer.chars().take(500).collect::<String>();
            let _ = observer::finalize_with_context(&app, &summary, None, None, Some(&outcome));
            Ok(run_id)
        }
        Err(e) => {
            let err_msg = e.to_string();
            let _ = observer::finalize_failure(&app, &run_id, &err_msg);
            Err(err_msg)
        }
    }
}

// ====== Prompt =====================================================

const NEWS_REVIEW_INSTRUCTIONS: &str = r#"
# News review 模式

一批刚入库的资讯被攒批送到你这里。你的任务是**关联当前自选股 + 持仓**判断影响，然后决定是否需要行动。

## 决策路径（按优先级）
1. **资讯影响当前 open position**（live 或 watch）：
   - 利空 / 假设破 → `close_position(reason=invalidated, ...)` 或 `adjust_position` 缩小止损 / 移止盈
   - 利好确认 → `adjust_position` 上移止盈（让赚的多飞一会）
2. **资讯影响当前自选股但未持仓**：
   - 强 actionable + 风险可控 → `open_position(kind=live, ...)`（盘中）或 `kind=watch`（盘外/集合竞价）
   - 信号偏弱但值得跟 → 现有持仓中调，无需新仓
3. **资讯提到新的潜力标的且不在自选股**：
   - 短期值得跟踪 → `add_to_watchlist(code, reason)`，下次扫盘自然纳入
   - 不值得跟（一次性 / 噪音）→ 略过
4. **看完无 actionable**：用一句话总结"这批 news 没有触动 → 跳过"，不要硬找事

## 行为约束
- **不要每条 news 都开仓** —— 大多数 news 应该是 no_action 或 add_to_watchlist
- **不要重复 open** —— 同 code 已有 open position 时，调用 adjust 而非新开
- open_position 必须填 direction / take_profit / stop_loss / invalidation_signals / signals_used / reasoning
- 不在交易时段时只允许 kind=watch（aggregate 会自动拦截 live 盘外建仓）
- 你写完最后用 ≤500 字符总结"我做了什么"（落到 episode.outcome_summary，供审计）
"#;

fn build_news_review_context(
    app: &AppHandle,
    batch_id: &str,
    trigger_reason: &str,
    queued_remaining: u64,
    news_items: &[crate::domain::news::NewsItem],
) -> String {
    let mut s = String::with_capacity(8192);

    // ===== 触发上下文 =====
    s.push_str(&format!(
        "# News batch review\n\
         batch_id: {batch_id}\n\
         trigger: {trigger_reason}\n\
         本批 news 数: {n}\n\
         队列剩余 pending: {queued_remaining}\n\n",
        n = news_items.len()
    ));
    if queued_remaining > 0 {
        s.push_str(&format!(
            "⚠️ 队列还有 {queued_remaining} 条 pending news 待消化——如果本批 actionable 不多，可以快速过；如果某些 news 涉及强信号请优先处理。\n\n"
        ));
    }

    // ===== 当前持仓 =====
    let repo = crate::infrastructure::account::PositionRepo::new(app.clone());
    let open_positions = repo.list_open().unwrap_or_default();
    s.push_str(&format!("## 当前 open positions（{}条）\n", open_positions.len()));
    if open_positions.is_empty() {
        s.push_str("（无持仓 / 无 watch）\n\n");
    } else {
        for p in open_positions.iter().take(20) {
            let tp = p.take_profit.as_ref().map(|y| y.value());
            let sl = p.stop_loss.as_ref().map(|y| y.value());
            s.push_str(&format!(
                "- {} ({}) kind={} direction={} shares={} avg_cost={:.2} take_profit={:?} stop_loss={:?}\n  reasoning: {}\n",
                p.name,
                p.code.as_str(),
                p.kind.as_str(),
                p.direction.as_str(),
                p.current_shares.value(),
                p.avg_entry_price.value(),
                tp,
                sl,
                truncate(&p.reasoning, 120),
            ));
        }
        s.push('\n');
    }

    // ===== 当前自选股 =====
    let watchlist = crate::infrastructure::account::watchlist::list_strings();
    s.push_str(&format!("## 自选股（{}）\n", watchlist.len()));
    if watchlist.is_empty() {
        s.push_str("（空——新的潜力标的可以用 add_to_watchlist 加入）\n\n");
    } else {
        for chunk in watchlist.chunks(10) {
            s.push_str(&format!("  {}\n", chunk.join(", ")));
        }
        s.push('\n');
    }

    // ===== 市场 overview =====
    // 需要时 agent 自己调 get_market_overview 工具拉——不强塞到 prompt 里省 token

    // ===== 本批 news 全量 =====
    s.push_str(&format!("## 本批 news（{}条，按时间倒序）\n\n", news_items.len()));
    for item in news_items.iter() {
        let summary = item.summary.as_deref().unwrap_or("(无 summary)");
        s.push_str(&format!(
            "### [{src}] {title}\n\
             id: {id}\n\
             published: {published}\n\
             link: {link}\n\
             {summary}\n\n",
            src = item.source,
            title = item.title,
            id = item.id,
            published = item.published.as_deref().unwrap_or("?"),
            link = item.link.as_deref().unwrap_or(""),
            summary = truncate(summary, 400),
        ));
    }

    s.push_str("---\n\n按 News review 模式给出决策（写工具调用 + ≤500 字符 outcome 总结）。");
    s
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
