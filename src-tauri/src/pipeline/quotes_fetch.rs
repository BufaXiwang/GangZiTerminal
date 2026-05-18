//! Snapshot-first 的 quotes 拉取 + 失败可见性事件。
//!
//! Agent 上下文 / chat pipeline 都不直接戳 `market_snapshot`——经过这里统一：
//! - 命中的 quotes 直接返回
//! - 没命中的 code 走 `EVENT_QUOTES_FETCH_STATUS` 给前端打提示
//! - `to_prompt_section` 把这些信息折叠进给 agent 的提示，避免它凭空填价格

use crate::domain::quotes::StockQuote;
use crate::pipeline::events::EVENT_QUOTES_FETCH_STATUS;
use serde_json::json;
use std::collections::HashSet;
use tauri::{AppHandle, Emitter};

/// 行情拉取的完整状态——拿到的 quotes + 失败可见信息。
#[derive(Debug, Clone, Default)]
pub struct QuotesFetchResult {
    pub quotes: Vec<StockQuote>,
    pub requested: Vec<String>,
    pub missing: Vec<String>,
    pub provider_error: Option<String>,
}

impl QuotesFetchResult {
    pub fn from_partial(quotes: Vec<StockQuote>, requested: Vec<String>) -> Self {
        let returned: HashSet<String> =
            quotes.iter().map(|q| q.code.as_str().to_string()).collect();
        let missing: Vec<String> = requested
            .iter()
            .filter(|c| !returned.contains(*c))
            .cloned()
            .collect();
        Self {
            quotes,
            requested,
            missing,
            provider_error: None,
        }
    }

    pub fn has_any_issue(&self) -> bool {
        self.provider_error.is_some() || !self.missing.is_empty()
    }

    pub fn to_prompt_section(&self) -> Option<String> {
        if !self.has_any_issue() {
            return None;
        }
        if let Some(err) = &self.provider_error {
            let err_short: String = err.chars().take(160).collect();
            return Some(format!(
                "🔴 行情接口异常\n- 错误：{}\n- 请求 {} 只均未拿到实时数据\n- 后续分析请避免依赖盘中价格；可用昨收 / 历史 K 线 / 涨停池 / 公告等离线信息判断",
                err_short,
                self.requested.len()
            ));
        }
        let missing_preview: String = self
            .missing
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join("、");
        let suffix = if self.missing.len() > 8 {
            format!("（共 {} 只缺数据）", self.missing.len())
        } else {
            String::new()
        };
        Some(format!(
            "⚠️ 行情数据部分缺失\n- 请求 {} 只，拿到 {} 只\n- 缺数据：{}{}\n- 这些代码可能停牌或接口未返回；分析时请明示，不要凭推断填具体价格",
            self.requested.len(),
            self.quotes.len(),
            missing_preview,
            suffix
        ))
    }
}

/// 拉行情 + 自动 emit 失败可见事件——pipeline 的统一入口。
pub async fn fetch_quotes_with_visibility(
    app: &AppHandle,
    stage: &str,
    codes: Vec<String>,
) -> QuotesFetchResult {
    let result = if codes.is_empty() {
        QuotesFetchResult::default()
    } else {
        use crate::infrastructure::quotes::snapshot::market_snapshot;
        let mut quotes: Vec<StockQuote> = Vec::with_capacity(codes.len());
        for code in &codes {
            if let Some(ts) =
                crate::infrastructure::quotes::repository::resolve_stock_ts_code(app, code)
            {
                if let Some(q) = market_snapshot::get(&ts) {
                    quotes.push(q);
                }
            }
        }
        QuotesFetchResult::from_partial(quotes, codes)
    };
    if result.has_any_issue() {
        let _ = app.emit(
            EVENT_QUOTES_FETCH_STATUS,
            json!({
                "stage": stage,
                "requested": result.requested.len(),
                "ok": result.quotes.len(),
                "missing": result.missing,
                "providerError": result.provider_error,
            }),
        );
    }
    result
}
