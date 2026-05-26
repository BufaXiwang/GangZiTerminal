//! Pipeline `agent_runtime` —— Agent Runtime（产品里的 Agent 应用层）。
//!
//! 对齐 docs/design/agent-runtime-module.md：
//! - `background_run`：后台 LLM run dispatcher（news_analysis / account_trigger_response）
//! - `decisions`：TradeIntent 状态机 + DispatchOutcome 推进
//! - `locks`：进程内 in-flight lock
//! - `packet`：RealtimeDecisionPacket 构造
//! - `router`：跨模块事件订阅 + consumption record
//! - `scheduler`：news buffer overflow tick
//!
//! 纯 domain 类型（AgentRun / DecisionEpisode / EvidenceRef / TradeIntent /
//! DecisionReview / StrategyCard / AgentToolName）在 [`crate::domain::agent_runtime`]。
//! 持久化在 [`crate::infrastructure::agent_runtime`]。

use tauri::AppHandle;

pub mod background_run;
pub mod decisions;
pub mod locks;
pub mod packet;
pub mod quotes_refresh;
pub mod router;
pub mod run_scope;
pub mod scheduler;

/// 启动 Agent Runtime 后台 loop —— 注册跨模块事件监听 + spawn news buffer tick。
pub fn spawn(app: AppHandle) {
    router::install_listeners(app.clone());
    scheduler::spawn(app);
}
