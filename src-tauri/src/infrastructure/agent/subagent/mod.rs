//! 子 Agent / Fork —— execution 底座（Infra 层）。
//!
//! Spec: docs/design/agent-infra-module.md §3.5 子 Agent / Fork + §3.6 Infra 默认 Tools
//!
//! fork 子 agent 是 Infra 的执行能力：在隔离上下文里再跑一遍 loop（复用 `run_agent_turn`），
//! 跑完只把**子 run 末轮文本**带回父（对齐 Claude Code「只取最后一条消息」）。纯执行机制，与业务无关。
//!
//! 本模块提供：
//! - `ForkHandle`：fork 执行底座 + **静态依赖注入**（registry / 子 provider 工厂 / repo / SkillStore /
//!   任务注册表）。`run_subagent` / `run_skill` 两个 tool 的 handler 持有它的 `Arc`。
//! - `ForkRuntime`：**每-run 注入**的运行时上下文（channel / fallback / retry / compaction / max_turns /
//!   is_subagent / parent_run_id / parent_event_tx）。由发起 run 的入口构造、作为 `DispatchExt` 透传给
//!   `run_agent_turn_forked`；dispatch 时 fork handler 把 `ext` downcast 回 `ForkRuntime` 读取——
//!   于是 fork 用的是**当前这次 run 的真实** channel / is_subagent / event_tx，而非注册时静态捕获的占位值。
//!   **不允许嵌套（对齐 Claude Code `isInForkChild` 布尔）**：子 run 的 registry 根本不含
//!   `run_subagent` / `run_skill`（见 `child_registry`），从根上没法再 fork；`ForkRuntime` 透传的
//!   `is_subagent` 标记仅作兜底守卫（`is_subagent == true` 时 `run_forked_agent` 拒绝，防工具未被
//!   正确剔除）。没有深度计数——只有顶层 agent 能 fork。
//! - `run_forked_agent`：起一个**子 run**（全新 `conversation_id` + `parent_run_id` 关联），
//!   继承父 channel / model / effort / 工具集（可用 `allowed_tools` 收紧），返回子 run 最终文本。
//! - `SubAgentTask` + `SubAgentTaskRegistry`：子 agent 任务注册表 + 生命周期（spec §3.5）。
//! - 三种执行模式：前台（阻塞）/ 后台（`run_in_background` → 立即返回 agentId + 完成时发通知）/ 并行
//!   （后台 spawn 天然并发）。
//! - `register_subagent_tools`：把 `run_subagent` / `run_skill` 注册进 registry（bootstrap 调用）。
//!
//! 隔离 / 继承 / 不嵌套（spec §3.5 不变量）：
//! - 隔离：子用全新 `conversation_id`（`fork:<parent_run_id>:<uuid>`），不与父共享消息历史；
//!   中间 tool 调用 / 试错不进父上下文。
//! - 继承：channel / model / effort / 工具集默认继承父；`allowed_tools` 给了就收紧到子集。
//!   **但子工具集一律剔除 spawn 类（`run_subagent` / `run_skill`）**——只有顶层能 spawn。
//! - 不嵌套：只有顶层 agent（`is_subagent == false`）能 fork；子 agent（`is_subagent == true`）不能
//!   再 fork（首选靠剔除 spawn 工具，兜底靠 `is_subagent` 布尔守卫）。对齐 Claude Code `isInForkChild`。
//! - 只回末轮文本：父对话只拿子 run **最后一轮**（不再发起工具调用、给出最终答案那轮）的 assistant
//!   文本；中间轮的铺垫 / 工具机制都不回父（每遇 `ToolStart` 清空文本累加器实现）。子 system prompt
//!   另提示子把完整结论放最后一条消息。后台另发 `<task-notification>`。


const SUBAGENT_TIMEOUT_MS: u64 = 600_000; // 10 分钟：fork 子 run 是长任务，远超普通 tool。
const SKILL_TIMEOUT_MS: u64 = 600_000;

/// fork 子 agent 的固定提示（spec §3.5「只回末轮文本」）：父只取子 run **最后一条消息**回灌，
/// 中间轮（含工具调用前的自然语言铺垫）都不回父。统一在 `run_forked_agent` 拼进子 run 引导，
/// `run_subagent`（自由 prompt）/ `run_skill`（SKILL.md 作 prompt）两条路径都带上。
const FORK_FINAL_MESSAGE_HINT: &str = "【返回约定】只有你的**最后一条消息**会被返回给调用者；\
中间轮的思考、铺垫、工具调用过程都不会回传。请把完整的结论 / 产出 / 交付物全部写进最后一条消息里，\
不要分散在中间轮。Only your final message is returned to the caller — put the complete result there.";


mod fork;
mod runtime;
mod task_registry;
mod tools;

pub use fork::run_forked_agent;
pub use runtime::{http_provider_factory, ForkHandle, ForkRuntime, ProviderFactory};
pub use task_registry::{SubAgentProgress, SubAgentStatus, SubAgentTask, SubAgentTaskRegistry};
pub use tools::{register_subagent_tools, stop_subagent, subagent_output};

#[cfg(test)]
pub(crate) use fork::handle_run_subagent;

#[cfg(test)]
mod tests;

