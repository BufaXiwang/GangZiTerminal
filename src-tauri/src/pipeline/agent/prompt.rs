//! Agent prompt 构建——chat 模式。
//!
//! briefing / review 已下线，这里只剩 chat 需要的：
//! - AGENT_IDENTITY：从 identity.md include_str! 进来的人格档案
//! - CHAT_SYSTEM_INSTRUCTIONS：chat 模式追加的工具/记忆/输出指令
//! - build_chat_system_context：system 块末尾的稳定上下文（memory + learning，打 cache_control）
//! - build_chat_dynamic_context：user 块开头的动态上下文（市场 + 持仓 + 数据可用性）
//! - 各类 formatter：market / quotes / positions / memory / learning

use crate::domain::account::types::{Position, PositionEvent};
use crate::domain::agent::memory::InvestorMemory;
use crate::domain::quotes::{MarketOverview, StockQuote};
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
- 对话过程中得出值得长期记忆的判断（关注主题、新原则、风险偏好变化、近期 insight）→ 调 `update_memory`。
- 如果意识到既有记忆不再适用 / 已被反例推翻 → 调 `remove_memory`（按字段名 + 精确字符串匹配）。
- 单条记忆 ≤ 80 字。记忆是流动判断快照，不是积累的标语清单——更新和删除都自然发生。

## 不要做
- 不要把判断包装成 JSON 给我解析——直接说人话。
- 不要在文本里塞"memoryUpdates: {...}"这种结构——用工具去写。
- 不要重复问用户已经说过的信息——上文+长期记忆+持仓档案都给你了。
- 不要写"如果你愿意，我下一步可以帮你 X / Y / Z"这种揽活尾巴——你是操盘手，决策已经做了或不做，没什么可"如果"的。"#;

// ====== Chat prompt 输入打包 ======

/// Chat 的"稳定"系统上下文输入——投资者长期记忆 + 学习画像。
///
/// 这部分跨 chat turn 基本不变（memory 写入是 mutation 工具触发的，单次 request
/// 内部稳定），适合放进 system block 末尾打 cache_control，让多轮 chat 共用 cache prefix。
pub struct ChatSystemContextInput<'a> {
    pub investor_memory: Option<&'a InvestorMemory>,
}

/// Chat 的"易变" 动态上下文输入——市场快照、持仓。
/// 这部分每次 chat 都不一样（盘口随时变、持仓随交易变），不进 cache prefix，
/// 作为 messages 列表里的第一条 user 消息塞给模型。
pub struct ChatDynamicContextInput<'a> {
    pub market_overview: Option<&'a MarketOverview>,
    pub simulated_positions: &'a [Position],
    pub position_events: &'a HashMap<String, Vec<PositionEvent>>,
    /// 行情拉取异常的"数据可用性"提示（None = 正常）。pipeline 计算后传入。
    pub quotes_availability: Option<&'a str>,
}

// ====== Builders ======

/// 行情可用性"提示"——成功时不输出，异常时插一段警告。
/// 前后加换行，让它在 prompt 里独立成块。
fn format_availability_block(availability: Option<&str>) -> String {
    match availability {
        Some(text) if !text.trim().is_empty() => format!("\n\n{}\n", text),
        _ => String::new(),
    }
}

/// 构造 chat 的 system 上下文文本（投资者长期记忆 + 学习画像）。
///
/// 用于和 identity / 系统指令一起打包成 system block，且这一段会打 cache_control
/// 形成 cacheable prefix——只要用户两轮之间没调 update_memory / remove_memory，
/// 这段文本就不变，多轮 chat 共用同一份 prompt cache。
pub fn build_chat_system_context(input: &ChatSystemContextInput) -> String {
    format!(
        r#"投资者长期记忆（持续累积，agent 可通过 update_memory / remove_memory 工具更新）：
{memory}"#,
        memory = format_investor_memory(input.investor_memory),
    )
}

/// 构造 chat 的动态上下文文本（市场快照 + 模拟持仓）。
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
{positions}"#,
        quotes_availability = format_availability_block(input.quotes_availability),
        market = format_market(input.market_overview),
        positions = format_positions(input.simulated_positions, input.position_events),
    )
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

#[allow(dead_code)]
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
    positions: &[Position],
    events_by_position: &HashMap<String, Vec<PositionEvent>>,
) -> String {
    let opens: Vec<&Position> = positions.iter().filter(|p| p.status.is_open()).collect();
    if opens.is_empty() {
        return "暂无模拟持仓。".into();
    }
    opens
        .iter()
        .take(12)
        .map(|p| {
            let mut header = format!(
                "{}({}) {}股 入场 ¥{}",
                p.name,
                p.code.as_str(),
                p.current_shares.value(),
                p.avg_entry_price.value()
            );
            if let Some(sl) = p.stop_loss {
                header.push_str(&format!(" 止损 ¥{:.2}", sl.value()));
            }
            if let Some(tp) = p.take_profit {
                header.push_str(&format!(" 止盈 ¥{:.2}", tp.value()));
            }
            header.push_str(&format!("\n  入场逻辑：{}", p.thesis));
            let events = events_by_position
                .get(p.id.as_str())
                .cloned()
                .unwrap_or_default();
            if events.is_empty() {
                return header;
            }
            let mut sorted = events;
            sorted.sort_by(|a, b| a.occurred_at.value().cmp(&b.occurred_at.value()));
            let chain: Vec<String> = sorted
                .iter()
                .rev()
                .take(10)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|event| {
                    // event.occurred_at.to_rfc3339() 形如 "2026-05-17T08:30:00+00:00"，取前 10 字符 = 日期
                    let iso = event.occurred_at.to_rfc3339();
                    let date: String = iso.chars().take(10).collect();
                    let note = if event.agent_note_md.is_empty() {
                        String::new()
                    } else {
                        format!("｜{}", truncate_chars(&event.agent_note_md, 80))
                    };
                    format!("    ├ {} {}{}", date, event.kind.tag(), note)
                })
                .collect();
            format!("{}\n  事件链：\n{}", header, chain.join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
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

// ====== Helpers ======

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn fmt_num(v: Option<f64>) -> String {
    match v {
        Some(x) => {
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
