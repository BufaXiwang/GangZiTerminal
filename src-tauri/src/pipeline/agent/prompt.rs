//! Agent prompt 构建——chat 模式。
//!
//! - AGENT_IDENTITY：从 identity.md include_str! 进来的人格档案
//! - CHAT_SYSTEM_INSTRUCTIONS：chat 模式追加的简短指令
//! - build_chat_system_context：system 块末尾的稳定上下文（StrategyCard 注入由 Runtime 提供）
//! - build_chat_dynamic_context：user 块开头的动态上下文（市场 + 持仓）

use crate::domain::account::types::Position;
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

## 工具
- 行情 / 资讯 / 账户读取：fetch_quotes / fetch_news / fetch_account
- 维护自选：update_watchlist（非交易写）
- 交易写：operate_account（place_order / cancel_order / open_position / scale_position / close_position / adjust_protection / record_invalidation_signal）
- 决策审计：record_decision_episode / record_decision_review（形成投资判断必须先 record_decision_episode，再调 operate_account；no_action 也必须落 episode）

## 不要做
- 不要包 JSON 整个回答
- 不要写"如果你愿意，下一步可以..."这种揽活尾巴
- 不要重复问已经在上下文里的信息"#;

// ====== Chat prompt 输入打包 ======

/// Chat 系统上下文输入（StrategyCard 摘要由 Agent Runtime packet build 注入）。
pub struct ChatSystemContextInput<'a> {
    pub strategy_summary: &'a str,
}

/// Chat 动态上下文输入——市场快照 + 持仓。
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

pub fn build_chat_system_context(input: &ChatSystemContextInput) -> String {
    if input.strategy_summary.trim().is_empty() {
        "当前没有 active StrategyCard。".to_string()
    } else {
        format!("Active StrategyCard 摘要：\n{}", input.strategy_summary)
    }
}

pub fn build_chat_dynamic_context(input: &ChatDynamicContextInput) -> String {
    format!(
        r#"以下是本次对话开始时的实时上下文，仅作参考——若需要更精准的盘口或 K 线，请用对应工具拉取：
{quotes_availability}
当前市场上下文：
{market}

当前模拟账户持仓（含当前价 / 盈亏）：
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
            line.push_str(&format!("\n  入场理由：{}", truncate_chars(&p.reasoning, 200)));
            line
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn fmt_num(v: Option<f64>) -> String {
    match v {
        Some(n) => format!("{:.2}", n),
        None => "—".into(),
    }
}

fn fmt_pct(v: Option<f64>) -> String {
    match v {
        Some(n) => format!("{:+.2}%", n),
        None => "—".into(),
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{}…", cut)
    }
}
