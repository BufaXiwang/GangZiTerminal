//! Agent prompt 构建——chat 模式。
//!
//! - AGENT_IDENTITY：从 identity.md include_str! 进来的人格档案
//! - CHAT_SYSTEM_INSTRUCTIONS：chat 模式追加的简短指令
//! - build_chat_system_context：system 块末尾的稳定上下文（active heuristics top-N 注入）
//! - build_chat_dynamic_context：user 块开头的动态上下文（市场 + 持仓）

use crate::domain::account::types::Position;
use crate::domain::agent::heuristic::Heuristic;
use crate::domain::quotes::regime::Regime;
use crate::domain::quotes::{MarketOverview, StockQuote};
use std::collections::HashMap;

pub(crate) const AGENT_IDENTITY: &str = include_str!("identity.md");

/// Chat 模式下追加在 identity 之后的 system 指令——简短运行时提醒，
/// 详细规则全在 identity.md。
pub(crate) const CHAT_SYSTEM_INSTRUCTIONS: &str = r#"你是 GangZiTerminal 的操盘手 Agent，正在和围观你交易的用户对话。

## Chat 里你仍然是操盘手
- 自己判断要开/平/调仓 → 直接调写工具下单，然后用自然语言汇报"我在 X 价开了 Y 股、止损 Z、理由 W"
- 用户给指令（"建 X" / "平 Y" / "调止损到 Z"）→ 同样直接执行 + 汇报
- 信心不足 → 直说"我不开，因为 X"——这本身就是一个决策，不要把球踢给用户
- 写工具失败 → 如实告诉用户哪条规则不通 + 下一步可行方案；绝不假装下单成功

## Heuristic 纪律
- 用户口头说出偏好 / 纠错 → 调 `propose_heuristic(origin="user_stated", ...)`，effective_state 直接 active
- 不要把一次性指令写成 heuristic，只写「可反复应用于未来场景」的判断
- 反复打脸 / 用户撤回 / 与新规则冲突且新的更准 → 调 `retire_heuristic`

## Position 纪律（v4 合并 Expectation）
- 开仓 → `open_position(kind=live/watch, direction, take_profit?, stop_loss?, invalidation_signals, signals_used, reasoning, conviction?)`
- `kind=watch` 表示"看好但不下注"——shares=0，不动现金，只走 judge → close → lesson 学习闭环
- 触发条件（take_profit / stop_loss / time_stop / invalidation_signals）由 scheduler tick 自动平仓，agent 只在主观撤回时调 `close_position(reason=manual)`
- 调整假设 → `adjust_position`（改 target / stop / horizon / invalidation_signals / reasoning）
- 如果你依赖了上面列出的 heuristic（按 id），就把它们填到 `applied_heuristic_ids`——close 时按这个精确给该 heuristic 计 hit/miss

## 不要做
- 不要包 JSON 整个回答
- 不要写"如果你愿意，下一步可以..."这种揽活尾巴
- 不要重复问已经在上下文里的信息"#;

// ====== Chat prompt 输入打包 ======

/// Chat 的"稳定"系统上下文输入——active heuristics top-N 注入。
pub struct ChatSystemContextInput<'a> {
    pub heuristics: &'a [Heuristic],
    pub current_regime: Option<Regime>,
}

/// Chat 的"易变"动态上下文输入——市场快照、持仓。
///
/// `live_quotes`：当前 chat run 已 fetch 的实时行情；按 6 位 code 索引用于
/// format_positions 显示当前价 + 盈亏%。
pub struct ChatDynamicContextInput<'a> {
    pub market_overview: Option<&'a MarketOverview>,
    pub simulated_positions: &'a [Position],
    pub live_quotes: &'a [StockQuote],
    pub quotes_availability: Option<&'a str>,
}

// ====== Builders ======

fn format_availability_block(availability: Option<&str>) -> String {
    match availability {
        Some(text) if !text.trim().is_empty() => format!("\n\n{}\n", text),
        _ => String::new(),
    }
}

/// 构造 chat 的 system 上下文文本（active heuristics + 当前 regime）。
pub fn build_chat_system_context(input: &ChatSystemContextInput) -> String {
    let regime_line = match input.current_regime {
        Some(r) => format!("当前市场状态（regime）：{}\n", r.as_str()),
        None => String::new(),
    };
    format!(
        r#"{regime_line}
你当前生效的启发式规则（heuristics——结构化原则 / 已知偏差 / 风险偏好，
带实战 track record，agent 用 propose_heuristic 写、apply/retire 调整）：
{heuristics}"#,
        heuristics = format_heuristics(input.heuristics),
    )
}

/// 构造 chat 的动态上下文文本（市场 + 持仓）。
pub fn build_chat_dynamic_context(input: &ChatDynamicContextInput) -> String {
    format!(
        r#"以下是本次对话开始时的实时上下文，仅作参考——若需要更精准的盘口或 K 线，请用对应工具拉取：
{quotes_availability}
当前市场上下文：
{market}

当前模拟账户持仓（含当前价 / 盈亏 / 假设字段；历史事件链请用 get_position(position_id) 单独拉）：
{positions}"#,
        quotes_availability = format_availability_block(input.quotes_availability),
        market = format_market(input.market_overview),
        positions = format_positions(input.simulated_positions, input.live_quotes),
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

fn format_positions(positions: &[Position], live_quotes: &[StockQuote]) -> String {
    let opens: Vec<&Position> = positions.iter().filter(|p| p.status.is_open()).collect();
    if opens.is_empty() {
        return "暂无模拟持仓。".into();
    }
    let quote_by_code: HashMap<&str, &StockQuote> = live_quotes
        .iter()
        .map(|q| (q.code.as_str(), q))
        .collect();
    opens
        .iter()
        .take(12)
        .map(|p| {
            let mut line = format!(
                "{}({}) [{}] {}股 成本 ¥{:.2} direction={}",
                p.name,
                p.code.as_str(),
                p.kind.as_str(),
                p.current_shares.value(),
                p.avg_entry_price.value(),
                p.direction.as_str(),
            );
            let entry = p.avg_entry_price.value();
            if let Some(q) = quote_by_code.get(p.code.as_str()) {
                if let Some(px) = q.price.as_ref().map(|y| y.value()) {
                    let pnl_pct = if entry > 0.0 {
                        (px - entry) / entry * 100.0
                    } else {
                        0.0
                    };
                    let pnl_abs = (px - entry) * p.current_shares.value() as f64;
                    line.push_str(&format!(
                        " → 现价 ¥{:.2}  {:+.2}% ({:+.0})",
                        px, pnl_pct, pnl_abs
                    ));
                }
            }
            if let Some(sl) = p.stop_loss {
                let dist = if entry > 0.0 {
                    (sl.value() - entry) / entry * 100.0
                } else {
                    0.0
                };
                line.push_str(&format!("\n  止损 ¥{:.2} ({:+.2}%)", sl.value(), dist));
            }
            if let Some(tp) = p.take_profit {
                let dist = if entry > 0.0 {
                    (tp.value() - entry) / entry * 100.0
                } else {
                    0.0
                };
                line.push_str(&format!("  止盈 ¥{:.2} ({:+.2}%)", tp.value(), dist));
            }
            if !p.invalidation_signals.is_empty() {
                let fams: Vec<_> = p.invalidation_signals.iter().map(|s| s.family_str()).collect();
                line.push_str(&format!("\n  失效条件: {}", fams.join(", ")));
            }
            line.push_str(&format!("\n  入场理由：{}", truncate_chars(&p.reasoning, 200)));
            if !p.signals_used.is_empty() {
                let fams: Vec<_> = p.signals_used.iter().map(|s| s.family_str()).collect();
                line.push_str(&format!("\n  入场信号: {}", fams.join(", ")));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn format_heuristics(hs: &[Heuristic]) -> String {
    if hs.is_empty() {
        return "暂无 active heuristic（启动时应已 seed 10 条；如果这里空说明启动 seed 失败）。"
            .into();
    }
    hs.iter()
        .map(|h| {
            let regime_str = if h.regime_tags.is_empty() {
                String::from("通用")
            } else {
                h.regime_tags
                    .iter()
                    .map(|r| r.as_str().to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let origin_icon = match h.origin {
                crate::domain::agent::heuristic::HeuristicOrigin::Seed => "📚",
                crate::domain::agent::heuristic::HeuristicOrigin::UserStated => "🧑",
                crate::domain::agent::heuristic::HeuristicOrigin::AgentInferred => "🤖",
            };
            let conf = h
                .confidence()
                .map(|c| format!("{:.0}%", c * 100.0))
                .unwrap_or_else(|| "—".into());
            format!(
                "- {} id={} [{}] hit/miss={}/{} conf={} regime={} · {}",
                origin_icon,
                h.id.as_str(),
                h.category.as_str(),
                h.hit_count,
                h.miss_count,
                conf,
                regime_str,
                h.body
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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
        Some(x) => format!("{:+.2}%", x),
        None => "—".into(),
    }
}
