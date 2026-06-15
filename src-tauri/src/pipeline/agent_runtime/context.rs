//! 分层 system prompt 组装 —— L1 角色纪律 / L2 投资策略 / L3 实时上下文。
//!
//! Spec: docs/design/agent-runtime-module.md §3 分层 system prompt
//!
//! - L1（所有 mode 共享、固定）：角色 + 纪律 + 本 mode 职责。`kind=System`，不可丢。
//! - L2（隔离层）：注入当前 active `InvestmentStrategy.strategy` 自然语言全文。`kind=System`，不可丢。
//! - L3（按 mode 不同）：实时上下文（本批 news / 账户 / 行情 / **当日已下单意图**）。`kind=Realtime`，可丢（压缩）。
//!
//! 工具清单 + skill 索引由 **Infra `SystemPromptBuilder`** 注入（不在这里）。

use crate::domain::agent::context::{ContextBundle, ContextContent, ContextPart, ContextPartKind};
use crate::domain::agent::runtime::{
    AgentRunMode, AgentTrade, AgentTradeStatus, InvestmentStrategy,
};

/// 「当日已下单意图」段的渲染入参（防自我打架，spec §6 / §3 mode 表）。
///
/// caller 负责采集：`trades` = 今日 AgentTrades（repo.list_trades_since(当日0点)）；
/// `active_orders_summary` / `positions_summary` = 从 `fetch_account` JSON 渲染好的摘要；
/// `remaining_quota` = 当日剩余可下单额度（额度上限 − 今日已下）。
pub struct IntradayIntentInput<'a> {
    pub trades: &'a [AgentTrade],
    pub active_orders_summary: String,
    pub positions_summary: String,
    pub remaining_quota: Option<u32>,
}

/// L3 实时上下文的一段（caller 已渲染好文本；builder 只负责包成 Realtime part）。
#[derive(Debug, Clone)]
pub struct RealtimeSection {
    pub label: String,
    pub content: String,
}

impl RealtimeSection {
    pub fn new(label: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            content: content.into(),
        }
    }
}

/// L1 固定基线纪律（所有 mode 共享）。
/// Spec: agent-runtime-module.md §分层 system prompt — L1 角色与纪律 + L1 自主工作流。
const L1_BASE: &str = "你是一名**有主见的 A 股交易者 / 投资者本人**，不是中立的分析助手。\n\
你对行情、个股、题材有**自己的判断和立场**：该看多就看多、该看空就看空、该回避就明说回避。\
用第一人称给观点（「我认为」「我会」「我不碰」「这只才是正宗龙头，那只只是跟风」），\
敢下判断、敢排座次、敢点名否定。**不要把分析摊成一堆中立选项让用户自己挑**——\
先给出你的结论和倾向，再讲支撑它的理由。用户找你是要你的**看法**，不是要一份说明书。\n\
区分两件事：**判断上要有锋芒**（敢表态、敢排序、敢否定，哪怕可能错）；\
**动作上有纪律但不优柔**（模拟盘下单/挂单**你自己拍板、直接执行，不用先问用户确认**；\
只有改投资策略才需用户确认。每笔有数据支撑的清晰理由——纪律是为了赢得更稳，不是为了不出手）。\n\
你拥有完整的工具集：fetch_quotes（行情）、fetch_account（账户）、fetch_news（资讯）、\
scan_market（市场扫描）、operate_account（下单）、update_watchlist（自选）等；\
本地资讯检索用 fetch_news，开放互联网用 web_search（搜）+ web_extract（读正文）。\n\
**联网深度调研**（如查某公司产业链 / 上下游 / 竞争格局，或多角度搜+读+综合）\
用 run_subagent fork 一个子 agent 去做（它能用 web_search/web_extract），\
让它把**整理好的结论简报**回传，避免一堆原始搜索结果污染主对话；简单查证才直接 web_search。\
复杂只读深挖（如临时复盘历史决策）同样用 run_subagent fork 只读子 agent。\n\
**【自主工作流：理解意图 → 拆解 → 取数 → 观察 → 校验 → 收口】**\n\
1. **不空承诺。** 禁止输出「我来帮你查一下」「请稍等」然后停止；需要数据就在本条回复里\
**直接发起工具调用**，拿到 <tool_result> 再下结论。\n\
2. **该拆就拆。** 多步任务（如「这只票要不要买」≈ 行情 + 基本面 + 持仓 + 资讯 + 策略对照）\
先用一两句列出步骤（可用 todo_write 登记清单并随进度更新），然后**连续执行到任务真正完成**，\
不要只做第一步就停；无依赖的工具一次回复内并发调，不分多轮。\n\
3. **取数后才分析。** 禁止在没有工具返回数据的情况下给出任何投资分析 / 结论。\n\
4. **收口前自检。** 给最终结论前自检「用户目标是否已被完整满足、关键数据是否齐」；\
缺了就继续取，而不是提前宣布完成。\n\
5. **不问许可、直接做（重要）。** 看到明显该做的下一步（继续取数、补一个维度、做盘面+基本面交叉校验、\
逐只核公司）——**直接做完再给结论**，严禁用「如果你愿意 / 要不要我继续 / 我下一步可以帮你… / 如果你要我就开始」\
这类反问把活推回给用户。**模拟盘下单/挂单不用问**——有把握就直接 operate_account 执行。\
**只有这三种情况才停下来问**：① 要改投资策略（upsert_investment_strategy，需用户确认）；\
② 真有多个互斥方向、必须用户拍板选哪个；③ 缺关键信息、不补就没法推进。\
其余一律默认「用户要的就是把这件事做完做透」，自己往下做。\n\
6. **结论先行、别教学。** 先给判断/结论，再给支撑理由；除非用户明确说「教我方法/怎么看」，否则不要长篇\
铺陈「怎么分析」的教程式内容。宁可短而有结论，不要长而在解释流程。\n\
**【这是模拟账户，本金就是拿来试错和学习的】** 不连真券商，亏损是迭代策略的**学费**，不是真金白银。\
所以**不要因为怕亏就长期空仓观望**——你的目标是：主动验证想法、尽可能把收益做高，\
并用每一笔实盘的真实结果（盈亏 / 与基准的超额）**反推策略合不合理、迭代出一套更有效的打法**。\
有了像样的理由就**敢于下手实盘验证**，而不是追求完美确定性而永远不出手；一笔亏了就当作样本，复盘它、改进它。\n\
仍要守的（这些是为了**赚得更好**，不是为了不动）：每笔交易要有数据支撑的清晰、可复盘理由；\
消息驱动先判断是否已被 price-in（别接最后一棒、别追到山顶）。\n\
**关于下单时机（重要）**：「行情过期不下单」只针对**追市价**——不要拿一个 stale 的实时价去打市价单。\
但**限价挂单（operate_account 限价单）任何时候都能用**：收盘后想布局明天，就**直接挂限价单**，\
它会留存为 pending、明天开盘按你设的价格条件自动成交。**不要因为「现在收盘了/要等明天」就什么都不做**——\
想清楚价格就直接挂上去。所有交易只经 operate_account（模拟盘，不连真券商）。";

fn l1_mode_line(mode: AgentRunMode) -> &'static str {
    match mode {
        AgentRunMode::Dialogue => "【当前模式：对话】响应用户；可下单 / 维护自选。\n\
            **禁止输出计划性文字后停止。** 当用户问行情/持仓/分析/标的相关问题时，\
            你必须**立即在同一条回复中调用工具**（如 fetch_quotes / scan_market / fetch_account / fetch_news），\
            拿到数据后给出分析结论。不要说「我来查一下」——直接查。\n\
            需临时复盘历史决策时用 run_subagent fork 一个只读子 agent（allowedTools 收紧为只读 fetch_*）拿回结论。\n\
            当用户表达新的投资偏好 / 约束 / 纪律（如调整仓位上限、止损线、集中度、风险偏好等）时，\
            在给出你的看法之外，**必须明确询问用户是否要将其写入投资策略**（经 upsert_investment_strategy），\
            得到用户明确确认后才写；未确认一律不写、不擅自改策略。",
        AgentRunMode::News => "【当前模式：news 分析】读本批 news，判断对持仓 / 自选 / 候选标的的影响。\
            大多数 news 应 no_action；要动手必须说明边际信息（为什么现在进还来得及），不追高、先判断是否已 price-in。\
            分析完本批 news 形成判断后，**必须**调用 record_analysis 声明 action / no_action + 理由\
            （含「为什么现在进还来得及 / 已 price-in」），relatedCodes 填相关标的；无论是否下单都要调一次。",
        AgentRunMode::AccountTrigger => "【当前模式：账户事件响应】止损 / 止盈命中、挂单成交 / 拒单等已发生。\
            结合原始建仓理由，决定是否平仓 / 调仓 / 调整保护条件；止损命中优先实时处置。",
        AgentRunMode::Review => "【当前模式：复盘（只读，永不下单）】客观复盘指定范围的决策链 + 账户结果 + 与基准的超额。\
            区分运气与能力；样本不足时不下绩效结论、只做过程复盘；给策略评估与建议（不自动改策略）。",
    }
}

/// 组装一次 run 的 ContextBundle（L1 + L2 + L3）。
///
/// - `strategy=None`（无 active 策略）→ 不注入 L2，并提示自动下单 mode 默认禁写交易。
/// - `realtime` 为 caller 渲染好的 L3 段（会下单的 run 必含「当日已下单意图」段，由 caller 保证）。
pub fn build_context(
    run_id: &str,
    mode: AgentRunMode,
    strategy: Option<&InvestmentStrategy>,
    realtime: Vec<RealtimeSection>,
) -> ContextBundle {
    let mut bundle = ContextBundle::new(run_id);

    // L1：角色纪律 + mode 职责（不可丢）。
    bundle.system_parts.push(sys_part(format!(
        "{L1_BASE}\n{}",
        l1_mode_line(mode)
    )));

    // L2：投资策略（隔离层，不可丢）。
    match strategy {
        Some(s) => bundle.system_parts.push(sys_part(format!(
            "【投资策略 v{}】\n{}",
            s.version, s.strategy
        ))),
        None => bundle.system_parts.push(sys_part(
            "【投资策略】当前未设置 active 策略；自动下单默认禁用，仅可读取数据 / 回答 / 提示用户先设置策略。"
                .to_string(),
        )),
    }

    // L3：实时上下文（可丢，压缩时优先清理）。
    for sec in realtime {
        bundle.system_parts.push(ContextPart {
            kind: ContextPartKind::Realtime,
            content: ContextContent::Text(format!("【{}】\n{}", sec.label, sec.content)),
            freshness: None,
            token_estimate: None,
            droppable: true,
        });
    }

    bundle
}

/// 渲染「当日已下单意图」L3 段（防自我打架，spec §6）。
///
/// 会下单的 run（dialogue/news/account_trigger）**强制**注入此段，让 fresh run 看见自己当日
/// 已下的单 / 活跃挂单 / 持仓 / 剩余额度，避免重复建仓或自我对打。即使空仓空单也注入（显式声明"无"）。
pub fn build_intraday_intents_section(input: IntradayIntentInput) -> RealtimeSection {
    let mut lines = String::new();

    if input.trades.is_empty() {
        lines.push_str("· 今日尚无已下 AgentTrade。\n");
    } else {
        lines.push_str("今日已下 AgentTrades：\n");
        for t in input.trades {
            lines.push_str(&format!(
                "· [{}] {} —— 理由：{}\n",
                trade_state_label(t),
                t.account_input_summary,
                t.reason
            ));
        }
    }

    lines.push_str(&format!(
        "活跃挂单：{}\n持仓：{}\n",
        blank_as_none(&input.active_orders_summary),
        blank_as_none(&input.positions_summary),
    ));
    match input.remaining_quota {
        Some(q) => lines.push_str(&format!("剩余可下单额度：{q} 单", q = q)),
        None => lines.push_str("剩余可下单额度：未知"),
    }

    RealtimeSection::new("当日已下单意图", lines)
}

/// AgentTrade 当前态的人读标签（提交中 / 已受理 / 被拒）。
fn trade_state_label(t: &AgentTrade) -> &'static str {
    match t.status {
        AgentTradeStatus::Submitting => "提交中",
        AgentTradeStatus::Settled => match &t.account_result_ref {
            Some(r) if r.accepted => "已受理",
            Some(_) => "被拒",
            None => "已结算",
        },
    }
}

fn blank_as_none(s: &str) -> &str {
    if s.trim().is_empty() {
        "无"
    } else {
        s
    }
}

fn sys_part(text: String) -> ContextPart {
    ContextPart {
        kind: ContextPartKind::System,
        content: ContextContent::Text(text),
        freshness: None,
        token_estimate: None,
        droppable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::runtime::StrategyStatus;
    use chrono::Utc;

    fn strat(version: u32) -> InvestmentStrategy {
        InvestmentStrategy {
            strategy_id: "s".into(),
            version,
            strategy: "价值优先，单票不超 25%，跌破成本 8% 止损。".into(),
            status: StrategyStatus::Active,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn text_of(p: &ContextPart) -> String {
        match &p.content {
            ContextContent::Text(s) => s.clone(),
            ContextContent::Json(v) => v.to_string(),
        }
    }

    #[test]
    fn builds_l1_l2_l3_in_order_with_droppability() {
        let b = build_context(
            "r1",
            AgentRunMode::News,
            Some(&strat(3)),
            vec![
                RealtimeSection::new("本批 news", "1) 某公司利好公告…"),
                RealtimeSection::new("当日已下单意图", "已开仓 600519 ×100；剩余可下单 19 单"),
            ],
        );
        // L1 + L2 + 2×L3 = 4 parts
        assert_eq!(b.system_parts.len(), 4);
        // L1：System、不可丢、含 mode 职责。
        assert_eq!(b.system_parts[0].kind, ContextPartKind::System);
        assert!(!b.system_parts[0].droppable);
        assert!(text_of(&b.system_parts[0]).contains("news 分析"));
        // 风险偏好:模拟账户、敢实盘验证(不再是"宁可空仓"的损失厌恶)。
        assert!(text_of(&b.system_parts[0]).contains("模拟账户"));
        assert!(text_of(&b.system_parts[0]).contains("敢于下手实盘验证"));
        // L2：System、不可丢、含策略版本 + 文本。
        assert_eq!(b.system_parts[1].kind, ContextPartKind::System);
        assert!(!b.system_parts[1].droppable);
        assert!(text_of(&b.system_parts[1]).contains("投资策略 v3"));
        assert!(text_of(&b.system_parts[1]).contains("跌破成本 8% 止损"));
        // L3：Realtime、可丢。
        assert_eq!(b.system_parts[2].kind, ContextPartKind::Realtime);
        assert!(b.system_parts[2].droppable);
        assert!(text_of(&b.system_parts[3]).contains("当日已下单意图"));
    }

    #[test]
    fn no_strategy_injects_disabled_notice() {
        let b = build_context("r1", AgentRunMode::Dialogue, None, vec![]);
        assert_eq!(b.system_parts.len(), 2); // L1 + L2(占位)
        assert!(text_of(&b.system_parts[1]).contains("未设置 active 策略"));
        assert!(text_of(&b.system_parts[1]).contains("自动下单默认禁用"));
    }

    #[test]
    fn review_mode_l1_is_readonly() {
        let b = build_context("r1", AgentRunMode::Review, Some(&strat(1)), vec![]);
        assert!(text_of(&b.system_parts[0]).contains("只读，永不下单"));
    }

    #[test]
    fn account_trigger_mode_l1_realtime_handling() {
        let b = build_context("r1", AgentRunMode::AccountTrigger, Some(&strat(1)), vec![]);
        assert!(text_of(&b.system_parts[0]).contains("止损命中优先实时处置"));
    }

    fn trade(summary: &str, reason: &str, status: AgentTradeStatus, accepted: Option<bool>) -> AgentTrade {
        use crate::domain::agent::runtime::AccountResultRef;
        AgentTrade {
            trade_id: "td".into(),
            run_id: "r".into(),
            client_order_id: "co".into(),
            strategy_version: Some(1),
            reason: reason.into(),
            account_input_summary: summary.into(),
            status,
            account_result_ref: accepted.map(|a| AccountResultRef {
                accepted: a,
                ..Default::default()
            }),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn intraday_intents_empty_still_declares_no_trades() {
        let sec = build_intraday_intents_section(IntradayIntentInput {
            trades: &[],
            active_orders_summary: String::new(),
            positions_summary: String::new(),
            remaining_quota: Some(20),
        });
        assert_eq!(sec.label, "当日已下单意图");
        assert!(sec.content.contains("今日尚无已下 AgentTrade"));
        assert!(sec.content.contains("活跃挂单：无"));
        assert!(sec.content.contains("持仓：无"));
        assert!(sec.content.contains("剩余可下单额度：20 单"));
    }

    #[test]
    fn intraday_intents_renders_trade_states() {
        let trades = vec![
            trade("买入 600519 ×100", "价值开仓", AgentTradeStatus::Settled, Some(true)),
            trade("买入 000001 ×500", "追涨", AgentTradeStatus::Settled, Some(false)),
            trade("卖出 600036 ×200", "止盈", AgentTradeStatus::Submitting, None),
        ];
        let sec = build_intraday_intents_section(IntradayIntentInput {
            trades: &trades,
            active_orders_summary: "600036 卖单挂单中".into(),
            positions_summary: "600519 ×100".into(),
            remaining_quota: None,
        });
        assert!(sec.content.contains("[已受理] 买入 600519 ×100"));
        assert!(sec.content.contains("[被拒] 买入 000001 ×500"));
        assert!(sec.content.contains("[提交中] 卖出 600036 ×200"));
        assert!(sec.content.contains("剩余可下单额度：未知"));
    }
}
