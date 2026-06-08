//! Gateway 实现 —— 用各 BC 真实 service 实现 Runtime 的 gateway 契约。
//!
//! Spec: docs/design/agent-runtime-module.md §1 边界（Runtime 调各 BC 公开 facade）
//!
//! Runtime 是 app 层、不属任一 BC：它**组合** Quotes/News/Account 的公开 facade（架构允许的方向，
//! 非 BC↔BC 反向依赖）。这些 adapter 把 Runtime 的窄 gateway trait 落到真 service：JSON↔canonical DTO。
//!
//! 三个 adapter 同范式（JSON ↔ canonical DTO → 真 service）：
//! - `NewsGatewayImpl`  — News `fetch_news`（只读 FTS）。
//! - `QuotesGatewayImpl` — Quotes `fetch_data`（tsCodes 路径）/ `scan_market`（scan 路径），二选一。
//! - `AccountGatewayImpl` — Account 只读 `fetch_account` + 写 `operate_account` / `update_watchlist`
//!   （写恒以 `AccountActor::Agent`；reject 是业务结果不抛错 → 映射成 `AccountResultRef`）。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value as JsonValue;

use crate::domain::account::requests::{
    AccountActor, FetchAccountRequest, MarkTriggerHandledRequest, OperateAccountRequest,
    OperateAccountResponse, UpdateWatchlistRequest,
};
use crate::domain::agent::runtime::AccountResultRef;
use crate::domain::news::types::FetchNewsRequest;
use crate::domain::shared::ErrorCode;
use crate::pipeline::account::service::AccountService;
use crate::pipeline::news::service::NewsService;
use crate::pipeline::quotes::service::{FetchDataRequest, QuotesService, ScanMarketRequest};

use super::gateways::{
    AccountGateway, EvalPage, GatewayError, NewsGateway, OperateOutcome, QuotesGateway,
};

/// `NewsGateway` 的真实现：包 `NewsService`，JSON ↔ `FetchNewsRequest`/`FetchNewsResponse`。
pub struct NewsGatewayImpl {
    service: Arc<NewsService>,
}

impl NewsGatewayImpl {
    pub fn new(service: Arc<NewsService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl NewsGateway for NewsGatewayImpl {
    async fn fetch(&self, input: JsonValue) -> Result<JsonValue, GatewayError> {
        let req: FetchNewsRequest = serde_json::from_value(input)
            .map_err(|e| GatewayError::new(ErrorCode::InvalidInput, format!("fetch_news 入参非法: {e}")))?;
        // NewsService.fetch_news 是同步只读（本地 DB / FTS）。
        let resp = self.service.fetch_news(req);
        serde_json::to_value(&resp)
            .map_err(|e| GatewayError::new(ErrorCode::DbError, format!("序列化 FetchNewsResponse 失败: {e}")))
    }
}

/// `QuotesGateway` 的真实现：包 `QuotesService`。`scan` 字段在 → `scan_market`，否则 → `fetch_data`。
pub struct QuotesGatewayImpl {
    service: Arc<QuotesService>,
}

impl QuotesGatewayImpl {
    pub fn new(service: Arc<QuotesService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl QuotesGateway for QuotesGatewayImpl {
    fn core_indexes(&self) -> Vec<String> {
        self.service
            .core_indexes()
            .into_iter()
            .map(|c| c.to_string())
            .collect()
    }

    async fn refresh_quotes(&self, ts_codes: Vec<String>) -> Result<(), GatewayError> {
        // focused 同步刷新（恒 final=true，亚秒级）；emit 的 market-quotes-refreshed 仅供 UI。
        // 非法 ts_code 跳过（subscribed/core_indexes 理应已校验，防御性过滤）。
        let codes: Vec<crate::domain::shared::TsCode> = ts_codes
            .into_iter()
            .filter_map(|c| crate::domain::shared::TsCode::parse(&c).ok())
            .collect();
        if codes.is_empty() {
            return Ok(());
        }
        self.service
            .refresh_quotes(codes)
            .await
            .map(|_| ())
            .map_err(|e| {
                GatewayError::new(
                    ErrorCode::ProviderUnavailable,
                    format!("refresh_quotes 失败: {e:?}"),
                )
            })
    }

    async fn fetch(&self, input: JsonValue) -> Result<JsonValue, GatewayError> {
        // scan 与 tsCodes 二选一（spec §4 fetch_quotes）：scan 字段存在 → 走 scan_market。
        if let Some(scan) = input.get("scan") {
            let req: ScanMarketRequest = serde_json::from_value(scan.clone()).map_err(|e| {
                GatewayError::new(ErrorCode::InvalidInput, format!("scan 入参非法: {e}"))
            })?;
            let resp = self.service.scan_market(req);
            return serde_json::to_value(&resp).map_err(|e| {
                GatewayError::new(ErrorCode::DbError, format!("序列化 ScanMarketResponse 失败: {e}"))
            });
        }
        // tsCodes 路径：整包当 FetchDataRequest（tsCodes/include/limit 同名 camelCase）。
        let req: FetchDataRequest = serde_json::from_value(input).map_err(|e| {
            GatewayError::new(ErrorCode::InvalidInput, format!("fetch_quotes 入参非法: {e}"))
        })?;
        let resp = self.service.fetch_data(req);
        serde_json::to_value(&resp).map_err(|e| {
            GatewayError::new(ErrorCode::DbError, format!("序列化 FetchDataResponse 失败: {e}"))
        })
    }
}

/// `AccountGateway` 的真实现：包 `AccountService`。写恒以 `AccountActor::Agent`。
pub struct AccountGatewayImpl {
    service: Arc<AccountService>,
}

impl AccountGatewayImpl {
    pub fn new(service: Arc<AccountService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl AccountGateway for AccountGatewayImpl {
    async fn fetch(&self, input: JsonValue) -> Result<JsonValue, GatewayError> {
        let req: FetchAccountRequest = serde_json::from_value(input).map_err(|e| {
            GatewayError::new(ErrorCode::InvalidInput, format!("fetch_account 入参非法: {e}"))
        })?;
        let resp = self.service.fetch_account(req);
        serde_json::to_value(&resp).map_err(|e| {
            GatewayError::new(ErrorCode::DbError, format!("序列化 FetchAccountResponse 失败: {e}"))
        })
    }

    async fn operate(
        &self,
        account_input: JsonValue,
        client_order_id: &str,
        _reason: &str,
    ) -> OperateOutcome {
        // reason 由 Runtime 侧 AgentTrade 记账。入参非法 → 业务拒绝（accepted=false），不抛错。
        let req: OperateAccountRequest = match serde_json::from_value(account_input) {
            Ok(r) => r,
            Err(e) => {
                return OperateOutcome::from_result(AccountResultRef {
                    accepted: false,
                    reason: Some(ErrorCode::InvalidInput),
                    message: Some(format!("operate_account 入参非法: {e}")),
                    ..Default::default()
                });
            }
        };
        // 透传 clientOrderId：Account 按它幂等去重（同一 clientOrderId 只执行一次，崩溃重提交安全）。
        let resp = self
            .service
            .operate_account_with_dedup(req, AccountActor::Agent, client_order_id);
        operate_response_to_outcome(resp)
    }

    async fn update_watchlist(&self, input: JsonValue) -> Result<JsonValue, GatewayError> {
        let req: UpdateWatchlistRequest = serde_json::from_value(input).map_err(|e| {
            GatewayError::new(ErrorCode::InvalidInput, format!("update_watchlist 入参非法: {e}"))
        })?;
        let resp = self.service.update_watchlist(req, AccountActor::Agent);
        serde_json::to_value(&resp).map_err(|e| {
            GatewayError::new(ErrorCode::DbError, format!("序列化 UpdateWatchlistResponse 失败: {e}"))
        })
    }

    async fn mark_trigger_handled(&self, trigger_id: &str) -> Result<bool, GatewayError> {
        let resp = self.service.mark_trigger_handled(MarkTriggerHandledRequest {
            trigger_id: trigger_id.to_string(),
            reason: "agent account_trigger run 终态".to_string(),
        });
        match resp.reason {
            Some(code) if !resp.accepted => {
                Err(GatewayError::new(code, resp.message.unwrap_or_default()))
            }
            _ => Ok(resp.accepted),
        }
    }

    fn max_daily_new_orders(&self) -> u32 {
        self.service.config().risk_policy.max_daily_new_orders
    }

    fn subscribed_codes(&self) -> Vec<String> {
        self.service
            .subscribed_codes()
            .into_iter()
            .map(|c| c.to_string())
            .collect()
    }

    async fn rebuild_account_snapshot(&self) -> Result<(), GatewayError> {
        self.service
            .rebuild_account_snapshot()
            .map(|_| ())
            .map_err(|code| GatewayError::new(code, "rebuild_account_snapshot 失败"))
    }

    async fn evaluate_account_triggers(
        &self,
        cursor: Option<String>,
        batch_size: u32,
    ) -> Result<EvalPage, GatewayError> {
        let now = chrono::Utc::now();
        let r = self
            .service
            .evaluate_account_triggers_once(now, batch_size as usize, cursor);
        Ok(EvalPage {
            has_more: r.has_more,
            next_cursor: r.next_cursor,
        })
    }

    async fn list_unhandled_triggers(
        &self,
        limit: u32,
    ) -> Result<Vec<super::gateways::UnhandledTrigger>, GatewayError> {
        // 只读 facade：fetch_account(include.triggers=true, trigger_handled=false)（spec §8 ⑤）。
        let req = FetchAccountRequest {
            include: Some(crate::domain::account::requests::FetchAccountInclude {
                triggers: Some(true),
                ..Default::default()
            }),
            trigger_handled: Some(
                crate::domain::account::requests::TriggerHandledFilter::Bool(false),
            ),
            limit: Some(limit),
            ..Default::default()
        };
        let resp = self.service.fetch_account(req);
        let out = resp
            .triggers
            .unwrap_or_default()
            .into_iter()
            .map(|t| super::gateways::UnhandledTrigger {
                trigger_id: t.trigger_id,
                order_id: t.order_id,
                summary: format!(
                    "{:?} {} {}",
                    t.trigger_type,
                    t.ts_code.as_ref().map(|c| c.as_str()).unwrap_or(""),
                    t.threshold.clone().unwrap_or_default(),
                ),
            })
            .collect();
        Ok(out)
    }

    fn daily_return(&self, now: chrono::DateTime<chrono::Utc>) -> Option<f64> {
        self.service.daily_return(now)
    }

    fn consecutive_losses(&self, now: chrono::DateTime<chrono::Utc>) -> u32 {
        self.service.consecutive_losses(now)
    }

    fn daily_drawdown(&self, now: chrono::DateTime<chrono::Utc>) -> f64 {
        self.service.daily_drawdown(now)
    }
}

/// `OperateAccountResponse` → `OperateOutcome`：稳定 ID 进 `result`（→ AgentTrade 审计戳）；
/// snapshot/warnings 仅回模型（spec §4），不持久化。
fn operate_response_to_outcome(resp: OperateAccountResponse) -> OperateOutcome {
    let snapshot = serde_json::to_value(&resp.snapshot).unwrap_or(JsonValue::Null);
    let warnings = resp.warnings.clone();
    let result = AccountResultRef {
        accepted: resp.accepted,
        order_id: resp.order_id,
        fill_ids: resp.fill_ids,
        position_id: resp.position_id,
        account_event_ids: resp.account_event_ids,
        rejection_event_id: resp.rejection_event_id,
        reason: resp.reason,
        message: resp.message,
    };
    OperateOutcome {
        result,
        snapshot,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::db::{run_migrations, AppDb};
    use crate::infrastructure::news::migrations::migrations as news_migrations;
    use crate::infrastructure::news::SourceRegistry;

    fn news_service() -> Arc<NewsService> {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, news_migrations()).unwrap());
        let registry = Arc::new(SourceRegistry::new());
        Arc::new(NewsService::new(db, registry).unwrap())
    }

    #[tokio::test]
    async fn news_gateway_maps_json_to_request_and_back() {
        let gw = NewsGatewayImpl::new(news_service());
        // 空库查询：合法入参 → 返回结构化 JSON（items 数组存在），不报错。
        let out = gw.fetch(serde_json::json!({"query": "茅台", "limit": 5})).await.unwrap();
        assert!(out.get("items").is_some(), "应返回 FetchNewsResponse JSON（含 items）");
    }

    #[tokio::test]
    async fn news_gateway_rejects_bad_input() {
        let gw = NewsGatewayImpl::new(news_service());
        // limit 类型错误（字符串）→ 反序列化失败 → InvalidInput。
        let err = gw
            .fetch(serde_json::json!({"limit": "not-a-number"}))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    // ----- Quotes -----

    fn quotes_service() -> Arc<QuotesService> {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, crate::infrastructure::quotes::migrations()).unwrap());
        Arc::new(QuotesService::new(db, crate::infrastructure::quotes::QuotesConfig::default()).unwrap())
    }

    #[tokio::test]
    async fn quotes_gateway_fetch_data_path() {
        let gw = QuotesGatewayImpl::new(quotes_service());
        // 未 seed 标的：fetch_data 仍返回结构化 JSON（items 数组存在，标的标 missing）。
        let out = gw
            .fetch(serde_json::json!({"tsCodes": ["600519.SH"], "include": {"quote": true}}))
            .await
            .unwrap();
        assert!(out.get("items").is_some(), "fetch_data 应返回 FetchDataResponse JSON");
    }

    #[tokio::test]
    async fn quotes_gateway_scan_path() {
        let gw = QuotesGatewayImpl::new(quotes_service());
        // scan 字段存在 → 走 scan_market；空库返回结构化结果不报错。
        let out = gw.fetch(serde_json::json!({"scan": {"limit": 5}})).await.unwrap();
        assert!(out.is_object(), "scan_market 应返回 ScanMarketResponse JSON");
    }

    // ----- Account -----

    fn account_service() -> Arc<AccountService> {
        use crate::pipeline::account::quote_gateway::MockQuoteGateway;
        use crate::pipeline::account::service::AccountServiceConfig;
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| {
            let mut all = Vec::new();
            all.extend(crate::infrastructure::quotes::migrations());
            all.extend(crate::infrastructure::account::migrations());
            all.extend(crate::infrastructure::account::migrations_tail());
            run_migrations(c, all).unwrap();
        });
        let gw = Arc::new(MockQuoteGateway::new());
        let svc = Arc::new(AccountService::new(db, gw, AccountServiceConfig::default()));
        svc.initialize_account_if_needed(crate::domain::shared::Money(
            rust_decimal::Decimal::new(1_000_000, 0),
        ))
        .unwrap();
        svc
    }

    #[tokio::test]
    async fn account_gateway_fetch_snapshot() {
        let gw = AccountGatewayImpl::new(account_service());
        let out = gw.fetch(serde_json::json!({"include": {"snapshot": true}})).await.unwrap();
        assert!(out.get("snapshot").is_some(), "fetch_account 应返回含 snapshot 的 JSON");
    }

    #[tokio::test]
    async fn account_gateway_operate_bad_input_is_business_reject_not_error() {
        let gw = AccountGatewayImpl::new(account_service());
        // 入参非法 → 不抛错，返回 accepted=false 的 AccountResultRef（写不走 GatewayError）。
        let r = gw.operate(serde_json::json!({"nonsense": true}), "co_x", "test").await;
        assert!(!r.result.accepted);
        assert_eq!(r.result.reason, Some(ErrorCode::InvalidInput));
    }
}
