//! Agent prompt 构建 + 输出解析。
//!
//! 完整 Rust 端口自 src/lib.ts。包括：
//! - 三个 prompt builder：briefing / review / chat reply
//! - 两个 JSON parser：parse_briefing / parse_review
//! - 全部 formatters：market / quotes / positions / memory / learning / records 等
//! - normalizers：把 Agent 输出的 JSON 校正成稳定 Rust 结构
//! - tradeCall → AnalysisResult 转换（briefing 触发模拟开仓时用）

use crate::agent_io::{
    AnalysisResult, BriefingHighlight, BriefingResult, BriefingSignal, BriefingTradeCall,
    EntryStrategy, ExitPlan, ExternalResearch, InvestorMemory, InvestorMemoryUpdate,
    LearningProfile, PositionEvent, PositionSizing, ReviewResult, SimulatedPosition,
    SimulatedTradePlan, StoredAnalysisRecord, StoredNewsItem, TargetStock,
};
use crate::domain::quotes::{MarketOverview, StockQuote};
use serde_json::Value;
use std::collections::HashMap;

pub(crate) const AGENT_IDENTITY: &str = include_str!("identity.md");

/// Chat 模式下追加在 identity 之后的 system 指令——指导 agent 怎么用工具、
/// 怎么管记忆、怎么在 chat 里也保持"操盘手"身份而不是退化成"咨询师"。
pub(crate) const CHAT_SYSTEM_INSTRUCTIONS: &str = r#"你是 GangZiTerminal 的操盘手 Agent，正在和围观你交易的用户对话。

## Chat 里你仍然是操盘手
**chat 是用来讲思路 / 接受指导的，不是用来"等你拍板我才动手"的**。

- 自己判断当前价位、市场环境、持仓状态适合开/加/减/平仓 → **直接调对应写工具下单**，然后用自然语言汇报"我在 X 价开了 Y 股、止损 Z、理由 W"。
- 信心不足、机会不够清晰 → 直说"我不开，因为 X"——这本身就是一个决策，不是把球踢给用户。
- "等用户确认再动手"是反模式，**禁止**：
  - ❌ "如果你要我执行，我会按 100 股做" → 应该是 ✅ "我按 100 股开了，止损 34.5"
  - ❌ "建议开仓 100 股" → 应该是 ✅ "我开 100 股，理由是..."
  - ❌ "你看要不要我下单" → 应该是 ✅ 直接下单 + 解释 或 ✅ "我不下，因为..."
- 用户明确指令（"帮我建 X" / "平 Y" / "调止损到 Z"）→ 同样直接执行 + 汇报。
- 写工具失败时（涨跌停 / T+1 / 资金不足 / code 找不到 / 接口异常）→ 如实告诉用户哪条规则不通 + 下一步可行方案；**绝不假装下单成功**。

## 回答方式
- 直接用 Markdown 回答。**不要输出 JSON 包裹整个回答**。
- 直接、克制、可复盘。信息不足直说"需要查证"，不要编造。
- 已执行的决策用陈述语气（"我开了 100 股紫金、止损 34.5"），不要用建议语气（"建议开 100 股"）。

## 工具使用
- 需要实时行情、K 线、技术指标、大盘概况、持仓详情、历史新闻时，**必须**调对应工具——不要凭印象。
- 决策前先调 `get_account` 看现金 / 持仓 / 总盈亏，再决定动作。
- 对话过程中得出值得长期记忆的判断（关注主题、新原则、风险偏好变化、近期 insight）→ 调 `update_memory`。
- 如果意识到既有记忆不再适用 / 已被反例推翻 → 调 `remove_memory`（按字段名 + 精确字符串匹配）。
- 单条记忆 ≤ 80 字。记忆是流动判断快照，不是积累的标语清单——更新和删除都自然发生。

## 不要做
- 不要把判断包装成 JSON 给我解析——直接说人话。
- 不要在文本里塞"memoryUpdates: {...}"这种结构——用工具去写。
- 不要重复问用户已经说过的信息——上文+长期记忆+持仓档案都给你了。
- 不要写"如果你愿意，我下一步可以帮你 X / Y / Z"这种揽活尾巴——你是操盘手，决策已经做了或不做，没什么可"如果"的。"#;

// ====== Prompt 输入打包结构 ======

pub struct BriefingPromptInput<'a> {
    pub pending_news: &'a [StoredNewsItem],
    pub market_overview: Option<&'a MarketOverview>,
    pub watchlist_quotes: &'a [StockQuote],
    pub simulated_positions: &'a [SimulatedPosition],
    pub position_events: &'a HashMap<String, Vec<PositionEvent>>,
    pub recent_briefings: &'a [RecentBriefing],
    pub investor_memory: Option<&'a InvestorMemory>,
    pub learning_profile: Option<&'a LearningProfile>,
    /// 行情拉取异常的"数据可用性"提示（None = 正常）。pipeline 计算后传入。
    pub quotes_availability: Option<&'a str>,
}

pub struct ReviewPromptInput<'a> {
    pub record: &'a StoredAnalysisRecord,
    pub market_overview: Option<&'a MarketOverview>,
    pub watchlist_quotes: &'a [StockQuote],
    pub simulated_positions: &'a [SimulatedPosition],
    pub position_events: &'a HashMap<String, Vec<PositionEvent>>,
    pub investor_memory: Option<&'a InvestorMemory>,
    pub learning_profile: Option<&'a LearningProfile>,
    pub recent_records: &'a [StoredAnalysisRecord],
    pub allow_external_research: bool,
    pub quotes_availability: Option<&'a str>,
}

/// Chat 的"半静态" system 上下文输入——只包括投资者长期记忆 + 学习画像。
/// 这部分跨 chat turn 基本不变（memory 写入是 mutation 工具触发的，单次 request
/// 内部稳定），适合放进 system block 末尾打 cache_control，让多轮 chat 共用 cache prefix。
pub struct ChatSystemContextInput<'a> {
    pub investor_memory: Option<&'a InvestorMemory>,
    pub learning_profile: Option<&'a LearningProfile>,
}

/// Chat 的"易变" 动态上下文输入——市场快照、持仓、最近 briefing。
/// 这部分每次 chat 都不一样（盘口随时变、持仓随交易变），不进 cache prefix，
/// 作为 messages 列表里的第一条 user 消息塞给模型。
pub struct ChatDynamicContextInput<'a> {
    pub recent_briefings: &'a [RecentBriefing],
    pub market_overview: Option<&'a MarketOverview>,
    pub simulated_positions: &'a [SimulatedPosition],
    pub position_events: &'a HashMap<String, Vec<PositionEvent>>,
    /// 行情拉取异常的"数据可用性"提示（None = 正常）。pipeline 计算后传入。
    pub quotes_availability: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct RecentBriefing {
    pub created_at: String,
    pub summary_md: String,
}

// ====== Builders ======

pub fn build_briefing_prompt(input: &BriefingPromptInput) -> String {
    let news_list = input
        .pending_news
        .iter()
        .enumerate()
        .map(|(i, item)| {
            format!(
                "{}. id={}｜来源={}｜时间={}｜标题={}｜摘要={}",
                i + 1,
                item.id,
                item.source,
                item.published.as_deref().unwrap_or("未知"),
                item.title,
                truncate_chars(item.summary.as_deref().unwrap_or(""), 240),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"你是 GangZiTerminal 的长期主 Agent，扮演一名专业投资者。你不分析每一条新闻，而是周期性地把一批待消化资讯放在一起看，结合行情、自选股、模拟持仓和历史记忆，输出一份"市场简报"。

输出必须是 JSON，不要 Markdown 包裹，不要额外解释：
{{
  "headline": "8-16 字的精炼标题，概括本批资讯的核心叙事或主线判断。直接说出主题/方向，不要套话（如\"今日要点\"、\"市场观察\"），更不要用引号或括号。",
  "summaryMd": "给用户阅读的 Markdown 简报。结构推荐：今日要点 / 主线信号 / 操作建议 / 待验证假设。要直接、克制、可复盘。",
  "signals": [
    {{
      "theme": "主线主题",
      "direction": "bullish | bearish | mixed | watch",
      "evidence": "为什么"
    }}
  ],
  "tradeCalls": [
    {{
      "code": "A股代码（可省略）",
      "name": "公司或主题名",
      "action": "buy | watch | avoid | sell_if_holding",
      "thesis": "交易假设",
      "triggerCondition": "进场触发",
      "invalidationCondition": "假设失效条件",
      "stopLoss": "止损（可省略）",
      "takeProfit": "止盈（可省略）",
      "timeStop": "时间止损（可省略）",
      "riskLevel": "low | medium | high",
      "confidence": 0.5
    }}
  ],
  "coveredNewsIds": ["你这次真正消化的资讯 id；剩下的下次再看"],
  "nextFocus": ["下一阶段需要重点跟踪的事情"],
  "highlight": null,
  "memoryUpdates": {{
    "focusThemes": [],
    "knownBiases": [],
    "investmentPrinciples": [],
    "watchQuestions": [],
    "recentInsights": []
  }},
  "memoryRemovals": {{
    "focusThemes": [],
    "watchQuestions": [],
    "recentInsights": []
  }}
}}

约束：
- coveredNewsIds 只放你真的考虑过、归入了简报的 id。其余资讯下一次再说，不要为了凑数勉强塞。
- tradeCalls 不是必须项；信号弱、风险高、噪声大时给空数组。
- 任何 buy 类 tradeCall 必须给出 triggerCondition + invalidationCondition；这是模拟训练，不是真实交易。
- summaryMd 是给用户看的；不要重复 signals/tradeCalls 的字段，要写有判断、有条理、有取舍。
- memoryUpdates 仅写本批资讯+对话能确实推断出的内容，没有就给空对象。
- **记忆是流动的判断快照，不是积累的标语清单**。复查上面"投资者长期记忆"里的现有条目，把不再反映当下判断、已被后续事实推翻、或当时偏狭隘的条目通过 memoryRemovals 字段（按字段名 + 精确字符串匹配）显式删除。新增和删除可以同时发生。单条不超过 80 字。

关于 highlight（"划重点"——投资者朋友戳一下用户，分享一个值得多想一秒的洞察）：
- highlight 默认 null。只有本批里真的有用户**没自己看出来的连接、机制、反共识或关键反差**才填。宁缺毋滥，80% 以上的 briefing 应该 null。
- 结构 {{ "importance": "high" | "medium", "message": "..." }}。high 会作为独立消息推送给用户；medium 仅内部标记。
- message 30-80 字，**对话式**——核心要求：给用户一个"信息或视角"，**不是 narrate 自己的动作**。
  "X 这条我会盯"是无效——你盯什么用户不关心。
  "X 这事关键不是 A 而是 B 因为 C"才有效——给了用户多想一步的角度。
- 切入点要变化，不要每条都"<标的>这条我..."。可以从机制（"这事的传导链是..."）、从对比（"和上次不一样的是..."）、从反共识（"市场把它当 X，但实际是 Y..."）、从触发条件（"如果 A 真落地，影响的是 B 不是 C..."）入手。

好例子（注意每条切入方式都不同）：
- "欧盟限项目融资如果真落地，伤的是欧洲订单入口本身，不是短线情绪——阳光、锦浪这种欧洲敞口大的得先看在手订单结构。"
- "华勤这次和单纯算力概念股不一样：超节点 Q2 真发货、数据业务过百亿，是有交付的算力。明天看市场认不认这个差异。"
- "存储别只看价：龙头今天放量滞涨，是高位拥挤的典型信号；先看下一根 K 的承接，再决定加不加。"
- "降息预期对成长理论利好，但今天 A 股没兑现——流动性叙事和盘面在脱钩，关键是资金会不会真回流。"

不要写：
- "X 这条我会马上看 / 重点盯" —— 在 narrate 自己的动作，对用户没信息
- "本次提醒 / 识别到 / 经分析" —— 套话
- 复述整篇 briefing —— 那是 summaryMd 的活
- 引号、括号包裹整段

待消化资讯（共 {count} 条）：
{news_list}
{quotes_availability}
当前市场上下文：
{market}

自选股行情：
{quotes}

当前模拟账户持仓（含事件链——开仓/复盘/调仓的完整推理路径）：
{positions}

近期 briefing 摘要（不要重复说）：
{recent_briefings}

学习档案：
{learning}

投资者长期记忆：
{memory}"#,
        count = input.pending_news.len(),
        news_list = news_list,
        quotes_availability = format_availability_block(input.quotes_availability),
        market = format_market(input.market_overview),
        quotes = format_quotes(input.watchlist_quotes),
        positions = format_positions(input.simulated_positions, input.position_events),
        recent_briefings = format_recent_briefings(input.recent_briefings),
        learning = format_learning_profile(input.learning_profile),
        memory = format_investor_memory(input.investor_memory),
    )
}

/// 行情可用性提示——拼到 prompt 顶部。None 时返回空串（不打扰）；有内容时
/// 前后加换行，让它在 prompt 里独立成块。
fn format_availability_block(availability: Option<&str>) -> String {
    match availability {
        Some(text) if !text.trim().is_empty() => format!("\n\n{}\n", text),
        _ => String::new(),
    }
}

/// Chat pipeline 用的 user-message 文本——包含本轮上下文 + 用户输入。
/// 上下文不进 system 块（因为它每轮都变，进 system 缓存命中率低）。
/// 构造 chat 的 system 上下文文本（投资者长期记忆 + 学习画像）。
///
/// 用于和 identity / 系统指令一起打包成 system block，且这一段会打 cache_control
/// 形成 cacheable prefix——只要用户两轮之间没调 update_memory / remove_memory，
/// 这段文本就不变，多轮 chat 共用同一份 prompt cache。
pub fn build_chat_system_context(input: &ChatSystemContextInput) -> String {
    format!(
        r#"投资者长期记忆（持续累积，agent 可通过 update_memory / remove_memory 工具更新）：
{memory}

学习档案（基于历史 analysis_records / positions 派生，只读）：
{learning}"#,
        memory = format_investor_memory(input.investor_memory),
        learning = format_learning_profile(input.learning_profile),
    )
}

/// 构造 chat 的动态上下文文本（市场快照 + 模拟持仓 + 最近 briefing）。
///
/// 用于作为 messages[0] 的 user 消息——每次 chat turn 都重建（盘口和持仓
/// 随时变化），不进 prompt cache 视野；不影响后续 chat 历史的 cache hit。
pub fn build_chat_dynamic_context(input: &ChatDynamicContextInput) -> String {
    format!(
        r#"以下是本次对话开始时的实时上下文，仅作参考——若需要更精准的盘口或 K 线，请用对应工具拉取：
{quotes_availability}
当前市场上下文：
{market}

当前模拟账户持仓（含事件链）：
{positions}

最近 briefing 摘要：
{recent_briefings}"#,
        quotes_availability = format_availability_block(input.quotes_availability),
        market = format_market(input.market_overview),
        positions = format_positions(input.simulated_positions, input.position_events),
        recent_briefings = format_recent_briefings(input.recent_briefings),
    )
}

pub fn build_review_prompt(input: &ReviewPromptInput) -> String {
    let original_analysis_json = serde_json::to_string_pretty(&input.record.result)
        .unwrap_or_else(|_| "无法序列化原始分析".to_string());
    let external = if input.allow_external_research {
        "search 模式已开启。必要时搜索后续公告、政策、行业数据或主流财经报道，并把证据写入 newsFollowUp/evidence。"
    } else {
        "不要使用外部搜索，只基于给定上下文复盘。"
    };

    format!(
        r#"现在你的任务是**复盘**——以"外部审稿人"的视角审判一条你（或过去的你）发出的交易假设。

复盘不是重新预测，也不是为原假设辩护。即便这是你之前的判断，请假装你不认识发出这条假设的人，专挑漏洞。

请基于原始假设、验证清单、当前市场和持仓，输出结构化复盘。可以使用搜索补充后续公开信息（公告、政策、行业数据、主流财经报道），但必须区分"原始判断"和"后续证据"。

输出必须是 JSON，不要 Markdown 包裹：
{{
  "summary": "复盘核心结论",
  "thesisStatus": "validated | invalidated | watching | inconclusive",
  "confidence": 0.5,
  "evidence": [],
  "priceAction": [],
  "newsFollowUp": [],
  "checklistReview": [],
  "mistakes": [],
  "nextActions": [],
  "learningUpdate": "可复用的判断方法或错误模式",
  "nextReviewAt": "可选，ISO 时间"
}}

判断标准：
- validated：原始逻辑被后续事实/价格行为明显支持。
- invalidated：核心假设被证伪或价格明显反向。
- watching：方向未证伪但仍缺关键验证。
- inconclusive：信息不足。
- 不要因模拟账户盈亏单独判断对错；必须回到原始假设和验证清单。

原始资讯：
{title}
来源：{source}
时间：{published}

原始分析：
{original_analysis}
{quotes_availability}
当前市场上下文：
{market}

相关行情快照：
{quotes}

当前模拟账户持仓（含事件链）：
{positions}

学习档案：
{learning}

投资者长期记忆：
{memory}

近期记录摘要：
{recent_records}

外部扩展消息面：{external}"#,
        title = input.record.item.title,
        source = input.record.item.source,
        published = input.record.item.published.as_deref().unwrap_or("未知"),
        original_analysis = original_analysis_json,
        quotes_availability = format_availability_block(input.quotes_availability),
        market = format_market(input.market_overview),
        quotes = format_quotes(input.watchlist_quotes),
        positions = format_positions(input.simulated_positions, input.position_events),
        learning = format_learning_profile(input.learning_profile),
        memory = format_investor_memory(input.investor_memory),
        recent_records = format_recent_records(input.recent_records),
        external = external,
    )
}

// ====== Parsers ======

pub fn parse_briefing(raw: &str) -> Result<BriefingResult, String> {
    let json_text = extract_json_object(raw).ok_or_else(|| "未找到 JSON 对象".to_string())?;
    let value: Value =
        serde_json::from_str(&json_text).map_err(|e| format!("briefing JSON 解析失败：{e}"))?;
    let summary_md = value
        .get("summaryMd")
        .and_then(Value::as_str)
        .ok_or_else(|| "briefing 缺少 summaryMd 字段。".to_string())?
        .to_string();
    let signals = normalize_signals(value.get("signals"));
    let trade_calls = normalize_trade_calls(value.get("tradeCalls"));
    Ok(BriefingResult {
        headline: normalize_headline(value.get("headline"), &signals, &trade_calls),
        summary_md,
        signals,
        trade_calls,
        covered_news_ids: as_string_list(value.get("coveredNewsIds"), Vec::new()),
        next_focus: as_string_list(value.get("nextFocus"), Vec::new()),
        highlight: normalize_highlight(value.get("highlight")),
        memory_updates: normalize_memory_update(value.get("memoryUpdates")),
        memory_removals: normalize_memory_update(value.get("memoryRemovals")),
    })
}

// parse_chat_reply 已删除——chat 不再走"输出 JSON 我解析"协议。
// 现在 agent 直接 Markdown 回复 + 通过 update_memory/remove_memory 工具写记忆。

pub fn parse_review(raw: &str) -> Result<ReviewResult, String> {
    let json_text = extract_json_object(raw).ok_or_else(|| "未找到 JSON 对象".to_string())?;
    let value: Value =
        serde_json::from_str(&json_text).map_err(|e| format!("review JSON 解析失败：{e}"))?;

    let required = [
        "summary",
        "thesisStatus",
        "evidence",
        "checklistReview",
        "learningUpdate",
    ];
    let mut missing = Vec::new();
    for field in required {
        let v = value.get(field);
        let absent = match v {
            None | Some(Value::Null) => true,
            Some(Value::String(s)) => s.is_empty(),
            Some(Value::Array(a)) => a.is_empty(),
            _ => false,
        };
        if absent {
            missing.push(field);
        }
    }
    if !missing.is_empty() {
        return Err(format!("复盘结果缺少核心字段：{}。", missing.join("、")));
    }
    Ok(normalize_review(&value))
}

// ====== Briefing tradeCall → AnalysisResult ======

pub fn trade_call_to_analysis_result(
    call: &BriefingTradeCall,
    briefing_summary: &str,
) -> AnalysisResult {
    let summary = if call.thesis.is_empty() {
        format!("{} {}", call.name, call.action)
    } else {
        call.thesis.clone()
    };
    let related = call
        .code
        .as_ref()
        .map(|c| vec![c.clone()])
        .unwrap_or_default();
    let impact = match call.action.as_str() {
        "buy" => "positive",
        "sell_if_holding" | "avoid" => "negative",
        _ => "neutral",
    }
    .to_string();
    let decision = match call.action.as_str() {
        "buy" => "bullish",
        "sell_if_holding" => "bearish",
        _ => "watch",
    }
    .to_string();
    let suitability = match call.risk_level.as_str() {
        "low" => "high",
        "medium" => "medium",
        _ => "low",
    }
    .to_string();

    AnalysisResult {
        summary,
        related_stocks: related,
        key_facts: vec![truncate_chars(briefing_summary, 280)],
        sectors: Vec::new(),
        themes: Vec::new(),
        impact,
        confidence: clamp(call.confidence, 0.0, 1.0),
        time_horizon: "short".into(),
        reasoning: vec![if call.thesis.is_empty() {
            "briefing 给出的交易假设。".to_string()
        } else {
            call.thesis.clone()
        }],
        risks: vec![if call.invalidation_condition.is_empty() {
            "若假设失效条件命中则放弃。".to_string()
        } else {
            call.invalidation_condition.clone()
        }],
        verification_checklist: [
            &call.trigger_condition[..],
            &call.invalidation_condition[..],
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect(),
        external_research: ExternalResearch {
            used: false,
            queries: Vec::new(),
            findings: vec!["briefing 不单独执行外部检索。".into()],
            sources: Vec::new(),
        },
        learning_notes: "本条交易假设来自批量 briefing；按触发/失效条件追踪。".into(),
        decision,
        trade_plan: SimulatedTradePlan {
            action: call.action.clone(),
            suitability,
            target_stocks: vec![TargetStock {
                name: call.name.clone(),
                code: call.code.clone(),
                reason: call.thesis.clone(),
                priority: 1,
            }],
            entry_strategy: EntryStrategy {
                style: "event_follow".into(),
                trigger_condition: if call.trigger_condition.is_empty() {
                    "等待信号确认。".into()
                } else {
                    call.trigger_condition.clone()
                },
                invalidation_condition: if call.invalidation_condition.is_empty() {
                    "假设证伪条件未给出。".into()
                } else {
                    call.invalidation_condition.clone()
                },
            },
            position_sizing: PositionSizing {
                suggested_weight: "0%-5%".into(),
                max_loss_per_trade: "不超过模拟账户 1%".into(),
                reason: call.thesis.clone(),
            },
            exit_plan: ExitPlan {
                take_profit_condition: call
                    .take_profit
                    .clone()
                    .unwrap_or_else(|| "达到验证目标或涨幅过快时分批止盈。".into()),
                stop_loss_condition: call
                    .stop_loss
                    .clone()
                    .unwrap_or_else(|| "跌破入场逻辑关键位或事件证伪。".into()),
                time_stop: call
                    .time_stop
                    .clone()
                    .unwrap_or_else(|| "3-5 个交易日仍未验证则复盘退出。".into()),
            },
            risk_level: call.risk_level.clone(),
            confidence: clamp(call.confidence, 0.0, 1.0),
            why_not_buy_now: if call.action == "buy" {
                Vec::new()
            } else {
                vec!["briefing 未给出立即买入信号。".into()]
            },
        },
    }
}

// ====== Normalizers ======

fn normalize_signals(value: Option<&Value>) -> Vec<BriefingSignal> {
    let arr = match value.and_then(Value::as_array) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let allowed = ["bullish", "bearish", "mixed", "watch"];
    arr.iter()
        .take(12)
        .map(|entry| {
            let theme = entry
                .get("theme")
                .and_then(Value::as_str)
                .unwrap_or("未命名信号")
                .to_string();
            let direction = entry
                .get("direction")
                .and_then(Value::as_str)
                .filter(|d| allowed.contains(d))
                .unwrap_or("watch")
                .to_string();
            let evidence = entry
                .get("evidence")
                .and_then(Value::as_str)
                .unwrap_or("未给出证据。")
                .to_string();
            BriefingSignal {
                theme,
                direction,
                evidence,
            }
        })
        .collect()
}

fn normalize_trade_calls(value: Option<&Value>) -> Vec<BriefingTradeCall> {
    let arr = match value.and_then(Value::as_array) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let actions = ["buy", "watch", "avoid", "sell_if_holding"];
    let risks = ["low", "medium", "high"];
    arr.iter()
        .take(8)
        .map(|entry| {
            let code = entry
                .get("code")
                .and_then(Value::as_str)
                .map(str::to_string);
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| code.clone().unwrap_or_else(|| "未命名标的".into()));
            let action = entry
                .get("action")
                .and_then(Value::as_str)
                .filter(|a| actions.contains(a))
                .unwrap_or("watch")
                .to_string();
            let confidence = entry
                .get("confidence")
                .and_then(Value::as_f64)
                .unwrap_or(0.5);
            BriefingTradeCall {
                code,
                name,
                action,
                thesis: entry
                    .get("thesis")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                trigger_condition: entry
                    .get("triggerCondition")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                invalidation_condition: entry
                    .get("invalidationCondition")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                stop_loss: entry
                    .get("stopLoss")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                take_profit: entry
                    .get("takeProfit")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                time_stop: entry
                    .get("timeStop")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                risk_level: entry
                    .get("riskLevel")
                    .and_then(Value::as_str)
                    .filter(|r| risks.contains(r))
                    .unwrap_or("medium")
                    .to_string(),
                confidence: clamp(confidence, 0.0, 1.0),
            }
        })
        .collect()
}

fn normalize_review(value: &Value) -> ReviewResult {
    let statuses = ["validated", "invalidated", "watching", "inconclusive"];
    ReviewResult {
        summary: value
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("复盘 Agent 未返回结论。")
            .to_string(),
        thesis_status: value
            .get("thesisStatus")
            .and_then(Value::as_str)
            .filter(|s| statuses.contains(s))
            .unwrap_or("inconclusive")
            .to_string(),
        confidence: clamp(
            value
                .get("confidence")
                .and_then(Value::as_f64)
                .unwrap_or(0.5),
            0.0,
            1.0,
        ),
        evidence: as_string_list(
            value.get("evidence"),
            vec!["缺少足够证据，需要继续观察。".into()],
        ),
        price_action: as_string_list(
            value.get("priceAction"),
            vec!["未返回价格行为复盘。".into()],
        ),
        news_follow_up: as_string_list(
            value.get("newsFollowUp"),
            vec!["未返回后续消息面复盘。".into()],
        ),
        checklist_review: as_string_list(
            value.get("checklistReview"),
            vec!["未逐条复盘原验证清单。".into()],
        ),
        mistakes: as_string_list(value.get("mistakes"), vec!["暂未识别明确偏差。".into()]),
        next_actions: as_string_list(
            value.get("nextActions"),
            vec!["继续跟踪验证清单中的关键变量。".into()],
        ),
        learning_update: value
            .get("learningUpdate")
            .and_then(Value::as_str)
            .unwrap_or("把原始判断和后续证据分开记录，避免事后归因。")
            .to_string(),
        reviewed_at: value
            .get("reviewedAt")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
        next_review_at: value
            .get("nextReviewAt")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn normalize_memory_update(value: Option<&Value>) -> InvestorMemoryUpdate {
    let obj = match value.and_then(Value::as_object) {
        Some(o) => o,
        None => return InvestorMemoryUpdate::default(),
    };
    fn pick_list(obj: &serde_json::Map<String, Value>, key: &str) -> Option<Vec<String>> {
        let list = obj.get(key)?.as_array()?;
        let cleaned: Vec<String> = list
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned)
        }
    }
    let risk = obj
        .get("riskPreference")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    InvestorMemoryUpdate {
        focus_themes: pick_list(obj, "focusThemes"),
        preferred_markets: pick_list(obj, "preferredMarkets"),
        risk_preference: risk,
        learning_goals: pick_list(obj, "learningGoals"),
        known_biases: pick_list(obj, "knownBiases"),
        investment_principles: pick_list(obj, "investmentPrinciples"),
        watch_questions: pick_list(obj, "watchQuestions"),
        recent_insights: pick_list(obj, "recentInsights"),
    }
}

fn normalize_highlight(value: Option<&Value>) -> Option<BriefingHighlight> {
    let obj = value?.as_object()?;
    let importance = obj.get("importance").and_then(Value::as_str)?;
    let importance = match importance {
        "high" | "medium" => importance,
        _ => return None,
    };
    let message = obj
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if message.chars().count() < 4 {
        return None;
    }
    Some(BriefingHighlight {
        importance: importance.to_string(),
        message: truncate_chars(message, 200),
    })
}

fn normalize_headline(
    value: Option<&Value>,
    signals: &[BriefingSignal],
    trade_calls: &[BriefingTradeCall],
) -> String {
    if let Some(s) = value.and_then(Value::as_str) {
        // 去掉常见包裹符号 + 空白
        let cleaned: String = s
            .chars()
            .filter(|c| {
                !matches!(
                    c,
                    '【' | '】' | '「' | '」' | '"' | '\'' | '(' | ')' | '（' | '）'
                )
            })
            .filter(|c| !c.is_whitespace())
            .collect();
        if cleaned.chars().count() >= 2 {
            return truncate_chars(&cleaned, 24);
        }
    }
    if let Some(s) = signals.first() {
        return truncate_chars(&s.theme, 24);
    }
    if let Some(t) = trade_calls.first() {
        return truncate_chars(&t.name, 24);
    }
    "Agent 简报".to_string()
}

// ====== Formatters ======

fn format_market(market: Option<&MarketOverview>) -> String {
    let m = match market {
        Some(m) => m,
        None => return "暂无市场上下文。".into(),
    };
    let indices = m
        .indices
        .iter()
        .take(6)
        .map(|item| {
            format!(
                "{}({}) {} {}",
                item.name,
                item.code.as_str(),
                fmt_num(item.price.map(|v| v.value())),
                fmt_pct(item.change_percent)
            )
        })
        .collect::<Vec<_>>()
        .join("；");
    let ts = chrono::DateTime::from_timestamp_millis(m.timestamp.value())
        .map(|t| t.to_rfc3339())
        .unwrap_or_default();
    format!(
        "指数：{indices}\n涨跌家数：上涨 {rise}，下跌 {fall}，平盘 {flat}\n时间：{ts}",
        indices = if indices.is_empty() {
            "暂无".to_string()
        } else {
            indices
        },
        rise = m.breadth.rise,
        fall = m.breadth.fall,
        flat = m.breadth.flat,
    )
}

fn format_quotes(quotes: &[StockQuote]) -> String {
    if quotes.is_empty() {
        return "暂无自选行情。".into();
    }
    quotes
        .iter()
        .take(20)
        .map(|q| {
            format!(
                "{}({}) 最新价 {}，涨跌幅 {}，成交额 {}",
                q.name,
                q.code.as_str(),
                fmt_num(q.price.map(|v| v.value())),
                fmt_pct(q.change_percent),
                fmt_num(q.day_amount.map(|v| v.value())),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_positions(
    positions: &[SimulatedPosition],
    events_by_position: &HashMap<String, Vec<PositionEvent>>,
) -> String {
    let opens: Vec<&SimulatedPosition> = positions.iter().filter(|p| p.status == "open").collect();
    if opens.is_empty() {
        return "暂无模拟持仓。".into();
    }
    opens
        .iter()
        .take(12)
        .map(|p| {
            let mut header = format!(
                "{}({}) {}股 入场 ¥{}",
                p.name, p.code, p.shares, p.entry_price
            );
            if let Some(sl) = p.stop_loss {
                header.push_str(&format!(" 止损 ¥{:.2}", sl));
            }
            if let Some(tp) = p.take_profit {
                header.push_str(&format!(" 止盈 ¥{:.2}", tp));
            }
            header.push_str(&format!("\n  入场逻辑：{}", p.thesis));
            let events = events_by_position.get(&p.id).cloned().unwrap_or_default();
            if events.is_empty() {
                return header;
            }
            let mut sorted = events;
            sorted.sort_by(|a, b| a.occurred_at.cmp(&b.occurred_at));
            let chain: Vec<String> = sorted
                .iter()
                .rev()
                .take(10)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|event| {
                    let date: String = event.occurred_at.chars().take(10).collect();
                    let note = match event.agent_note_md.as_deref() {
                        Some(s) if !s.is_empty() => format!("｜{}", truncate_chars(s, 80)),
                        _ => String::new(),
                    };
                    format!("    ├ {} {}{}", date, event.event_kind, note)
                })
                .collect();
            format!("{}\n  事件链：\n{}", header, chain.join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn format_learning_profile(profile: Option<&LearningProfile>) -> String {
    let p = match profile {
        Some(p) => p,
        None => return "暂无学习档案。".into(),
    };
    let top_themes = if p.top_themes.is_empty() {
        "暂无".to_string()
    } else {
        p.top_themes
            .iter()
            .map(|t| format!("{}({})", t.name, t.count))
            .collect::<Vec<_>>()
            .join("；")
    };
    let mistakes = if p.common_mistakes.is_empty() {
        "暂无".to_string()
    } else {
        p.common_mistakes
            .iter()
            .map(|m| format!("{}({})", m.text, m.count))
            .collect::<Vec<_>>()
            .join("；")
    };
    format!(
        "等级 Lv.{lvl}，学习分 {score}\n记录 {total}，已复盘 {reviewed}，复盘率 {rate}%\n验证 {v}，证伪 {iv}，继续观察 {w}，证据不足 {ic}\n高频主题：{themes}\n常见偏差：{mistakes}\n当前建议：{suggest}",
        lvl = p.level,
        score = p.score,
        total = p.total_records,
        reviewed = p.reviewed_records,
        rate = (p.review_rate * 100.0).round() as i32,
        v = p.validated_count,
        iv = p.invalidated_count,
        w = p.watching_count,
        ic = p.inconclusive_count,
        themes = top_themes,
        mistakes = mistakes,
        suggest = p.focus_suggestions.join("；"),
    )
}

fn format_investor_memory(memory: Option<&InvestorMemory>) -> String {
    let m = match memory {
        Some(m) => m,
        None => return "暂无投资者长期记忆。".into(),
    };
    let join = |xs: &[String], empty: &str| {
        if xs.is_empty() {
            empty.to_string()
        } else {
            xs.join("；")
        }
    };
    format!(
        "关注主题：{ft}\n偏好市场：{pm}\n风险偏好：{rp}\n学习目标：{lg}\n常见偏差：{kb}\n投资原则：{ip}\n待复盘问题：{wq}\n近期洞察：{ri}\n更新时间：{ua}",
        ft = join(&m.focus_themes, "暂无"),
        pm = join(&m.preferred_markets, "暂无"),
        rp = if m.risk_preference.is_empty() { "未明确".into() } else { m.risk_preference.clone() },
        lg = join(&m.learning_goals, "暂无"),
        kb = join(&m.known_biases, "暂无"),
        ip = join(&m.investment_principles, "暂无"),
        wq = join(&m.watch_questions, "暂无"),
        ri = join(&m.recent_insights, "暂无"),
        ua = if m.updated_at.is_empty() { "未知".into() } else { m.updated_at.clone() },
    )
}

fn format_recent_records(records: &[StoredAnalysisRecord]) -> String {
    if records.is_empty() {
        return "暂无最近记录。".into();
    }
    records
        .iter()
        .take(8)
        .map(|r| {
            format!(
                "{}｜{}｜判断 {}｜动作 {}｜{}",
                r.created_at,
                r.item.title,
                r.result.decision,
                r.result.trade_plan.action,
                r.result.summary
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_recent_briefings(briefings: &[RecentBriefing]) -> String {
    if briefings.is_empty() {
        return "暂无最近 briefing。".into();
    }
    briefings
        .iter()
        .take(6)
        .map(|b| format!("[{}]\n{}", b.created_at, truncate_chars(&b.summary_md, 400)))
        .collect::<Vec<_>>()
        .join("\n---\n")
}

// ====== Helpers ======

/// 从 raw 文本里抽出第一个完整 JSON 对象（{...}），处理 Agent 偶尔在 JSON 前后加文字的情况。
fn extract_json_object(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut start: Option<usize> = None;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 && start.is_none() {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        return Some(raw[s..=i].to_string());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn as_string_list(value: Option<&Value>, fallback: Vec<String>) -> Vec<String> {
    match value.and_then(Value::as_array) {
        Some(arr) if !arr.is_empty() => arr
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        _ => fallback,
    }
}

fn clamp(value: f64, min: f64, max: f64) -> f64 {
    if value.is_nan() {
        return min;
    }
    value.max(min).min(max)
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn fmt_num(v: Option<f64>) -> String {
    match v {
        Some(x) => {
            // 对齐 TS 端 String(x) 的行为：整数显示无小数，浮点保留默认精度
            if x.fract() == 0.0 {
                format!("{}", x as i64)
            } else {
                format!("{}", x)
            }
        }
        None => "未知".into(),
    }
}

fn fmt_pct(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{}{:.2}%", if x > 0.0 { "+" } else { "" }, x),
        None => "未知".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_object_handles_strings_with_braces() {
        let raw = r#"some preamble {"a": "b{c}d", "x": {"y": 1}} trailing"#;
        let s = extract_json_object(raw).unwrap();
        assert_eq!(s, r#"{"a": "b{c}d", "x": {"y": 1}}"#);
    }

    #[test]
    fn parse_briefing_minimal() {
        let raw = r#"{"summaryMd": "x", "tradeCalls": [], "signals": []}"#;
        let b = parse_briefing(raw).unwrap();
        assert_eq!(b.summary_md, "x");
        assert_eq!(b.headline, "Agent 简报");
    }
}
