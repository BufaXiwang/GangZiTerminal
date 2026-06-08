//! BC gateway 契约 —— Runtime 视角下「我需要各 BC 提供什么」。
//!
//! Spec: docs/design/agent-runtime-module.md §4 工具
//!
//! 依赖倒置：Runtime 定义这些 trait（它需要的能力），**bootstrap/adapter 用各 BC 的真实
//! service 实现它们**（QuotesService / NewsService / AccountService）。Runtime 的 tool handler
//! 只依赖 trait → 可用 mock 单测，且不反向耦合具体 BC。
//!
//! 边界：gateway 只暴露 Runtime 真正用到的窄接口；account write 经 `operate`（Runtime 单点生成
//! `client_order_id` 注入，Account 按它去重 + fail-closed 兜底硬风控）。

use async_trait::async_trait;
use serde_json::Value as JsonValue;

use crate::domain::agent::runtime::AccountResultRef;
use crate::domain::shared::codes::WarningCode;

/// `operate` 的完整产出：持久化的 `result`（→ AgentTrade 审计戳）+ 仅供工具输出给模型看的
/// `snapshot`/`warnings`（spec §4 `OperateAccountToolOutput`，**不进 AgentTrade**）。
pub struct OperateOutcome {
    pub result: AccountResultRef,
    /// Account canonical `AccountSnapshot` JSON（交易后账户状态，仅回模型，不持久化）。
    pub snapshot: JsonValue,
    pub warnings: Vec<WarningCode>,
}

impl OperateOutcome {
    /// 只有 result（snapshot 空 / 无 warning）—— 拒绝路径与测试 mock 用。
    pub fn from_result(result: AccountResultRef) -> Self {
        Self {
            result,
            snapshot: JsonValue::Null,
            warnings: Vec::new(),
        }
    }
}

/// 行情读（→ Quotes `fetch_data` + `scan_market`）。
#[async_trait]
pub trait QuotesGateway: Send + Sync {
    /// `input` = `fetch_quotes` tool 的 JSON 入参（tsCodes/scan/include）。返回 PacketQuotes-ish JSON。
    async fn fetch(&self, input: JsonValue) -> Result<JsonValue, GatewayError>;

    /// 核心基准指数 ts_code 列表（→ Quotes `core_indexes`），供 review 报告基准对照（spec §3/§6）。
    /// 默认空（测试 mock 无需 override）。
    fn core_indexes(&self) -> Vec<String> {
        Vec::new()
    }

    /// 聚焦按需刷新一批 quotes（→ Quotes `refresh_quotes`）。**同步、恒 `final=true`、亚秒级**：
    /// universe 的后台 fallback 不挂这条路径。用于账户自驱 quote tick——在 rebuild→eval 前先把
    /// `subscribed_codes ∪ core_indexes` 刷新（spec agent-runtime §6 行情/账户维护调度）。
    /// 默认 noop（测试 mock 无需 override）。
    async fn refresh_quotes(&self, _ts_codes: Vec<String>) -> Result<(), GatewayError> {
        Ok(())
    }
}

/// 资讯读（→ News `fetch_news`，含 ids 批量 / query FTS）。
#[async_trait]
pub trait NewsGateway: Send + Sync {
    async fn fetch(&self, input: JsonValue) -> Result<JsonValue, GatewayError>;
}

/// `evaluate_account_triggers` 的分页结果（Runtime 只需游标驱动评估；trigger 本身经
/// `account-triggered` 事件流走既有 Runtime 消费链）。对齐 account-module `AccountTriggerResult`。
#[derive(Debug, Clone, Default)]
pub struct EvalPage {
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

/// 账户读 + 写（→ Account 只读 facade / `operate_account` / `update_watchlist`）。
#[async_trait]
pub trait AccountGateway: Send + Sync {
    /// 只读：`fetch_account` tool 的 JSON 入参 → PacketAccount-ish JSON。
    async fn fetch(&self, input: JsonValue) -> Result<JsonValue, GatewayError>;

    /// 交易写：`account_input` 为 Account canonical 动作 JSON；`client_order_id` 由 Runtime 注入
    /// （Account 按它幂等去重）。返回 `OperateOutcome`：持久化 `result` + 仅回模型的 snapshot/warnings
    /// （接受/拒绝都返回，不抛错）。
    async fn operate(
        &self,
        account_input: JsonValue,
        client_order_id: &str,
        reason: &str,
    ) -> OperateOutcome;

    /// 自选写：`update_watchlist`。
    async fn update_watchlist(&self, input: JsonValue) -> Result<JsonValue, GatewayError>;

    /// 标记账户触发器已被 Runtime 处置（→ Account `mark_trigger_handled`）。
    /// 仅在 account_trigger run 终态后调用（spec §6）。默认 noop（测试 mock 无需 override）。
    async fn mark_trigger_handled(&self, _trigger_id: &str) -> Result<bool, GatewayError> {
        Ok(false)
    }

    /// 账户当前关注集合（→ Account `subscribed_codes` = watchlist ∪ open_positions ∪ pending_orders）。
    /// 账户自驱 quote tick 据此（并 ∪ core_indexes）做 focused refresh + rebuild + eval。
    /// 空 → 跳过本 tick（spec agent-runtime §6）。默认空（测试 mock 无需 override）。
    fn subscribed_codes(&self) -> Vec<String> {
        Vec::new()
    }

    /// 账户级「当日新开仓上限」（→ Account `AccountRiskPolicy.max_daily_new_orders`），
    /// 供编排级当日额度闸门（spec §6）。默认无限（测试 mock 不限额）。
    fn max_daily_new_orders(&self) -> u32 {
        u32::MAX
    }

    /// 重建账户快照（→ Account `rebuild_account_snapshot`）。Runtime 在账户自驱 quote tick 的
    /// focused refresh 之后先重建快照、再评估触发器（spec agent-runtime §6 行情/账户维护调度）。
    /// 默认 noop（测试 mock 无需 override）。
    async fn rebuild_account_snapshot(&self) -> Result<(), GatewayError> {
        Ok(())
    }

    /// 评估账户触发器 — **单批**（→ Account `evaluate_account_triggers`）。Runtime 按返回的
    /// `has_more`/`next_cursor` 分页耗尽；命中的 trigger 经 `account-triggered` 事件流被既有
    /// Runtime 消费链处置。默认空页（测试 mock 无需 override）。
    async fn evaluate_account_triggers(
        &self,
        _cursor: Option<String>,
        _batch_size: u32,
    ) -> Result<EvalPage, GatewayError> {
        Ok(EvalPage::default())
    }

    /// 启动恢复 ⑤（spec §8）：列出 Account 侧**未 handled** 的 trigger（只读 facade，按
    /// `trigger_handled=false` 过滤）。每项 = `(triggerId, orderId?, summary)`，供 Runtime 启动时补扫、
    /// 经既有 account_trigger 路由处置（dedupe 保证不重复）。默认空（测试 mock 无需 override）。
    async fn list_unhandled_triggers(&self, _limit: u32) -> Result<Vec<UnhandledTrigger>, GatewayError> {
        Ok(Vec::new())
    }

    // ----------------------------------------------------------------
    // 账户财务事实只读 facade（账户为单一所有者，下沉自 Runtime）
    // Spec: account-module.md §2「账户财务事实只读 facade」/ agent-runtime §3②
    //
    // 复盘超额 = `daily_return` 与 `core_indexes` 涨幅相减（跨 BC 编排）仍在 Runtime。
    // ----------------------------------------------------------------

    /// 当日组合收益率（→ Account `daily_return(now)`）；首次当日估值幂等落日初基线。
    /// 日初为 0 / 权益缺失 → None。默认 None（测试 mock 无需 override）。
    fn daily_return(&self, _now: chrono::DateTime<chrono::Utc>) -> Option<f64> {
        None
    }

}

/// 未 handled 的账户触发器（启动恢复 ⑤ 补扫用，spec §8）。
#[derive(Debug, Clone)]
pub struct UnhandledTrigger {
    pub trigger_id: String,
    pub order_id: Option<String>,
    pub summary: String,
}

/// gateway 读失败（写不走这里——写返回 AccountResultRef 自带 accepted=false + reason）。
#[derive(Debug, Clone)]
pub struct GatewayError {
    pub code: crate::domain::shared::ErrorCode,
    pub message: String,
}

impl GatewayError {
    pub fn new(code: crate::domain::shared::ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
