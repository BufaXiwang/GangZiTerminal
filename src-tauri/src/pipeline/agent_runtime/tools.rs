//! 领域工具 spec + 按 mode 的工具集选择。
//!
//! Spec: docs/design/agent-runtime-module.md §3 mode 表 / §4 工具
//!
//! 本模块是 Runtime **自有**部分：定义领域工具的 `ToolSpec`（名/schema/副作用/spawn 标记）
//! 和「每个 mode 暴露哪些工具」。**handler 桥接到各 BC service** 在 bootstrap/adapter 接线
//! （WP4），那里握 QuotesService / NewsService / AccountService / StrategyService 句柄。
//!
//! 本地通用 tool（read/write/edit_file、run_bash）+ skill（create/run_skill）+ run_subagent
//! 由 **Infra 默认注册**（见 agent-infra §3.6），不在这里。

use crate::domain::agent::runtime::AgentRunMode;
use crate::domain::agent::tools::{SideEffect, ToolSpec};

// ---- 超时（spec-anchored）---------------------------------------------------
const READ_TIMEOUT_MS: u64 = 15_000;
const WRITE_TIMEOUT_MS: u64 = 20_000;

// ---- Runtime 领域工具名（真源：agent-runtime-module.md §4；Infra 只认 opaque ToolName）----
pub const FETCH_QUOTES: &str = "fetch_quotes";
pub const FETCH_NEWS: &str = "fetch_news";
pub const FETCH_ACCOUNT: &str = "fetch_account";
pub const OPERATE_ACCOUNT: &str = "operate_account";
pub const UPDATE_WATCHLIST: &str = "update_watchlist";
pub const RECORD_ANALYSIS: &str = "record_analysis";
pub const RECORD_REVIEW_SUGGESTION: &str = "record_review_suggestion";
pub const UPSERT_INVESTMENT_STRATEGY: &str = "upsert_investment_strategy";

/// `fetch_quotes` —— 行情 / K线 / 指标 / 基本面 / 扫描（→ Quotes `fetch_data` + `scan_market`）。
pub fn spec_fetch_quotes() -> ToolSpec {
    ToolSpec::new(
        FETCH_QUOTES,
        "获取行情快照 / K线 / 指标 / 基本面（tsCodes 路径，→ Quotes fetch_data），或市场扫描（scan 路径，\
         → Quotes scan_market）。tsCodes 与 scan 二选一。只读、不触发远端刷新。\
         indicators 只接受 true（全部指标）或指标名数组（如 [\"ma5\",\"macd_dif\",\"rsi6\"]），\
         **不要传对象**（如 {\"ma\":[5,10]}）。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "tsCodes": { "type": "array", "items": { "type": "string" }, "description": "标的列表（与 scan 二选一）" },
                "scan": { "type": "object", "description": "ScanMarketRequest（与 tsCodes 二选一）" },
                "include": {
                    "type": "object",
                    "properties": {
                        "quote": { "type": "boolean" },
                        "klines": { "type": "array", "items": { "type": "string" }, "description": "如 [\"day\"]" },
                        "indicators": {
                            "description": "true=全部指标；或指标名数组，可选值：ma5/ma10/ma20/ma60/ema12/ema26/\
                                            macd_dif/macd_dea/macd_hist/rsi6/rsi12/rsi24/kdj_k/kdj_d/kdj_j/\
                                            boll_upper/boll_mid/boll_lower/volume_ma5/volume_ma10。\
                                            （不要传对象；误传对象会被当作 true 处理）"
                        },
                        "profile": { "type": "boolean" },
                        "dailyBasic": { "type": "boolean" }
                    }
                }
            }
        }),
        vec![
            r#"<use_tool name="fetch_quotes">{"tsCodes":["600519.SH"],"include":{"quote":true}}</use_tool>"#.into(),
            r#"<use_tool name="fetch_quotes">{"tsCodes":["600406.SH"],"include":{"quote":true,"klines":["day"],"indicators":["ma5","ma20","macd_dif","rsi6"],"dailyBasic":true}}</use_tool>"#.into(),
        ],
        READ_TIMEOUT_MS,
        SideEffect::None,
    )
}

/// `fetch_news` —— 新闻列表 / 全文 / 关键词 FTS / 按 ids 取批（→ News `fetch_news`）。
pub fn spec_fetch_news() -> ToolSpec {
    ToolSpec::new(
        FETCH_NEWS,
        "查资讯：query 关键词检索、sources / 时间窗过滤、ids 按 newsId 精确取回、includeArticle 取正文。只读。\
         【query 用法，务必遵守】query 只放**一个核心关键词**（最多两个），优先 4 字及以上（如「半导体」「贵州茅台」「业绩预增」）。\
         **多词是 AND（全部都要命中），词越多越搜不到——经常返回空**。\
         **绝对不要**把日期、股票代码、「A股/市场」之类宽泛词塞进 query。\
         **日期范围一律用 publishedFrom / publishedTo，不要写进 query。**\
         想要某天/某段时间的全部资讯，就**只传 publishedFrom（+publishedTo），不传 query**。\
         例：今天的半导体资讯 → {\"query\":\"半导体\",\"publishedFrom\":\"2026-06-09T00:00:00Z\"}；\
         今天全部资讯 → {\"publishedFrom\":\"2026-06-09T00:00:00Z\"}（无 query）。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "关键词；尽量 4 字及以上（trigram 子串检索），2~3 字走 LIKE 兜底" },
                "ids": { "type": "array", "items": { "type": "string" } },
                "sources": { "type": "array", "items": { "type": "string" } },
                "publishedFrom": { "type": "string" },
                "publishedTo": { "type": "string" },
                "includeArticle": { "type": "boolean" },
                "limit": { "type": "integer" },
                "offset": { "type": "integer" }
            }
        }),
        vec![r#"<use_tool name="fetch_news">{"query":"贵州茅台","limit":20}</use_tool>"#.into()],
        READ_TIMEOUT_MS,
        SideEffect::None,
    )
}

/// `fetch_account` —— 账户总览 / 持仓 / 订单 / 自选 / 事件 / 触发（→ Account 只读 facade）。
pub fn spec_fetch_account() -> ToolSpec {
    ToolSpec::new(
        FETCH_ACCOUNT,
        "读模拟账户：总览 / 持仓 / 订单 / 自选 / 事件 / 触发。只读。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "include": {
                    "type": "object",
                    "properties": {
                        "snapshot": { "type": "boolean" },
                        "positions": { "type": "boolean" },
                        "orders": { "type": "boolean" },
                        "watchlist": { "type": "boolean" },
                        "events": { "type": "boolean" },
                        "triggers": { "type": "boolean" }
                    }
                }
            }
        }),
        vec![r#"<use_tool name="fetch_account">{"include":{"snapshot":true,"positions":true}}</use_tool>"#.into()],
        READ_TIMEOUT_MS,
        SideEffect::None,
    )
}

/// `operate_account` —— 挂单 / 撤单 / 开平调仓 / 调整保护（→ Account operate_account）。
/// clientOrderId 由 Runtime handler 单点生成注入，模型不提供。
pub fn spec_operate_account() -> ToolSpec {
    ToolSpec::new(
        OPERATE_ACCOUNT,
        "对模拟账户下单 / 撤单 / 开仓 / 调仓 / 平仓 / 调整保护。accountInput 为 Account canonical 动作；\
         reason 说明本次理由。受账户级风控（AccountRiskPolicy，fail-closed）约束。\
         **市价单**：orderType=\"market\"，盘中按现价撮合。\
         **限价挂单**：orderType=\"limit\" + limitPrice，单子留存为 pending，行情满足价格条件时（含次日开盘）自动成交——\
         **收盘后想布局明天就用限价单挂上**，不必等开盘。quantity 是股数（A 股 100 股 = 1 手，须 100 的整数倍）。\
         可选 stopLoss / takeProfit 设保护价。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "accountInput": { "type": "object", "description": "OperateAccountInput；action ∈ place_order/open_position/scale_position/close_position/cancel_order/adjust_protection。\
                    open_position 字段：tsCode, quantity(100整数倍), orderType(market|limit), limitPrice(限价时必填), stopLoss?, takeProfit?, reason" },
                "reason": { "type": "string" }
            },
            "required": ["accountInput", "reason"]
        }),
        vec![
            r#"<use_tool name="operate_account">{"accountInput":{"action":"open_position","tsCode":"600519.SH","quantity":100,"orderType":"market","reason":"盘中追入"},"reason":"突破建仓"}</use_tool>"#.into(),
            r#"<use_tool name="operate_account">{"accountInput":{"action":"open_position","tsCode":"300308.SZ","quantity":100,"orderType":"limit","limitPrice":"45.50","stopLoss":"41.00","reason":"低吸挂单等开盘"},"reason":"收盘后挂限价单布局明天"}</use_tool>"#.into(),
        ],
        WRITE_TIMEOUT_MS,
        SideEffect::TradingWrite,
    )
}

/// `update_watchlist` —— 增删自选 / 备注（→ Account update_watchlist）。
pub fn spec_update_watchlist() -> ToolSpec {
    ToolSpec::new(
        UPDATE_WATCHLIST,
        "维护自选：添加 / 删除 / 改备注。",
        serde_json::json!({
            "type": "object",
            "properties": { "accountInput": { "type": "object" } },
            "required": ["accountInput"]
        }),
        vec![r#"<use_tool name="update_watchlist">{"accountInput":{"action":"add","tsCode":"600519.SH"}}</use_tool>"#.into()],
        WRITE_TIMEOUT_MS,
        SideEffect::NonTradingWrite,
    )
}

/// `record_analysis` —— news 分析定论：声明 action/no_action + 理由 + 相关标的（仅 news mode）。
/// `tradeIds` 由 Runtime 按 run_id 关联本 run 已记的 AgentTrades，模型不提供。
pub fn spec_record_analysis() -> ToolSpec {
    ToolSpec::new(
        RECORD_ANALYSIS,
        "对本批 news 形成判断后声明结论：kind 为 action（下单/改自选）或 no_action（观望）；\
         summary 写结论 + 理由（含「为什么现在进还来得及/已 price-in」判断）；relatedCodes 为相关标的（可空）。\
         **summary 第一行必须是一行主题标题**（≤30 字，概括本批新闻主题 + 判断要点，\
         如「中东冲突升级+AI算力利好——映射已 price-in，观望」），不要以「结论」「no_action」开头；\
         正文从第二行起。大多数 news 应为 no_action。形成判断后**必须**调用一次。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["action", "no_action"] },
                "summary": { "type": "string" },
                "relatedCodes": { "type": "array", "items": { "type": "string" }, "description": "相关标的（可空）" }
            },
            "required": ["kind", "summary"]
        }),
        vec![r#"<use_tool name="record_analysis">{"kind":"no_action","summary":"白酒提价利好——已被 price-in，观望\n\n结论：现价追高风险大，不出手。理由：……","relatedCodes":["600519.SH"]}</use_tool>"#.into()],
        WRITE_TIMEOUT_MS,
        SideEffect::NonTradingWrite,
    )
}

/// `record_review_suggestion` —— 复盘策略建议落库（仅 review mode）。
///
/// review run 形成对策略的调整建议时调用，声明一条结构化建议文本；Runtime 持久化为 `ReviewSuggestion`
/// （绑定本 review run + 交易日）。下次 review 读上一交易日的建议 + 策略版本历史 → 确定性对账「该建议
/// 后是否已 upsert 采纳」（spec §3 ④ follow-up）。**只记建议、不改策略**（策略只在对话中用户确认后写）。
pub fn spec_record_review_suggestion() -> ToolSpec {
    ToolSpec::new(
        RECORD_REVIEW_SUGGESTION,
        "复盘形成对投资策略的调整建议时调用：text 写一条清晰可执行的策略建议（如「单票上限收紧到 15%」）。\
         本工具只登记建议供下次复盘对账是否被采纳，**不会改策略**（策略只在对话中由用户确认后更新）。\
         无策略调整建议则不必调用。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "策略建议文本（清晰、可执行）" }
            },
            "required": ["text"]
        }),
        vec![r#"<use_tool name="record_review_suggestion">{"text":"连续追高亏损 3 笔，建议把单日新开仓上限从 5 降到 3，并对涨幅>5% 的标的禁止开仓。"}</use_tool>"#.into()],
        WRITE_TIMEOUT_MS,
        SideEffect::NonTradingWrite,
    )
}

/// `upsert_investment_strategy` —— 写策略新版本（仅 dialogue mode、用户确认后）。
pub fn spec_upsert_investment_strategy() -> ToolSpec {
    ToolSpec::new(
        UPSERT_INVESTMENT_STRATEGY,
        "写投资策略新版本（自然语言）。仅在用户于对话中明确确认后调用。version 自动 +1。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "strategyId": { "type": "string" },
                "baseVersion": { "type": "integer" },
                "strategy": { "type": "string" },
                "status": { "type": "string", "enum": ["active", "paused"] },
                "reason": { "type": "string" }
            },
            "required": ["strategy", "reason"]
        }),
        vec![r#"<use_tool name="upsert_investment_strategy">{"strategy":"价值优先，单票不超 20%…","reason":"用户确认收紧仓位"}</use_tool>"#.into()],
        WRITE_TIMEOUT_MS,
        SideEffect::NonTradingWrite,
    )
}

/// 某 mode 暴露的领域工具 spec 集（spec §3 mode 表）。
///
/// 本地通用 tool / skill / run_subagent 由 Infra 默认注册，不在此列。
pub fn domain_tools_for_mode(mode: AgentRunMode) -> Vec<ToolSpec> {
    match mode {
        AgentRunMode::Dialogue => vec![
            spec_fetch_quotes(),
            spec_fetch_news(),
            spec_fetch_account(),
            spec_update_watchlist(),
            spec_operate_account(),
            spec_upsert_investment_strategy(),
        ],
        AgentRunMode::News => vec![
            spec_fetch_news(),
            spec_fetch_quotes(),
            spec_fetch_account(),
            spec_update_watchlist(),
            spec_operate_account(),
            spec_record_analysis(),
        ],
        AgentRunMode::AccountTrigger => vec![
            spec_fetch_account(),
            spec_fetch_quotes(),
            spec_fetch_news(),
            spec_operate_account(),
        ],
        // review 只读、永不下单（不含 operate_account）。`record_review_suggestion` 是 non_trading_write
        // （只登记建议供下次 follow-up 对账，不改策略），仅 review mode 暴露。临时复盘用 Infra run_subagent。
        AgentRunMode::Review => vec![
            spec_fetch_account(),
            spec_fetch_quotes(),
            spec_fetch_news(),
            spec_record_review_suggestion(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn names(mode: AgentRunMode) -> HashSet<String> {
        domain_tools_for_mode(mode)
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    #[test]
    fn dialogue_has_full_set_including_strategy() {
        let n = names(AgentRunMode::Dialogue);
        for t in [
            FETCH_QUOTES, FETCH_NEWS, FETCH_ACCOUNT, UPDATE_WATCHLIST, OPERATE_ACCOUNT,
            UPSERT_INVESTMENT_STRATEGY,
        ] {
            assert!(n.contains(t), "dialogue 缺 {t}");
        }
        // 不另设 run_review 工具：临时复盘用 Infra run_subagent（spec §3/§4/§11）。
        assert!(!n.contains("run_review"));
    }

    #[test]
    fn news_has_operate_but_not_strategy_or_run_review() {
        let n = names(AgentRunMode::News);
        assert!(n.contains(OPERATE_ACCOUNT));
        // 不另设 run_review 工具：临时复盘用 Infra run_subagent。
        assert!(!n.contains("run_review"));
        // 策略只在 dialogue 写。
        assert!(!n.contains(UPSERT_INVESTMENT_STRATEGY), "news 不该有 upsert_investment_strategy");
    }

    #[test]
    fn record_analysis_only_in_news_mode() {
        assert!(names(AgentRunMode::News).contains(RECORD_ANALYSIS), "news 缺 record_analysis");
        // record_analysis 是 news 形成判断的唯一定论工具，不在其它 mode 暴露（spec §3/§4/§6）。
        assert!(!names(AgentRunMode::Dialogue).contains(RECORD_ANALYSIS));
        assert!(!names(AgentRunMode::AccountTrigger).contains(RECORD_ANALYSIS));
        assert!(!names(AgentRunMode::Review).contains(RECORD_ANALYSIS));
    }

    #[test]
    fn account_trigger_can_operate_no_run_review() {
        let n = names(AgentRunMode::AccountTrigger);
        assert!(n.contains(OPERATE_ACCOUNT));
        assert!(!n.contains("run_review")); // 保持精简快反
        assert!(!n.contains(UPDATE_WATCHLIST));
    }

    #[test]
    fn review_is_readonly_never_operates() {
        let n = names(AgentRunMode::Review);
        assert!(n.contains(FETCH_ACCOUNT) && n.contains(FETCH_QUOTES) && n.contains(FETCH_NEWS));
        // review 永不下单、不改自选、不写策略。
        assert!(!n.contains(OPERATE_ACCOUNT), "review 永不 operate_account");
        assert!(!n.contains(UPDATE_WATCHLIST));
        assert!(!n.contains(UPSERT_INVESTMENT_STRATEGY));
        assert!(!n.contains("run_review"));
        // record_review_suggestion 仅 review mode 暴露（non_trading_write，只登记建议不改策略，spec §3 ④）。
        assert!(n.contains(RECORD_REVIEW_SUGGESTION), "review 缺 record_review_suggestion");
    }

    #[test]
    fn record_review_suggestion_only_in_review_mode() {
        assert!(names(AgentRunMode::Review).contains(RECORD_REVIEW_SUGGESTION));
        for m in [AgentRunMode::Dialogue, AgentRunMode::News, AgentRunMode::AccountTrigger] {
            assert!(!names(m).contains(RECORD_REVIEW_SUGGESTION), "{m:?} 不该有 record_review_suggestion");
        }
        // 它是 non_trading_write（不改策略、不下单）。
        assert_eq!(spec_record_review_suggestion().side_effect, SideEffect::NonTradingWrite);
    }

    #[test]
    fn side_effects_and_spawn_marks_correct() {
        assert_eq!(spec_operate_account().side_effect, SideEffect::TradingWrite);
        assert_eq!(spec_fetch_quotes().side_effect, SideEffect::None);
        assert_eq!(spec_update_watchlist().side_effect, SideEffect::NonTradingWrite);
        assert!(!spec_operate_account().is_spawn);
    }
}
