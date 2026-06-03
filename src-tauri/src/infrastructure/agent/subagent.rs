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

use crate::domain::agent::{
    AgentEvent, AgentMessage, AgentMessageBlock, AgentMessageRole, AgentRunRequest,
    AgentStopReason, CompactionConfig, ContextBundle, ProviderChannel, RetryConfig, SideEffect,
    ToolSpec,
};
use crate::domain::shared::ErrorCode;
use crate::infrastructure::agent::http_provider::HttpProvider;
use crate::infrastructure::agent::loop_executor::{
    run_agent_turn_forked, LoopError, ProviderStream,
};
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use crate::infrastructure::agent::payload_store::PayloadStore;
use crate::infrastructure::agent::skill_store::SkillStore;
use crate::infrastructure::agent::tool_registry::{
    DispatchExt, RegisterError, ToolHandler, ToolHandlerFuture, ToolHandlerOutput, ToolInvocation,
    ToolRegistry,
};
use chrono::Utc;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use uuid::Uuid;

const SUBAGENT_TIMEOUT_MS: u64 = 600_000; // 10 分钟：fork 子 run 是长任务，远超普通 tool。
const SKILL_TIMEOUT_MS: u64 = 600_000;

/// fork 子 agent 的固定提示（spec §3.5「只回末轮文本」）：父只取子 run **最后一条消息**回灌，
/// 中间轮（含工具调用前的自然语言铺垫）都不回父。统一在 `run_forked_agent` 拼进子 run 引导，
/// `run_subagent`（自由 prompt）/ `run_skill`（SKILL.md 作 prompt）两条路径都带上。
const FORK_FINAL_MESSAGE_HINT: &str = "【返回约定】只有你的**最后一条消息**会被返回给调用者；\
中间轮的思考、铺垫、工具调用过程都不会回传。请把完整的结论 / 产出 / 交付物全部写进最后一条消息里，\
不要分散在中间轮。Only your final message is returned to the caller — put the complete result there.";

// ───────────────────────── 子 provider 工厂 ─────────────────────────

/// 子 run 的 provider 工厂：给定 channel → 一个 `ProviderStream`。
///
/// 生产用 `HttpProvider`；hermetic 测试注入 scripted 工厂，避免真网络（spec 验证记录：hermetic）。
pub type ProviderFactory =
    Arc<dyn Fn(&ProviderChannel) -> Result<Box<dyn ProviderStream>, LoopError> + Send + Sync>;

/// 默认生产工厂：`HttpProvider::new`。
pub fn http_provider_factory() -> ProviderFactory {
    Arc::new(|ch: &ProviderChannel| {
        HttpProvider::new(ch.clone()).map(|p| Box::new(p) as Box<dyn ProviderStream>)
    })
}

// ───────────────────────── 子 Agent 任务注册表 ─────────────────────────

/// 子 run 状态（spec §3.5 `SubAgentTask.status`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Killed,
}

impl SubAgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SubAgentStatus::Queued => "queued",
            SubAgentStatus::Running => "running",
            SubAgentStatus::Completed => "completed",
            SubAgentStatus::Failed => "failed",
            SubAgentStatus::Killed => "killed",
        }
    }
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            SubAgentStatus::Completed | SubAgentStatus::Failed | SubAgentStatus::Killed
        )
    }
}

/// 子 run 累计进度（spec §3.5 `SubAgentTask.progress`，从子 run 的 turn / usage 累计）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SubAgentProgress {
    pub tokens: u32,
    pub tool_uses: u32,
    pub duration_ms: u64,
}

/// 一条子 agent 任务（spec §3.5 `SubAgentTask`）。
pub struct SubAgentTask {
    pub agent_id: String,
    pub parent_run_id: String,
    pub conversation_id: String,
    pub description: String,
    pub status: SubAgentStatus,
    pub progress: SubAgentProgress,
    /// 子 run 已产出的最终结果文本（终态时填）。后台任务的 `subagent_output` 读它。
    pub result: Option<String>,
    /// abort 句柄（后台任务才有；前台同步跑不需要）。`stop_subagent` 用它取消。
    pub abort: Option<AbortHandle>,
    /// 防重复完成通知（spec §3.5 `notified`）。
    pub notified: bool,
}

/// 子 agent 任务注册表。主 Agent 通过它管理所有 spawn 出来的子 run（spec §3.5）。
#[derive(Clone, Default)]
pub struct SubAgentTaskRegistry {
    tasks: Arc<Mutex<HashMap<String, SubAgentTask>>>,
}

impl SubAgentTaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// spawn：注册一条新任务（初始 `running`）。
    fn register(
        &self,
        agent_id: &str,
        parent_run_id: &str,
        conversation_id: &str,
        description: &str,
        abort: Option<AbortHandle>,
    ) {
        let mut g = self.tasks.lock().expect("SubAgentTaskRegistry poisoned");
        g.insert(
            agent_id.to_string(),
            SubAgentTask {
                agent_id: agent_id.to_string(),
                parent_run_id: parent_run_id.to_string(),
                conversation_id: conversation_id.to_string(),
                description: description.to_string(),
                status: SubAgentStatus::Running,
                progress: SubAgentProgress::default(),
                result: None,
                abort,
                notified: false,
            },
        );
    }

    /// 终态：complete / fail / kill，落进度 + 结果。
    fn finish(
        &self,
        agent_id: &str,
        status: SubAgentStatus,
        progress: SubAgentProgress,
        result: Option<String>,
    ) {
        let mut g = self.tasks.lock().expect("SubAgentTaskRegistry poisoned");
        if let Some(t) = g.get_mut(agent_id) {
            // killed 是终态，不被后续 complete/fail 覆盖。
            if t.status == SubAgentStatus::Killed {
                return;
            }
            t.status = status;
            t.progress = progress;
            t.result = result;
        }
    }

    /// 给一条已注册的任务补挂 abort 句柄（后台任务：先 register 再 spawn 再挂句柄，避免子 run
    /// 瞬时完成时回调早于 register 的竞态）。
    fn set_abort(&self, agent_id: &str, abort: AbortHandle) {
        let mut g = self.tasks.lock().expect("SubAgentTaskRegistry poisoned");
        if let Some(t) = g.get_mut(agent_id) {
            // 若任务已终态（瞬时完成），不再挂句柄（abort 无意义）。
            if !t.status.is_terminal() {
                t.abort = Some(abort);
            }
        }
    }

    /// 标记一条任务为 killed（abort 句柄触发后），返回是否成功（任务存在且非终态）。
    pub fn mark_killed(&self, agent_id: &str) -> bool {
        let mut g = self.tasks.lock().expect("SubAgentTaskRegistry poisoned");
        match g.get_mut(agent_id) {
            Some(t) if !t.status.is_terminal() => {
                if let Some(h) = &t.abort {
                    h.abort();
                }
                t.status = SubAgentStatus::Killed;
                true
            }
            _ => false,
        }
    }

    /// 读一条任务的快照（status / progress / result），用于 `subagent_output`。
    pub fn snapshot(&self, agent_id: &str) -> Option<(SubAgentStatus, SubAgentProgress, Option<String>)> {
        let g = self.tasks.lock().expect("SubAgentTaskRegistry poisoned");
        g.get(agent_id)
            .map(|t| (t.status, t.progress, t.result.clone()))
    }

    /// 取并置位 `notified`（防重复通知）：返回 true 表示本次是第一次通知。
    fn take_notify_flag(&self, agent_id: &str) -> bool {
        let mut g = self.tasks.lock().expect("SubAgentTaskRegistry poisoned");
        match g.get_mut(agent_id) {
            Some(t) if !t.notified => {
                t.notified = true;
                true
            }
            _ => false,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.tasks.lock().unwrap().len()
    }

    #[cfg(test)]
    pub fn status_of(&self, agent_id: &str) -> Option<SubAgentStatus> {
        self.tasks.lock().unwrap().get(agent_id).map(|t| t.status)
    }
}

// ───────────────────────── Fork 运行时上下文（每-run 注入） ─────────────────────────

/// **每-run** 注入的 fork 运行时上下文（spec §3.5）。
///
/// 由发起 run 的入口（Tauri command / scheduler / Runtime；测试里手工构造）在跑某个 run 时构造，
/// 作为 `DispatchExt` 透传给 `run_agent_turn_forked`；fork handler 在 **dispatch 时**把 `ext` downcast
/// 回 `ForkRuntime`，于是拿到的是**当前这次 run 的真实** channel / is_subagent / parent_run_id / event_tx
/// （而不是注册 handler 时静态捕获的占位值）。
///
/// 不嵌套（对齐 Claude Code `isInForkChild` 布尔）：子 run 跑 `run_agent_turn_forked` 时传的是
/// `child()`（is_subagent=true、parent_run_id=子 run id、不回灌父 event_tx）。子 run 的 registry 已剔除
/// spawn 工具（不会再触发 fork）；即便某种原因仍持有，`is_subagent == true` 也被 `run_forked_agent`
/// 的布尔守卫拒绝。没有深度计数。
#[derive(Clone)]
pub struct ForkRuntime {
    /// 继承父：channel / 容错 / 压缩 / max_turns。
    pub channel: ProviderChannel,
    pub fallback_channels: Vec<ProviderChannel>,
    pub retry: Option<RetryConfig>,
    pub compaction: Option<CompactionConfig>,
    pub max_turns: u32,
    /// 是否是 fork 出的子 agent（对齐 Claude Code `isInForkChild`）。顶层 run = false；
    /// `child()` 产出的子运行时 = true。子 agent 不允许再 fork。
    pub is_subagent: bool,
    /// 父 run 标识（子 run 用它做 parent 关联）。
    pub parent_run_id: String,
    /// 父 run 的 event_tx（后台任务完成时 emit `<task-notification>`）。
    pub parent_event_tx: Option<mpsc::Sender<AgentEvent>>,
}

impl ForkRuntime {
    /// 从一个 run 的 channel + parent_run_id 构造（is_subagent=false，其余取默认）。
    pub fn new(channel: ProviderChannel, parent_run_id: impl Into<String>) -> Self {
        Self {
            channel,
            fallback_channels: vec![],
            retry: None,
            compaction: None,
            max_turns: 12,
            is_subagent: false,
            parent_run_id: parent_run_id.into(),
            parent_event_tx: None,
        }
    }

    pub fn with_fallback_channels(mut self, channels: Vec<ProviderChannel>) -> Self {
        self.fallback_channels = channels;
        self
    }
    pub fn with_retry(mut self, retry: Option<RetryConfig>) -> Self {
        self.retry = retry;
        self
    }
    pub fn with_compaction(mut self, compaction: Option<CompactionConfig>) -> Self {
        self.compaction = compaction;
        self
    }
    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = max_turns;
        self
    }
    pub fn with_is_subagent(mut self, is_subagent: bool) -> Self {
        self.is_subagent = is_subagent;
        self
    }
    pub fn with_event_tx(mut self, tx: Option<mpsc::Sender<AgentEvent>>) -> Self {
        self.parent_event_tx = tx;
        self
    }

    pub fn is_subagent(&self) -> bool {
        self.is_subagent
    }

    /// 给子 run 用的运行时上下文：is_subagent=true、parent_run_id=子 run id、event_tx 不回灌父（隔离）。
    /// 置 is_subagent=true（对齐 CC `isInForkChild`）使子 run 一旦因某种原因仍触发 fork 时被布尔守卫兜底拒绝。
    pub fn child(&self, child_run_id: &str) -> ForkRuntime {
        let mut c = self.clone();
        c.is_subagent = true;
        c.parent_run_id = child_run_id.to_string();
        c.parent_event_tx = None;
        c
    }

    /// 包装成 dispatch 用的不透明 `DispatchExt`（registry 透传，handler downcast）。
    pub fn into_ext(self) -> DispatchExt {
        Arc::new(self)
    }

    /// 从 dispatch 透传来的 `ext` 还原 `ForkRuntime`（downcast；非 fork 上下文 → None）。
    fn from_ext(ext: &Option<DispatchExt>) -> Option<ForkRuntime> {
        ext.as_ref()
            .and_then(|e| e.clone().downcast::<ForkRuntime>().ok())
            .map(|a| (*a).clone())
    }
}

// ───────────────────────── Fork 句柄（静态依赖注入） ─────────────────────────

/// fork 执行底座 + **静态依赖注入**。
///
/// `run_subagent` / `run_skill` 的 handler 持有它的 `Arc`，在 dispatch 时触发子 run loop。
/// 持有的是 **run 无关的静态依赖**：父 registry（默认继承的工具集）、子 provider 工厂、repo、
/// 任务注册表、SkillStore，以及一组**默认运行时配置**（channel / 容错 / 压缩 / max_turns / is_subagent /
/// parent_run_id / event_tx）。
///
/// 真实的运行时上下文由 `ForkRuntime` 在**发起 run 时注入**（经 `DispatchExt` 透传到 dispatch）。
/// 当 dispatch 带了 `ForkRuntime` → 用它（当前 run 的真实 channel / is_subagent / event_tx）；没带 →
/// 退回 `ForkHandle` 自带的默认（bootstrap 占位 / 测试手工配置）。这样 handler **不再静态捕获**
/// 运行时上下文，Phase 3 接线只需发起 run 时填 `ForkRuntime`，无需 registry replace。
#[derive(Clone)]
pub struct ForkHandle {
    /// 默认继承的工具集（父 registry）。`allowed_tools` 给了就构造收紧的子 registry。
    registry: Arc<ToolRegistry>,
    /// 子 run 的 provider 工厂（生产 = HttpProvider；测试 = scripted）。
    provider_factory: ProviderFactory,
    /// 审计落库 repo（子 run 用自己的 conversation_id + parent 关联）。
    repo: Option<AgentMessagesRepo>,
    payload_store: Option<PayloadStore>,
    /// 子 agent 任务注册表（spec §3.5）。
    tasks: SubAgentTaskRegistry,
    /// Skill 存盘访问（`run_skill` 读 SKILL.md 全文作 prompt）。
    skill_store: SkillStore,
    /// 默认运行时上下文（无 `ForkRuntime` 注入时的退路：bootstrap 占位 / 测试手工配置）。
    default_rt: ForkRuntime,
}

impl ForkHandle {
    /// 构造一个 fork 句柄。生产由 bootstrap 装配；测试可手工拼。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<ToolRegistry>,
        provider_factory: ProviderFactory,
        repo: Option<AgentMessagesRepo>,
        payload_store: Option<PayloadStore>,
        channel: ProviderChannel,
        skill_store: SkillStore,
        tasks: SubAgentTaskRegistry,
    ) -> Self {
        Self {
            registry,
            provider_factory,
            repo,
            payload_store,
            tasks,
            skill_store,
            default_rt: ForkRuntime::new(channel, String::new()),
        }
    }

    pub fn with_fallback_channels(mut self, channels: Vec<ProviderChannel>) -> Self {
        self.default_rt.fallback_channels = channels;
        self
    }
    pub fn with_retry(mut self, retry: Option<RetryConfig>) -> Self {
        self.default_rt.retry = retry;
        self
    }
    pub fn with_compaction(mut self, compaction: Option<CompactionConfig>) -> Self {
        self.default_rt.compaction = compaction;
        self
    }
    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.default_rt.max_turns = max_turns;
        self
    }
    pub fn with_is_subagent(mut self, is_subagent: bool) -> Self {
        self.default_rt.is_subagent = is_subagent;
        self
    }
    /// 绑定默认父 run 上下文（parent_run_id + event_tx），供后台完成通知。
    pub fn for_parent_run(
        mut self,
        parent_run_id: impl Into<String>,
        parent_event_tx: Option<mpsc::Sender<AgentEvent>>,
    ) -> Self {
        self.default_rt.parent_run_id = parent_run_id.into();
        self.default_rt.parent_event_tx = parent_event_tx;
        self
    }

    pub fn tasks(&self) -> &SubAgentTaskRegistry {
        &self.tasks
    }
    pub fn skill_store(&self) -> &SkillStore {
        &self.skill_store
    }
    pub fn is_subagent(&self) -> bool {
        self.default_rt.is_subagent
    }

    /// 解析本次 dispatch 用的运行时上下文：dispatch 带了 `ForkRuntime` → 用它（当前 run 真实上下文）；
    /// 没带 → 退回 `ForkHandle` 自带默认。这是「run 时注入」的核心解析点。
    fn resolve_rt(&self, ext: &Option<DispatchExt>) -> ForkRuntime {
        ForkRuntime::from_ext(ext).unwrap_or_else(|| self.default_rt.clone())
    }

    /// 构造子 run 的 registry。
    ///
    /// **不允许嵌套（首选机制）**：无论 `allowed_tools` 是否给定，子 registry **都不含**
    /// `run_subagent` / `run_skill`——这俩是 spawn 类工具，剔除后子 agent 的 system prompt 里根本没有
    /// 它们 → 不能再 fork（对齐 Claude Code 的「不给/拒绝 fork 工具」）。其它工具（含 `create_skill`
    /// = 写文件、本地 tool、领域 tool）保留。`create_skill` 不算 spawn（不递归），保留。
    ///
    /// - `allowed_tools=None`：继承父全部工具，**但仍剔除 spawn 类**。
    /// - `allowed_tools=Some(..)`：收紧到子集，**且剔除其中的 spawn 类**（即便被显式列入也不给）。
    ///
    /// 子 registry 仍走父 repo / payload_store（dispatch 持久化），故用父的 repo/payload_store 构造。
    fn child_registry(&self, allowed: Option<&[String]>) -> Result<Arc<ToolRegistry>, LoopError> {
        let child = match (&self.repo, &self.payload_store) {
            (Some(r), Some(p)) => ToolRegistry::new(r.clone(), p.clone()),
            _ => ToolRegistry::new_without_persist(),
        };
        // allowed=None → 继承父全部；Some → 收紧到子集。两种情况都剔除 spawn 类工具。
        let allow: Option<std::collections::HashSet<&str>> =
            allowed.map(|a| a.iter().map(|s| s.as_str()).collect());
        for spec in self.registry.list_tools() {
            // 从根上剔除 spawn 类工具，禁止嵌套 fork。
            if is_spawn_tool(&spec.name) {
                continue;
            }
            if allow
                .as_ref()
                .map(|a| a.contains(spec.name.as_str()))
                .unwrap_or(true)
            {
                if let Some(handler) = self.registry.clone_handler(&spec.name) {
                    // 重注册同名 tool 到子 registry（不会 Duplicate：子是新空表）。
                    let _ = child.register_tool(spec, handler);
                }
            }
        }
        Ok(Arc::new(child))
    }
}

// ───────────────────────── run_forked_agent（核心底座） ─────────────────────────

/// fork 执行底座（spec §3.5 `run_forked_agent`）。
///
/// 给定 `{ rt(子 run 的运行时上下文), prompt, system_prompt?, allowed_tools? }` → 起一个**子 run**
/// （复用 `run_agent_turn_forked`），全新 `conversation_id`（带 parent 关联），跑完返回
/// **子 run 末轮文本**（每遇 `ToolStart` 清空累加器 → 只剩不再调工具的末轮；对齐 Claude Code「最后一条
/// 消息」）。子的中间轮铺垫 / 工具过程不回给父（隔离）。并在子 prompt 末尾拼 `FORK_FINAL_MESSAGE_HINT`
/// 提示子把完整结论放最后一条消息。
///
/// `rt` 是**发起这次 fork 的运行时**（即当前正在执行的 run 的运行时）。顶层 run = `is_subagent=false`；
/// 若它本身已是子 agent（`is_subagent == true`）→ 拒绝（不允许嵌套，兜底守卫，对齐 CC `isInForkChild`）。
/// 通过守卫后，本函数内部调 `rt.child(agent_id)` 派生子 run 的运行时（置 `is_subagent=true`、
/// parent_run_id=子 run id、不回灌父 event_tx），原样包成 `DispatchExt` 透传进子 run 的 loop——于是子 run
/// 内部若再触发 fork，`spawn_or_run` 拿到的 rt 已是 `is_subagent=true`，被预检拒绝。
///
/// 子 run 的 registry 已剔除 spawn 工具，**正常不会再嵌套 fork**；本函数的布尔守卫只是兜底（防工具未被
/// 正确剔除）。没有深度计数。
///
/// `agent_id` 是子 run 标识（也是子 run 的 run_id）。
///
/// 返回 `Ok(final_text)` 或 `Err(...)`（子 run 失败 / 嵌套被拒 / provider 构造失败）。
pub async fn run_forked_agent(
    handle: &ForkHandle,
    rt: &ForkRuntime,
    agent_id: &str,
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
) -> Result<String, LoopError> {
    // 不嵌套兜底守卫（spec §3.5 不变量，对齐 CC `isInForkChild` 布尔）。`rt` 是**发起方**的运行时：
    // 若发起方本身已是子 agent（is_subagent==true）→ 拒绝。只有顶层 agent 能 fork。正常路径子 registry
    // 已剔除 spawn 工具不会走到这里；这是防工具未被正确剔除的兜底。没有任何数字比较。
    if rt.is_subagent {
        return Err(LoopError::Provider(
            "nested sub-agent not allowed; only the top-level agent can fork".to_string(),
        ));
    }
    // 通过守卫后派生子 run 的运行时（is_subagent=true、parent 关联子 run id、隔离 event_tx）。
    let child_rt = rt.child(agent_id);

    // 隔离上下文：全新 conversation_id，带 parent_run_id 关联（命名编码，无需 DB schema 改动）。
    let conversation_id = format!("fork:{}:{}", child_rt.parent_run_id, Uuid::new_v4());

    // 子 registry：默认继承父；allowed_tools 收紧。
    let registry = handle.child_registry(allowed_tools)?;

    // 子 run 的 providers：channel + fallback（继承父）。
    let mut providers: Vec<Box<dyn ProviderStream>> = Vec::new();
    providers.push((handle.provider_factory)(&child_rt.channel)?);
    for fb in &child_rt.fallback_channels {
        providers.push((handle.provider_factory)(fb)?);
    }

    // 子 run 的 input：一条 user message = prompt。统一在此拼上 fork 提示（spec §3.5「只回末轮文本」）：
    // 告诉子 agent 只有最后一条消息会被返回，必须把完整结论放进末轮。两条 fork 路径
    // （run_subagent 自由 prompt / run_skill SKILL.md 作 prompt）都经由此处，故提示统一注入最稳妥。
    let mut blocks = Vec::new();
    if let Some(sys) = system_prompt {
        // system_prompt 作为子 run 的引导：拼进 user prompt 前缀（隔离上下文，独立 system 由
        // SystemPromptBuilder 在 loop 内注入 tool 清单；这里把 skill 正文 / 引导塞进 user turn）。
        blocks.push(AgentMessageBlock::Text {
            text: format!("{sys}\n\n{prompt}\n\n{FORK_FINAL_MESSAGE_HINT}"),
        });
    } else {
        blocks.push(AgentMessageBlock::Text {
            text: format!("{prompt}\n\n{FORK_FINAL_MESSAGE_HINT}"),
        });
    }
    let input = vec![AgentMessage {
        message_id: format!("am-{}", Uuid::new_v4()),
        run_id: Some(agent_id.to_string()),
        conversation_id: Some(conversation_id.clone()),
        seq: None,
        kind: None,
        role: AgentMessageRole::User,
        blocks,
        created_at: Utc::now(),
    }];

    let request = AgentRunRequest {
        run_id: agent_id.to_string(),
        trigger: "subagent_fork".to_string(),
        channel: child_rt.channel.clone(),
        max_turns: child_rt.max_turns,
        input,
        conversation_id: Some(conversation_id),
        compaction: child_rt.compaction.clone(),
        fallback_channels: child_rt.fallback_channels.clone(),
        retry: child_rt.retry.clone(),
    };

    // 子 run 用**自己的** event 通道（隔离）：聚合 TextDelta 为最终文本；不回灌父。
    let (child_tx, mut child_rx) = mpsc::channel::<AgentEvent>(256);
    let context = ContextBundle::new(agent_id);

    // 子 run 内部再 fork 时，dispatch 读到的运行时上下文 = `child_rt`（is_subagent=true，由 child() 置）。
    // 经 `<use_tool name="run_subagent">` 触发的嵌套 fork 据此被 `spawn_or_run` 预检拒绝（无深度计数）。
    let child_ext = child_rt.clone().into_ext();

    let started = std::time::Instant::now();
    let run = run_agent_turn_forked(
        request,
        registry,
        context,
        providers,
        child_tx,
        handle.repo.clone(),
        Some(child_ext),
        // 子 run 的取消走任务注册表的 AbortHandle（stop_subagent），不经 loop 的 CancellationToken。
        tokio_util::sync::CancellationToken::new(),
    );

    // 并行消费子事件 + 等子 run 完成。
    //
    // **只回末轮文本**（对齐 Claude Code「只取子 run 最后一条消息」，spec §3.5）：
    // 子 loop 的事件顺序在每个 turn 内固定为「该 turn 的全部 `TextDelta` → 该 turn 的 `ToolStart`/
    // `ToolEnd`」（provider 在 `next_turn` 里先 emit 文本，loop 随后逐个 dispatch tool）。一个**发起了
    // 工具调用**的 turn 不是末轮（loop 会继续推进下一轮）；**没有任何工具调用**的 turn 才是末轮（loop
    // `break`）。`TextDelta` 不带 turn 标记，故用 `ToolStart` 作 turn 边界信号：**每遇到一个 `ToolStart`
    // 就清空文本累加器**——于是任何带工具调用的 turn 的铺垫文本都被丢弃，loop 结束时累加器里只剩末轮
    // （不再发起工具调用、给出最终答案那轮）的文本。`tool_uses` 仍累计全程（进度统计不变）。
    let collector = async {
        let mut text = String::new();
        let mut tool_uses: u32 = 0;
        while let Some(ev) = child_rx.recv().await {
            match ev {
                AgentEvent::TextDelta { delta, .. } => text.push_str(&delta),
                AgentEvent::ToolStart { .. } => {
                    // 进入工具调用 = 当前累计的文本属于非末轮的铺垫 → 丢弃，只保留之后（末轮）的文本。
                    text.clear();
                    tool_uses += 1;
                }
                _ => {}
            }
        }
        (text, tool_uses)
    };

    let (summary_res, (text, tool_uses)) = tokio::join!(run, collector);
    let duration_ms = started.elapsed().as_millis() as u64;
    let summary = summary_res?;
    let progress = SubAgentProgress {
        tokens: summary.input_tokens.saturating_add(summary.output_tokens),
        tool_uses,
        duration_ms,
    };
    // 终态记进注册表（前台 / 后台共用此函数；前台 caller 已 register 过）。
    let status = match summary.stop_reason {
        AgentStopReason::Error | AgentStopReason::ToolError => SubAgentStatus::Failed,
        _ => SubAgentStatus::Completed,
    };
    handle
        .tasks
        .finish(agent_id, status, progress, Some(text.clone()));

    Ok(text)
}

// ───────────────────────── tool input DTO ─────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunSubagentInput {
    prompt: String,
    #[serde(default)]
    allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    run_in_background: bool,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunSkillInput {
    name: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

fn parse_input<T: for<'de> Deserialize<'de>>(inv: &ToolInvocation) -> Result<T, ToolHandlerOutput> {
    serde_json::from_value::<T>(inv.input.clone()).map_err(|e| {
        ToolHandlerOutput::err(
            serde_json::json!({ "reason": "invalid_input", "message": e.to_string() }),
            ErrorCode::InvalidInput,
        )
    })
}

fn err_out(code: ErrorCode, msg: impl Into<String>) -> ToolHandlerOutput {
    let reason = serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    ToolHandlerOutput::err(
        serde_json::json!({ "reason": reason, "message": msg.into() }),
        code,
    )
}

// ───────────────────────── run_subagent handler ─────────────────────────

async fn handle_run_subagent(
    handle: ForkHandle,
    rt: ForkRuntime,
    inv: ToolInvocation,
) -> ToolHandlerOutput {
    let input: RunSubagentInput = match parse_input(&inv) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if input.prompt.trim().is_empty() {
        return err_out(ErrorCode::InvalidInput, "prompt must not be empty");
    }
    let description = input
        .description
        .clone()
        .unwrap_or_else(|| first_line(&input.prompt, 80));

    spawn_or_run(
        &handle,
        &rt,
        &description,
        &input.prompt,
        None,
        input.allowed_tools.as_deref(),
        input.run_in_background,
    )
    .await
}

// ───────────────────────── run_skill handler ─────────────────────────

async fn handle_run_skill(
    handle: ForkHandle,
    rt: ForkRuntime,
    inv: ToolInvocation,
) -> ToolHandlerOutput {
    let input: RunSkillInput = match parse_input(&inv) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if !is_valid_skill_name(&input.name) {
        return err_out(
            ErrorCode::InvalidInput,
            "skill name must match ^[a-z0-9][a-z0-9-]*$",
        );
    }
    // 取 SKILL.md 全文作子 run 的 prompt（spec §3.5 / §3.6：以 SKILL.md 为 prompt）。
    let body = match handle.skill_store.read_body(&input.name) {
        Ok(c) => c,
        Err(code) => return err_out(code, format!("skill not found: {}", input.name)),
    };
    let skill_dir = handle
        .skill_store
        .skill_md_path(&input.name)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // system_prompt = SKILL.md 全文 + skill 目录路径（+ 可选 args）——子 agent 据此用
    // read_file / run_bash 读 references / 跑 scripts（spec §3.6 三级渐进披露 ③）。
    let mut sys = format!(
        "你正在执行 skill「{}」。以下是它的完整 SKILL.md，严格按它行事。\n\
         skill 目录（references / scripts / assets 在此）：{}\n\n{}",
        input.name, skill_dir, body
    );
    if let Some(args) = &input.args {
        sys.push_str("\n\n【调用参数 args】\n");
        sys.push_str(&serde_json::to_string_pretty(args).unwrap_or_default());
    }
    let prompt = "请执行上述 skill，完成后用一段文本给出最终结果。";

    // run_skill 永远前台（产品契约：`{name, result}`）。
    let agent_id = new_agent_id();
    let conv_preview = format!("skill:{}", input.name);
    handle.tasks.register(
        &agent_id,
        &rt.parent_run_id,
        &conv_preview,
        &format!("skill {}", input.name),
        None,
    );
    match run_forked_agent(&handle, &rt, &agent_id, prompt, Some(&sys), None).await {
        Ok(result) => ToolHandlerOutput::ok(serde_json::json!({
            "name": input.name,
            "result": result,
        })),
        Err(e) => {
            handle.tasks.finish(
                &agent_id,
                SubAgentStatus::Failed,
                SubAgentProgress::default(),
                None,
            );
            err_out(ErrorCode::ProviderUnavailable, format!("skill run failed: {e}"))
        }
    }
}

/// 前台阻塞跑 / 后台 spawn——`run_subagent` 两种模式共用。`rt` 是**当前 run** 的 fork 运行时上下文
/// （由 dispatch 注入 / 退回 handle 默认解析得到）。
async fn spawn_or_run(
    handle: &ForkHandle,
    rt: &ForkRuntime,
    description: &str,
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    background: bool,
) -> ToolHandlerOutput {
    // 不嵌套预检（对齐 CC `isInForkChild` 布尔）：`rt` 是**当前发起 run** 的运行时。若它本身已是
    // 子 agent（is_subagent==true）→ 拒绝再 spawn（run_forked_agent 内会再判一次兜底）。只有顶层能 fork。
    // 没有任何数字比较。
    if rt.is_subagent {
        return err_out(
            ErrorCode::InvalidInput,
            "nested sub-agent not allowed; only the top-level agent can fork".to_string(),
        );
    }

    let agent_id = new_agent_id();

    if background {
        // 后台：spawn 子 run，立即返回 agentId；完成时 emit <task-notification>。
        // 先 register（避免子 run 瞬时完成时回调早于 register 的竞态），spawn 后再补挂 abort 句柄。
        handle.tasks.register(
            &agent_id,
            &rt.parent_run_id,
            "", // 后台任务的 conversation_id 在子 run 内部生成；注册表只记 parent 关联。
            description,
            None,
        );
        // 把**发起方**运行时（is_subagent=false）移进后台任务；run_forked_agent 内派生子 run 运行时。
        let initiator_rt = rt.clone();
        let child_handle = handle.clone();
        let prompt_owned = prompt.to_string();
        let sys_owned = system_prompt.map(|s| s.to_string());
        let allowed_owned: Option<Vec<String>> = allowed_tools.map(|a| a.to_vec());
        let tasks = handle.tasks.clone();
        let parent_tx = rt.parent_event_tx.clone();
        let parent_run_id = rt.parent_run_id.clone();
        let aid = agent_id.clone();

        let join = tokio::spawn(async move {
            let res = run_forked_agent(
                &child_handle,
                &initiator_rt,
                &aid,
                &prompt_owned,
                sys_owned.as_deref(),
                allowed_owned.as_deref(),
            )
            .await;
            // 完成通知（防重复）：emit <task-notification> 到父 event_tx。
            if tasks.take_notify_flag(&aid) {
                let (status_str, usage) = match &res {
                    Ok(_) => {
                        let (st, pr, _) = tasks
                            .snapshot(&aid)
                            .unwrap_or((SubAgentStatus::Completed, SubAgentProgress::default(), None));
                        (st.as_str().to_string(), pr)
                    }
                    Err(e) => {
                        tasks.finish(&aid, SubAgentStatus::Failed, SubAgentProgress::default(), Some(e.to_string()));
                        ("failed".to_string(), SubAgentProgress::default())
                    }
                };
                if let Some(tx) = &parent_tx {
                    let note = format!(
                        "<task-notification agent_id=\"{}\"><status>{}</status><usage tokens=\"{}\" tool_uses=\"{}\"/></task-notification>",
                        aid, status_str, usage.tokens, usage.tool_uses
                    );
                    let _ = tx
                        .send(AgentEvent::TextDelta {
                            run_id: parent_run_id.clone(),
                            delta: note,
                        })
                        .await;
                }
            }
        });
        // 补挂 abort 句柄（供 stop_subagent）；若子 run 已瞬时完成，set_abort 会跳过。
        handle.tasks.set_abort(&agent_id, join.abort_handle());
        ToolHandlerOutput::ok(serde_json::json!({
            "agentId": agent_id,
            "status": "running",
            "background": true,
        }))
    } else {
        // 前台：阻塞跑完，结果作为 tool_result 返回。
        handle.tasks.register(
            &agent_id,
            &rt.parent_run_id,
            "",
            description,
            None,
        );
        match run_forked_agent(&handle, rt, &agent_id, prompt, system_prompt, allowed_tools)
            .await
        {
            Ok(result) => ToolHandlerOutput::ok(serde_json::json!({
                "agentId": agent_id,
                "result": result,
            })),
            Err(e) => {
                handle.tasks.finish(
                    &agent_id,
                    SubAgentStatus::Failed,
                    SubAgentProgress::default(),
                    None,
                );
                err_out(ErrorCode::ProviderUnavailable, format!("subagent run failed: {e}"))
            }
        }
    }
}

// ───────────────────────── 管理 API（spec §3.5） ─────────────────────────

/// `stop_subagent(agentId)`：abort 一个运行中的子 run（spec §3.5）。返回是否成功。
pub fn stop_subagent(handle: &ForkHandle, agent_id: &str) -> bool {
    handle.tasks.mark_killed(agent_id)
}

/// `subagent_output(agentId)`：读后台任务的进度 / 已产出（spec §3.5）。
pub fn subagent_output(handle: &ForkHandle, agent_id: &str) -> Option<serde_json::Value> {
    handle.tasks.snapshot(agent_id).map(|(st, pr, result)| {
        serde_json::json!({
            "agentId": agent_id,
            "status": st.as_str(),
            "progress": {
                "tokens": pr.tokens,
                "toolUses": pr.tool_uses,
                "durationMs": pr.duration_ms,
            },
            "result": result,
        })
    })
}

// ───────────────────────── helpers ─────────────────────────

fn new_agent_id() -> String {
    format!("sub_{}", Uuid::new_v4())
}

/// spawn 类工具（会再起一个子 run，构成嵌套 fork）。子 registry 一律剔除这些，禁止嵌套。
/// 注意：`create_skill` 是写文件、不递归，**不算** spawn，保留给子 agent。
fn is_spawn_tool(name: &str) -> bool {
    matches!(name, "run_subagent" | "run_skill")
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    line.chars().take(max).collect()
}

/// skill name slug 校验（与 skill_tools 一致）：`^[a-z0-9][a-z0-9-]*$`。
fn is_valid_skill_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let first = name.chars().next().unwrap();
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// ───────────────────────── 注册 ─────────────────────────

/// fork tool 的 handler。持有 **静态依赖**（`ForkHandle`）+ 一个分发标记（哪个 tool）。
///
/// 关键：它**不捕获**运行时上下文（channel / is_subagent / parent_run_id / event_tx）——这些由
/// `invoke_with_ext` 在 dispatch 时从 `ext`（`ForkRuntime`）解析（带了用注入的真实值；没带退回
/// `ForkHandle` 默认）。这正是修掉「注册时静态捕获 → 嵌套 fork is_subagent 标记丢失 / channel 用占位」的根因。
struct ForkToolHandler {
    handle: ForkHandle,
    which: ForkTool,
}

#[derive(Clone, Copy)]
enum ForkTool {
    Subagent,
    Skill,
}

impl ToolHandler for ForkToolHandler {
    fn invoke(&self, inv: ToolInvocation) -> ToolHandlerFuture {
        // 无 ext 路径（如直接走 `dispatch_tool_call`）：退回 handle 默认运行时上下文。
        self.invoke_with_ext(inv, None)
    }

    fn invoke_with_ext(
        &self,
        inv: ToolInvocation,
        ext: Option<DispatchExt>,
    ) -> ToolHandlerFuture {
        let handle = self.handle.clone();
        let rt = handle.resolve_rt(&ext);
        let which = self.which;
        Box::pin(async move {
            match which {
                ForkTool::Subagent => handle_run_subagent(handle, rt, inv).await,
                ForkTool::Skill => handle_run_skill(handle, rt, inv).await,
            }
        }) as ToolHandlerFuture
    }
}

/// 把 `run_subagent` / `run_skill` 注册进 registry（spec §3.6，bootstrap 默认注册）。
///
/// `handle` 注入 fork 执行底座所需的**静态依赖**（registry / provider 工厂 / repo / 任务注册表 /
/// SkillStore + 默认运行时配置）。注意：传入的 `registry` 通常**就是** `handle.registry`——
/// `run_subagent`/`run_skill` 注册进同一个父 registry。子 run 继承父工具集时**剔除这两个 spawn 工具**
/// （见 `child_registry`），故子 agent 不能再 fork（不嵌套，对齐 Claude Code）。
///
/// handler **不静态捕获**运行时上下文：发起 run 时由 `ForkRuntime`（经 `DispatchExt`）注入，
/// dispatch 时解析。Phase 3 接线只需发起 run 时填 `ForkRuntime`，无需 registry replace。
pub fn register_subagent_tools(
    registry: &ToolRegistry,
    handle: ForkHandle,
) -> Result<(), RegisterError> {
    registry.register_tool(
        tool_spec_run_subagent(),
        Arc::new(ForkToolHandler {
            handle: handle.clone(),
            which: ForkTool::Subagent,
        }),
    )?;
    registry.register_tool(
        tool_spec_run_skill(),
        Arc::new(ForkToolHandler {
            handle,
            which: ForkTool::Skill,
        }),
    )?;
    Ok(())
}

fn tool_spec_run_subagent() -> ToolSpec {
    ToolSpec::new(
        "run_subagent",
        "fork 一个隔离子 agent 跑子任务：在全新上下文里再跑一遍 loop（继承当前 model / 工具集），\
         只把最终结果带回。prompt 是给子 agent 的任务说明。allowedTools 给了就把子 agent 的工具集\
         收紧到该子集（如只读）。runInBackground=true 时立即返回 agentId（异步并发跑，完成时收到\
         <task-notification>）；默认前台阻塞，结果作为 tool_result 返回。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "给子 agent 的任务说明（非空）" },
                "allowedTools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "收紧子 agent 工具集到该子集；省略 = 继承当前全部工具"
                },
                "runInBackground": { "type": "boolean", "description": "true=后台异步；默认 false=前台阻塞" },
                "description": { "type": "string", "description": "一句话任务描述（进任务注册表；省略取 prompt 首行）" }
            },
            "required": ["prompt"]
        }),
        vec![r#"<use_tool name="run_subagent">{"prompt":"扫一遍今日动量候选并给出 3 个标的","allowedTools":["fetch_quotes"]}</use_tool>"#.into()],
        SUBAGENT_TIMEOUT_MS,
        SideEffect::None,
    )
}

fn tool_spec_run_skill() -> ToolSpec {
    ToolSpec::new(
        "run_skill",
        "fork 一个子 agent 执行某个 skill（playbook）：以该 skill 的 SKILL.md 全文为引导跑一个隔离\
         子 run（正文不进当前上下文），只把最终结果带回。name 见 system prompt 的「可用 Skill」索引。\
         可选 args 作为调用参数传给子 agent。不存在 → not_found。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "skill slug（见 system prompt 的可用 Skill 索引）" },
                "args": { "description": "可选调用参数（任意 JSON）" }
            },
            "required": ["name"]
        }),
        vec![r#"<use_tool name="run_skill">{"name":"momentum-scan"}</use_tool>"#.into()],
        SKILL_TIMEOUT_MS,
        SideEffect::None,
    )
}

// ───────────────────────── tests (hermetic, no network) ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::{ContextBundle, WireFormat};
    use crate::infrastructure::agent::loop_executor::ProviderTurnOutcome;
    use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::agent::tool_parser::{ParserEvent, ToolCallParser};
    use crate::infrastructure::agent::tool_registry::{FnToolHandler, ToolHandler};
    use crate::infrastructure::db::{run_migrations, AppDb};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc::Sender;

    fn channel() -> ProviderChannel {
        ProviderChannel {
            channel_id: "fake".into(),
            provider: "fake".into(),
            wire_format: WireFormat::Messages,
            base_url: None,
            api_key: String::new(),
            model: "fake-model".into(),
            stream: true,
            enabled: true,
            supports_vision: false,
            supports_thinking: false,
            max_output_tokens: None,
            context_window_tokens: None,
            thinking_budget_tokens: None,
        }
    }

    fn scripted_outcome(text: &str, stop: AgentStopReason) -> ProviderTurnOutcome {
        let mut parser = ToolCallParser::new();
        let mut events = parser.feed(text);
        events.extend(parser.finalize());
        let tool_events: Vec<ParserEvent> = events
            .into_iter()
            .filter(|e| !matches!(e, ParserEvent::TextDelta(_)))
            .collect();
        ProviderTurnOutcome {
            text: text.to_string(),
            usage_input: 3,
            usage_output: 5,
            stop_reason: stop,
            tool_events,
        }
    }

    /// Scripted provider: replays one turn of fixed text. emit clean TextDelta (mirrors HttpProvider).
    struct ScriptedProvider {
        outcome: Option<ProviderTurnOutcome>,
        delay_ms: u64,
    }
    #[async_trait::async_trait]
    impl ProviderStream for ScriptedProvider {
        async fn next_turn(
            &mut self,
            _messages: &[AgentMessage],
            _context: &ContextBundle,
            event_tx: &Sender<AgentEvent>,
            run_id: &str,
        ) -> Result<ProviderTurnOutcome, LoopError> {
            if self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            let out = self
                .outcome
                .take()
                .ok_or_else(|| LoopError::Provider("scripted exhausted".into()))?;
            let mut parser = ToolCallParser::new();
            let mut events = parser.feed(&out.text);
            events.extend(parser.finalize());
            for ev in events {
                if let ParserEvent::TextDelta(s) = ev {
                    let _ = event_tx
                        .send(AgentEvent::TextDelta {
                            run_id: run_id.to_string(),
                            delta: s,
                        })
                        .await;
                }
            }
            Ok(out)
        }
    }

    /// A provider factory that hands out a fresh scripted provider replaying `text` (single turn).
    fn fixed_factory(text: &'static str) -> ProviderFactory {
        Arc::new(move |_ch: &ProviderChannel| {
            Ok(Box::new(ScriptedProvider {
                outcome: Some(scripted_outcome(text, AgentStopReason::Completed)),
                delay_ms: 0,
            }) as Box<dyn ProviderStream>)
        })
    }

    fn slow_factory(text: &'static str, delay_ms: u64) -> ProviderFactory {
        Arc::new(move |_ch: &ProviderChannel| {
            Ok(Box::new(ScriptedProvider {
                outcome: Some(scripted_outcome(text, AgentStopReason::Completed)),
                delay_ms,
            }) as Box<dyn ProviderStream>)
        })
    }

    fn fresh_repo() -> AgentMessagesRepo {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        AgentMessagesRepo::new(db)
    }

    fn skill_store_with(name: &str, desc: &str, body: &str) -> (SkillStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("gangzi-subagent-skill-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = SkillStore::new(dir.clone());
        let md = store.skill_md_path(name);
        std::fs::create_dir_all(md.parent().unwrap()).unwrap();
        let content =
            crate::infrastructure::agent::skill_store::render_skill_md(name, desc, body);
        std::fs::write(&md, content).unwrap();
        (store, dir)
    }

    fn empty_skill_store() -> (SkillStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("gangzi-subagent-empty-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        (SkillStore::new(dir.clone()), dir)
    }

    fn base_handle(factory: ProviderFactory, repo: Option<AgentMessagesRepo>) -> ForkHandle {
        let (store, _dir) = empty_skill_store();
        ForkHandle::new(
            Arc::new(ToolRegistry::new_without_persist()),
            factory,
            repo,
            None,
            channel(),
            store,
            SubAgentTaskRegistry::new(),
        )
        .for_parent_run("parent-run", None)
    }

    /// A fork runtime for an initiating run, parent_run_id = "parent-run".
    /// `is_subagent=false` = top-level (may fork); `true` = already a sub-agent (may NOT fork).
    fn base_rt(is_subagent: bool) -> ForkRuntime {
        ForkRuntime::new(channel(), "parent-run").with_is_subagent(is_subagent)
    }

    // ---- run_forked_agent: returns the child's final assistant text ----
    #[tokio::test]
    async fn run_forked_agent_returns_child_final_text() {
        let handle = base_handle(fixed_factory("子 agent 的结论：买入 600519"), None);
        // register so run_forked_agent's terminal `finish` has a task to update.
        handle.tasks.register("sub_1", "parent-run", "", "analyze", None);
        let out = run_forked_agent(&handle, &base_rt(false), "sub_1", "去分析", None, None)
            .await
            .unwrap();
        assert!(out.contains("子 agent 的结论"));
        let (st, _pr, result) = handle.tasks.snapshot("sub_1").unwrap();
        assert_eq!(st, SubAgentStatus::Completed);
        assert!(result.unwrap().contains("子 agent 的结论"));
    }

    // ---- isolation: child uses a brand-new fork conversation_id (parent linkage encoded) ----
    #[tokio::test]
    async fn child_uses_fresh_fork_conversation_id() {
        let repo = fresh_repo();
        let handle = base_handle(fixed_factory("done"), Some(repo.clone()));
        handle
            .tasks
            .register("sub_iso", "parent-run", "", "iso", None);
        run_forked_agent(&handle, &base_rt(false), "sub_iso", "hi", None, None)
            .await
            .unwrap();
        // The child's messages were persisted under a `fork:<parent>:...` conversation_id,
        // distinct from any parent conversation (isolation). Find them by run_id.
        let msgs = repo.load_messages_by_run("sub_iso").unwrap();
        assert!(!msgs.is_empty(), "child run must persist messages");
        let conv = msgs[0].conversation_id.clone().unwrap();
        assert!(
            conv.starts_with("fork:sub_iso:"),
            "child conversation_id must encode parent linkage, got {conv}"
        );
    }

    // ---- allowed_tools tightening: child registry only carries the subset ----
    #[tokio::test]
    async fn allowed_tools_tightens_child_registry() {
        // parent registry has two tools: keep + drop.
        let parent = Arc::new(ToolRegistry::new_without_persist());
        let mk = |name: &str| {
            ToolSpec::new(
                name,
                "t",
                serde_json::json!({"type":"object"}),
                vec![format!(r#"<use_tool name="{name}">{{}}</use_tool>"#)],
                5000,
                SideEffect::None,
            )
        };
        let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(|inv: ToolInvocation| {
            Box::pin(async move { ToolHandlerOutput::ok(inv.input) }) as ToolHandlerFuture
        }));
        parent.register_tool(mk("keep"), handler.clone()).unwrap();
        parent.register_tool(mk("drop"), handler).unwrap();

        let (store, _d) = empty_skill_store();
        let handle = ForkHandle::new(
            parent.clone(),
            fixed_factory("done"),
            None,
            None,
            channel(),
            store,
            SubAgentTaskRegistry::new(),
        );
        // None → inherit full parent.
        let full = handle.child_registry(None).unwrap();
        assert!(full.has_tool("keep") && full.has_tool("drop"));
        // Some(["keep"]) → only keep.
        let tight = handle
            .child_registry(Some(&["keep".to_string()]))
            .unwrap();
        assert!(tight.has_tool("keep"));
        assert!(!tight.has_tool("drop"), "drop must be filtered out");
    }

    // ---- no nesting: an initiator that is already a sub-agent (is_subagent=true) is refused ----
    // (boolean guard, aligned with Claude Code `isInForkChild`; no depth counting). ----
    #[tokio::test]
    async fn subagent_flag_refuses_nested_fork() {
        let handle = base_handle(fixed_factory("x"), None);
        // An initiator already inside a sub-agent (is_subagent=true) must NOT be able to fork —
        // the defensive guard refuses it (for when the spawn tools were somehow not stripped).
        let err = run_forked_agent(&handle, &base_rt(true), "sub_deep", "go", None, None)
            .await
            .expect_err("must refuse fork when initiator is already a sub-agent");
        match err {
            LoopError::Provider(msg) => assert!(
                msg.contains("nested sub-agent not allowed"),
                "got: {msg}"
            ),
            other => panic!("unexpected error: {other:?}"),
        }
        // The top-level initiator (is_subagent=false) is NOT refused.
        handle.tasks.register("sub_ok", "parent-run", "", "ok", None);
        run_forked_agent(&handle, &base_rt(false), "sub_ok", "go", None, None)
            .await
            .expect("a top-level agent's fork must be allowed");
    }

    // ---- ForkRuntime::child marks the derived runtime as a sub-agent (boolean, no depth) ----
    #[tokio::test]
    async fn fork_runtime_child_marks_is_subagent() {
        let rt = base_rt(false);
        assert!(!rt.is_subagent());
        let child = rt.child("sub_x");
        assert!(child.is_subagent());
        assert_eq!(child.parent_run_id, "sub_x");
        // A child stays a sub-agent (idempotent — there is no level counting).
        assert!(child.child("sub_y").is_subagent());
    }

    // ---- spawn_or_run preflight: a sub-agent initiator is rejected with invalid_input ----
    #[tokio::test]
    async fn spawn_or_run_rejects_subagent_initiator() {
        let handle = base_handle(fixed_factory("x"), None);
        let out = spawn_or_run(
            &handle,
            &base_rt(true), // already a sub-agent
            "desc",
            "go",
            None,
            None,
            false,
        )
        .await;
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));
    }

    // ---- run_subagent tool: foreground blocks and returns result ----
    #[tokio::test]
    async fn run_subagent_foreground_returns_result() {
        let handle = base_handle(fixed_factory("前台结果 42"), None);
        let registry = ToolRegistry::new_without_persist();
        register_subagent_tools(&registry, handle.clone()).unwrap();
        assert!(registry.has_tool("run_subagent"));
        assert!(registry.has_tool("run_skill"));

        let out = registry
            .dispatch_tool_call(
                "parent-run",
                "tc_1".into(),
                "run_subagent",
                serde_json::json!({ "prompt": "算个数" }),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.output_summary);
        assert!(out.output_summary["result"]
            .as_str()
            .unwrap()
            .contains("前台结果 42"));
        assert!(out.output_summary["agentId"].as_str().is_some());
    }

    // ---- run_subagent empty prompt → invalid_input ----
    #[tokio::test]
    async fn run_subagent_rejects_empty_prompt() {
        let handle = base_handle(fixed_factory("x"), None);
        let registry = ToolRegistry::new_without_persist();
        register_subagent_tools(&registry, handle).unwrap();
        let out = registry
            .dispatch_tool_call(
                "parent-run",
                "tc_e".into(),
                "run_subagent",
                serde_json::json!({ "prompt": "  " }),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));
    }

    // ---- background spawn: returns agentId immediately + emits <task-notification> on completion ----
    #[tokio::test]
    async fn run_subagent_background_notifies_on_completion() {
        let (parent_tx, mut parent_rx) = mpsc::channel::<AgentEvent>(64);
        let handle = base_handle(slow_factory("后台完成", 20), None)
            .for_parent_run("parent-run", Some(parent_tx));
        let registry = ToolRegistry::new_without_persist();
        register_subagent_tools(&registry, handle.clone()).unwrap();

        let out = registry
            .dispatch_tool_call(
                "parent-run",
                "tc_bg".into(),
                "run_subagent",
                serde_json::json!({ "prompt": "后台跑", "runInBackground": true }),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(out.output_summary["background"], true);
        let agent_id = out.output_summary["agentId"].as_str().unwrap().to_string();

        // Wait for the <task-notification> on the parent channel.
        let mut got_note = false;
        while let Some(ev) = parent_rx.recv().await {
            if let AgentEvent::TextDelta { delta, .. } = ev {
                if delta.contains("<task-notification") && delta.contains(&agent_id) {
                    assert!(delta.contains("<status>completed</status>"));
                    got_note = true;
                    break;
                }
            }
        }
        assert!(got_note, "background completion must emit a <task-notification>");
        // subagent_output now reports completed with a result.
        let snap = subagent_output(&handle, &agent_id).unwrap();
        assert_eq!(snap["status"], "completed");
        assert!(snap["result"].as_str().unwrap().contains("后台完成"));
    }

    // ---- stop_subagent aborts a running background task ----
    #[tokio::test]
    async fn stop_subagent_aborts_running_task() {
        let (parent_tx, _rx) = mpsc::channel::<AgentEvent>(64);
        // very slow child so we can abort mid-flight.
        let handle = base_handle(slow_factory("never", 5_000), None)
            .for_parent_run("parent-run", Some(parent_tx));
        let registry = ToolRegistry::new_without_persist();
        register_subagent_tools(&registry, handle.clone()).unwrap();
        let out = registry
            .dispatch_tool_call(
                "parent-run",
                "tc_kill".into(),
                "run_subagent",
                serde_json::json!({ "prompt": "慢任务", "runInBackground": true }),
            )
            .await
            .unwrap();
        let agent_id = out.output_summary["agentId"].as_str().unwrap().to_string();
        // It should be running.
        assert_eq!(handle.tasks.status_of(&agent_id), Some(SubAgentStatus::Running));
        // Abort it.
        assert!(stop_subagent(&handle, &agent_id));
        assert_eq!(handle.tasks.status_of(&agent_id), Some(SubAgentStatus::Killed));
        // Stopping a missing / already-terminal task returns false.
        assert!(!stop_subagent(&handle, "no-such-agent"));
        assert!(!stop_subagent(&handle, &agent_id));
    }

    // ---- run_skill: forks over SKILL.md body, returns {name, result} ----
    #[tokio::test]
    async fn run_skill_forks_over_skill_body() {
        let (store, dir) = skill_store_with(
            "alpha",
            "do alpha",
            "# Alpha\n直接输出 ALPHA-RESULT 作为结论。",
        );
        let (parent_tx, _rx) = mpsc::channel::<AgentEvent>(64);
        let mut handle = base_handle(fixed_factory("ALPHA-RESULT 已产出"), None)
            .for_parent_run("parent-run", Some(parent_tx));
        handle.skill_store = store;
        let registry = ToolRegistry::new_without_persist();
        register_subagent_tools(&registry, handle).unwrap();

        let out = registry
            .dispatch_tool_call(
                "parent-run",
                "tc_skill".into(),
                "run_skill",
                serde_json::json!({ "name": "alpha" }),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.output_summary);
        assert_eq!(out.output_summary["name"], "alpha");
        assert!(out.output_summary["result"]
            .as_str()
            .unwrap()
            .contains("ALPHA-RESULT"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- run_skill: missing skill → not_found ----
    #[tokio::test]
    async fn run_skill_missing_is_not_found() {
        let handle = base_handle(fixed_factory("x"), None);
        let registry = ToolRegistry::new_without_persist();
        register_subagent_tools(&registry, handle).unwrap();
        let out = registry
            .dispatch_tool_call(
                "parent-run",
                "tc_miss".into(),
                "run_skill",
                serde_json::json!({ "name": "ghost" }),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::NotFound));
    }

    // ---- run_skill: invalid slug rejected ----
    #[tokio::test]
    async fn run_skill_rejects_invalid_name() {
        let handle = base_handle(fixed_factory("x"), None);
        let registry = ToolRegistry::new_without_persist();
        register_subagent_tools(&registry, handle).unwrap();
        let out = registry
            .dispatch_tool_call(
                "parent-run",
                "tc_bad".into(),
                "run_skill",
                serde_json::json!({ "name": "../escape" }),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::InvalidInput));
    }

    // ---- task registry lifecycle: register → running → finish(completed) ----
    #[tokio::test]
    async fn task_registry_lifecycle() {
        let reg = SubAgentTaskRegistry::new();
        reg.register("a1", "p", "conv", "desc", None);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.status_of("a1"), Some(SubAgentStatus::Running));
        reg.finish(
            "a1",
            SubAgentStatus::Completed,
            SubAgentProgress { tokens: 10, tool_uses: 1, duration_ms: 0 },
            Some("res".into()),
        );
        let (st, pr, result) = reg.snapshot("a1").unwrap();
        assert_eq!(st, SubAgentStatus::Completed);
        assert_eq!(pr.tokens, 10);
        assert_eq!(result.as_deref(), Some("res"));
        // notify flag is one-shot.
        assert!(reg.take_notify_flag("a1"));
        assert!(!reg.take_notify_flag("a1"));
    }

    // ---- parallel background spawns run concurrently and each notifies ----
    #[tokio::test]
    async fn parallel_background_spawns_each_notify() {
        let (parent_tx, mut parent_rx) = mpsc::channel::<AgentEvent>(64);
        let handle = base_handle(slow_factory("ok", 15), None)
            .for_parent_run("parent-run", Some(parent_tx));
        let registry = Arc::new(ToolRegistry::new_without_persist());
        register_subagent_tools(&registry, handle.clone()).unwrap();

        let n = 3;
        let counter = Arc::new(AtomicUsize::new(0));
        for _ in 0..n {
            let out = registry
                .dispatch_tool_call(
                    "parent-run",
                    ToolRegistry::new_tool_call_id(),
                    "run_subagent",
                    serde_json::json!({ "prompt": "并发", "runInBackground": true }),
                )
                .await
                .unwrap();
            assert_eq!(out.output_summary["background"], true);
        }
        // Collect notifications.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while counter.load(Ordering::SeqCst) < n {
            tokio::select! {
                ev = parent_rx.recv() => {
                    match ev {
                        Some(AgentEvent::TextDelta { delta, .. }) if delta.contains("<task-notification") => {
                            counter.fetch_add(1, Ordering::SeqCst);
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        assert_eq!(counter.load(Ordering::SeqCst), n, "each background spawn must notify once");
    }

    // ───────── No nesting: child has no fork tools; nested fork attempts are rejected ─────────

    /// Provider that, on its FIRST turn of a run, emits `<use_tool name="run_subagent">` (attempting
    /// to fork a nested child via real `<use_tool>` dispatch), then on subsequent turns emits plain
    /// text to terminate. Each run gets a FRESH instance. Used to prove a child run CANNOT nest:
    /// since the child registry has no `run_subagent`, the dispatch is rejected as an unregistered
    /// tool and no second-level child is ever created.
    struct NestingForkProvider {
        turn: u32,
    }
    #[async_trait::async_trait]
    impl ProviderStream for NestingForkProvider {
        async fn next_turn(
            &mut self,
            _messages: &[AgentMessage],
            _context: &ContextBundle,
            event_tx: &Sender<AgentEvent>,
            run_id: &str,
        ) -> Result<ProviderTurnOutcome, LoopError> {
            self.turn += 1;
            let (text, stop) = if self.turn == 1 {
                (
                    r#"<use_tool name="run_subagent">{"prompt":"go deeper"}</use_tool>"#.to_string(),
                    AgentStopReason::ProviderStop,
                )
            } else {
                ("done".to_string(), AgentStopReason::Completed)
            };
            // Emit clean TextDelta like a real provider (XML suppressed).
            let mut parser = ToolCallParser::new();
            let mut events = parser.feed(&text);
            events.extend(parser.finalize());
            for ev in events {
                if let ParserEvent::TextDelta(s) = ev {
                    let _ = event_tx
                        .send(AgentEvent::TextDelta {
                            run_id: run_id.to_string(),
                            delta: s,
                        })
                        .await;
                }
            }
            Ok(scripted_outcome(&text, stop))
        }
    }

    fn nesting_factory() -> ProviderFactory {
        Arc::new(|_ch: &ProviderChannel| {
            Ok(Box::new(NestingForkProvider { turn: 0 }) as Box<dyn ProviderStream>)
        })
    }

    /// No nesting (the core invariant): the child run's registry must NOT contain the spawn tools
    /// `run_subagent` / `run_skill` (so the child's system prompt has no way to fork), while still
    /// keeping other tools (e.g. `create_skill`, `read_file`). Additionally, when a scripted child
    /// nonetheless emits `<use_tool name="run_subagent">`, the dispatch is rejected (unregistered
    /// tool) and NO second-level child run is created — proving forks fan out only one level deep.
    #[tokio::test]
    async fn subagent_has_no_fork_tools_no_nesting() {
        // 1) Direct structural check on child_registry: spawn tools stripped, others kept.
        let parent = Arc::new(ToolRegistry::new_without_persist());
        let mk = |name: &str| {
            ToolSpec::new(
                name,
                "t",
                serde_json::json!({"type":"object"}),
                vec![format!(r#"<use_tool name="{name}">{{}}</use_tool>"#)],
                5000,
                SideEffect::None,
            )
        };
        let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(|inv: ToolInvocation| {
            Box::pin(async move { ToolHandlerOutput::ok(inv.input) }) as ToolHandlerFuture
        }));
        // Parent carries spawn tools + non-spawn tools.
        parent.register_tool(mk("run_subagent"), handler.clone()).unwrap();
        parent.register_tool(mk("run_skill"), handler.clone()).unwrap();
        parent.register_tool(mk("create_skill"), handler.clone()).unwrap();
        parent.register_tool(mk("read_file"), handler).unwrap();

        let (store, _d) = empty_skill_store();
        let handle = ForkHandle::new(
            parent.clone(),
            nesting_factory(),
            None,
            None,
            channel(),
            store,
            SubAgentTaskRegistry::new(),
        );

        // allowed=None → inherit all parent tools, but spawn tools are still stripped.
        let inherited = handle.child_registry(None).unwrap();
        assert!(!inherited.has_tool("run_subagent"), "child must NOT carry run_subagent");
        assert!(!inherited.has_tool("run_skill"), "child must NOT carry run_skill");
        assert!(inherited.has_tool("create_skill"), "create_skill is not spawn — kept");
        assert!(inherited.has_tool("read_file"), "non-spawn tools are inherited");

        // allowed explicitly lists spawn tools → still stripped (cannot be re-granted).
        let tightened = handle
            .child_registry(Some(&[
                "run_subagent".to_string(),
                "run_skill".to_string(),
                "read_file".to_string(),
            ]))
            .unwrap();
        assert!(!tightened.has_tool("run_subagent"), "spawn tool stripped even if allow-listed");
        assert!(!tightened.has_tool("run_skill"), "spawn tool stripped even if allow-listed");
        assert!(tightened.has_tool("read_file"), "allow-listed non-spawn tool kept");

        // 2) End-to-end: a top-level run forks ONE child. That child's loop emits
        //    `<use_tool name="run_subagent">`, but its registry has no such tool → rejected, no
        //    grandchild created. Exactly ONE child run is registered.
        let tasks = SubAgentTaskRegistry::new();
        let (store2, _dir2) = empty_skill_store();
        let registry = Arc::new(ToolRegistry::new_without_persist());
        let handle2 = ForkHandle::new(
            registry.clone(),
            nesting_factory(),
            None,
            None,
            channel(),
            store2,
            tasks.clone(),
        );
        register_subagent_tools(&registry, handle2.clone()).unwrap();

        // Top-level run (is_subagent=false) dispatches run_subagent once → forks a single child.
        let request = AgentRunRequest {
            run_id: "top".into(),
            trigger: "test".into(),
            channel: channel(),
            max_turns: 6,
            input: vec![AgentMessage {
                message_id: "am-top".into(),
                run_id: Some("top".into()),
                conversation_id: None,
                seq: None,
                kind: None,
                role: AgentMessageRole::User,
                blocks: vec![AgentMessageBlock::Text { text: "start".into() }],
                created_at: Utc::now(),
            }],
            conversation_id: None,
            compaction: None,
            fallback_channels: vec![],
            retry: None,
        };
        let top_rt = ForkRuntime::new(channel(), "top"); // is_subagent=false (top-level)
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(512);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let summary = run_agent_turn_forked(
            request,
            registry.clone(),
            ContextBundle::new("top"),
            vec![nesting_factory()(&channel()).unwrap()],
            tx,
            None,
            Some(top_rt.into_ext()),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        let _ = drain.await;
        assert_eq!(summary.stop_reason, AgentStopReason::Completed);

        // Exactly ONE child run was registered. The child loop's attempt to dispatch run_subagent
        // hits an empty/stripped registry → no grandchild. Forks fan out only one level deep.
        assert_eq!(
            tasks.len(),
            1,
            "only the top-level agent forks (one child); the child cannot nest (got {} tasks)",
            tasks.len()
        );
    }

    /// fork uses the channel from the INJECTED ForkRuntime (the live run's channel), not the
    /// ForkHandle's placeholder default. We assert this by injecting a runtime whose channel has a
    /// distinctive model and having the provider factory record which channel it was built over.
    #[tokio::test]
    async fn fork_uses_injected_runtime_channel_not_placeholder() {
        use std::sync::Mutex as StdMutex;
        let seen_models: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let seen = seen_models.clone();
        let factory: ProviderFactory = Arc::new(move |ch: &ProviderChannel| {
            seen.lock().unwrap().push(ch.model.clone());
            Ok(Box::new(ScriptedProvider {
                outcome: Some(scripted_outcome("ok", AgentStopReason::Completed)),
                delay_ms: 0,
            }) as Box<dyn ProviderStream>)
        });
        let (store, _dir) = empty_skill_store();
        // ForkHandle's DEFAULT channel uses model "PLACEHOLDER-MODEL".
        let mut placeholder = channel();
        placeholder.model = "PLACEHOLDER-MODEL".into();
        let handle = ForkHandle::new(
            Arc::new(ToolRegistry::new_without_persist()),
            factory,
            None,
            None,
            placeholder,
            store,
            SubAgentTaskRegistry::new(),
        )
        .for_parent_run("parent-run", None);
        let registry = ToolRegistry::new_without_persist();
        register_subagent_tools(&registry, handle).unwrap();

        // Inject a ForkRuntime whose channel uses model "LIVE-MODEL".
        let mut live = channel();
        live.model = "LIVE-MODEL".into();
        let rt = ForkRuntime::new(live, "parent-run"); // is_subagent=false (top-level)
        let out = registry
            .dispatch_tool_call_with_ext(
                "parent-run",
                "tc_live".into(),
                "run_subagent",
                serde_json::json!({ "prompt": "用真实 channel 跑" }),
                Some(rt.into_ext()),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.output_summary);

        let models = seen_models.lock().unwrap();
        assert!(
            models.iter().any(|m| m == "LIVE-MODEL"),
            "fork must build provider over the injected runtime channel, got {models:?}"
        );
        assert!(
            !models.iter().any(|m| m == "PLACEHOLDER-MODEL"),
            "fork must NOT use the ForkHandle placeholder channel, got {models:?}"
        );
    }

    // ───────── Added hermetic regressions for the 定型后 fork/skill invariants ─────────

    /// Provider that emits `<use_tool name="run_subagent">` on its FIRST turn and captures every
    /// `<tool_result>` / `<tool_error>` it is fed back on subsequent turns, then terminates. Used to
    /// prove a CHILD run that tries to nest gets a `<tool_error code="invalid_input">` (unregistered
    /// tool), not a grandchild — the end-to-end of the "no nesting" invariant (spec §3.5).
    struct NestThenCaptureProvider {
        turn: u32,
        captured: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait::async_trait]
    impl ProviderStream for NestThenCaptureProvider {
        async fn next_turn(
            &mut self,
            messages: &[AgentMessage],
            _context: &ContextBundle,
            event_tx: &Sender<AgentEvent>,
            run_id: &str,
        ) -> Result<ProviderTurnOutcome, LoopError> {
            self.turn += 1;
            // Record any tool_result/tool_error text fed in via the latest user message.
            if let Some(last) = messages.last() {
                for b in &last.blocks {
                    if let AgentMessageBlock::Text { text } = b {
                        if text.contains("<tool_error") || text.contains("<tool_result") {
                            self.captured.lock().unwrap().push(text.clone());
                        }
                    }
                }
            }
            let (text, stop) = if self.turn == 1 {
                (
                    r#"<use_tool name="run_subagent">{"prompt":"nest"}</use_tool>"#.to_string(),
                    AgentStopReason::ProviderStop,
                )
            } else {
                ("done".to_string(), AgentStopReason::Completed)
            };
            let mut parser = ToolCallParser::new();
            let mut events = parser.feed(&text);
            events.extend(parser.finalize());
            for ev in events {
                if let ParserEvent::TextDelta(s) = ev {
                    let _ = event_tx
                        .send(AgentEvent::TextDelta {
                            run_id: run_id.to_string(),
                            delta: s,
                        })
                        .await;
                }
            }
            Ok(scripted_outcome(&text, stop))
        }
    }

    /// No-nesting (end-to-end, child's view): when a child run emits `<use_tool name="run_subagent">`,
    /// its stripped registry has no such tool, so the loop feeds back a `<tool_error
    /// code="invalid_input">` (unregistered tool, spec §2 失败 code 归类) and NO grandchild is spawned.
    /// This complements `subagent_has_no_fork_tools_no_nesting` (which checks task count) by asserting
    /// the child actually receives the protocol-level rejection, so the model can self-correct.
    /// Spec: agent-infra-module.md §3.5 (不嵌套) + §2 (未注册 tool → invalid_input).
    #[tokio::test]
    async fn child_nested_fork_attempt_yields_tool_error_invalid_input() {
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_factory = captured.clone();
        let factory: ProviderFactory = Arc::new(move |_ch: &ProviderChannel| {
            Ok(Box::new(NestThenCaptureProvider {
                turn: 0,
                captured: captured_for_factory.clone(),
            }) as Box<dyn ProviderStream>)
        });

        let tasks = SubAgentTaskRegistry::new();
        let (store, _dir) = empty_skill_store();
        let registry = Arc::new(ToolRegistry::new_without_persist());
        let handle = ForkHandle::new(
            registry.clone(),
            factory,
            None,
            None,
            channel(),
            store,
            tasks.clone(),
        )
        .for_parent_run("top", None);
        register_subagent_tools(&registry, handle.clone()).unwrap();

        // Top-level run forks ONE child (is_subagent=false). The child loop emits run_subagent.
        let request = AgentRunRequest {
            run_id: "top".into(),
            trigger: "test".into(),
            channel: channel(),
            max_turns: 6,
            input: vec![AgentMessage {
                message_id: "am-top".into(),
                run_id: Some("top".into()),
                conversation_id: None,
                seq: None,
                kind: None,
                role: AgentMessageRole::User,
                blocks: vec![AgentMessageBlock::Text { text: "start".into() }],
                created_at: Utc::now(),
            }],
            conversation_id: None,
            compaction: None,
            fallback_channels: vec![],
            retry: None,
        };
        let top_rt = ForkRuntime::new(channel(), "top");
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(512);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        run_agent_turn_forked(
            request,
            registry.clone(),
            ContextBundle::new("top"),
            vec![(handle_factory_for_top())(&channel()).unwrap()],
            tx,
            None,
            Some(top_rt.into_ext()),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        let _ = drain.await;

        // Exactly one child run (no grandchild).
        assert_eq!(tasks.len(), 1, "only the top-level agent forks; child cannot nest");
        // The child was fed back a <tool_error code="invalid_input"> for its run_subagent attempt.
        let caps = captured.lock().unwrap();
        let joined = caps.join("\n");
        assert!(
            joined.contains("<tool_error") && joined.contains("run_subagent"),
            "child must receive a <tool_error> for its run_subagent attempt, got: {joined}"
        );
        assert!(
            joined.contains("invalid_input"),
            "the nested-fork tool_error must use code=invalid_input (unregistered tool), got: {joined}"
        );
    }

    /// Helper: the top-level provider for the end-to-end nesting test must itself emit run_subagent on
    /// turn 1 (to fork the child), then finish. Reuses NestingForkProvider's shape but with its OWN
    /// capture-free instance so the top run forks exactly one child.
    fn handle_factory_for_top() -> ProviderFactory {
        Arc::new(|_ch: &ProviderChannel| {
            Ok(Box::new(NestingForkProvider { turn: 0 }) as Box<dyn ProviderStream>)
        })
    }

    /// Background completion notification must be emitted on the channel carried by the **injected**
    /// ForkRuntime (the live run's parent_event_tx), not the ForkHandle's default. Proves the run-time
    /// injection path (DispatchExt → ForkRuntime) drives the notification target.
    /// Spec: agent-infra-module.md §3.5 (fork 上下文 run 时注入 + 后台 <task-notification>).
    #[tokio::test]
    async fn background_notification_uses_injected_runtime_event_tx() {
        // ForkHandle default has NO event_tx (would silently drop notifications if used).
        let handle = base_handle(slow_factory("后台完成", 15), None);
        let registry = ToolRegistry::new_without_persist();
        register_subagent_tools(&registry, handle.clone()).unwrap();

        // Inject a ForkRuntime carrying the LIVE parent event channel.
        let (live_tx, mut live_rx) = mpsc::channel::<AgentEvent>(64);
        let rt = ForkRuntime::new(channel(), "parent-run").with_event_tx(Some(live_tx));

        let out = registry
            .dispatch_tool_call_with_ext(
                "parent-run",
                "tc_bg_inj".into(),
                "run_subagent",
                serde_json::json!({ "prompt": "后台跑", "runInBackground": true }),
                Some(rt.into_ext()),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        let agent_id = out.output_summary["agentId"].as_str().unwrap().to_string();

        // The <task-notification> must arrive on the INJECTED channel.
        let mut got = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            tokio::select! {
                ev = live_rx.recv() => match ev {
                    Some(AgentEvent::TextDelta { delta, .. })
                        if delta.contains("<task-notification") && delta.contains(&agent_id) =>
                    {
                        got = true;
                        break;
                    }
                    Some(_) => {}
                    None => break,
                },
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        assert!(got, "background completion must notify on the injected runtime's event_tx");
    }

    /// SubAgentTask.progress accumulates real wall-clock duration for a sub-run (spec §3.5 progress).
    /// A deliberately slow scripted child guarantees a non-zero duration, so we can assert it is
    /// recorded (the run-time `duration_ms` is measured, not a placeholder zero).
    #[tokio::test]
    async fn subagent_progress_records_nonzero_duration() {
        let handle = base_handle(slow_factory("done", 25), None);
        handle.tasks.register("sub_dur", "parent-run", "", "dur", None);
        run_forked_agent(&handle, &base_rt(false), "sub_dur", "go", None, None)
            .await
            .unwrap();
        let (_st, pr, _r) = handle.tasks.snapshot("sub_dur").unwrap();
        assert!(
            pr.duration_ms > 0,
            "sub-run progress must record a measured (non-zero) duration_ms, got {}",
            pr.duration_ms
        );
        // tokens accumulated from the child's usage (ScriptedProvider reports 3+5).
        assert!(pr.tokens > 0, "progress must accumulate child token usage");
    }

    /// Provider that, on turn 1, emits a natural-language *preamble* AND a `<use_tool name="read_file">`
    /// (both must NOT leak to the parent: the XML mechanics for context hygiene, the preamble because
    /// only the FINAL turn's text is returned), then on turn 2 emits the final answer.
    struct NoisyThenFinalProvider {
        turn: u32,
    }
    #[async_trait::async_trait]
    impl ProviderStream for NoisyThenFinalProvider {
        async fn next_turn(
            &mut self,
            _messages: &[AgentMessage],
            _context: &ContextBundle,
            event_tx: &Sender<AgentEvent>,
            run_id: &str,
        ) -> Result<ProviderTurnOutcome, LoopError> {
            self.turn += 1;
            let (text, stop) = if self.turn == 1 {
                // Turn 1: a natural-language preamble (PREAMBLE-LET-ME-READ) BEFORE a tool call. The
                // preamble is real authored text (a clean TextDelta) — it must be discarded because it
                // is NOT the final turn. The <use_tool> XML + read_file tool_result must also not leak.
                (
                    r#"PREAMBLE-LET-ME-READ <use_tool name="read_file">{"path":"/etc/hosts"}</use_tool>"#.to_string(),
                    AgentStopReason::ProviderStop,
                )
            } else {
                ("FINAL-SKILL-ANSWER".to_string(), AgentStopReason::Completed)
            };
            let mut parser = ToolCallParser::new();
            let mut events = parser.feed(&text);
            events.extend(parser.finalize());
            for ev in events {
                if let ParserEvent::TextDelta(s) = ev {
                    let _ = event_tx
                        .send(AgentEvent::TextDelta {
                            run_id: run_id.to_string(),
                            delta: s,
                        })
                        .await;
                }
            }
            Ok(scripted_outcome(&text, stop))
        }
    }

    /// run_skill returns ONLY the child's **final-turn** text (spec §3.5 「只回末轮文本」, aligned with
    /// Claude Code「最后一条消息」). This guards two things at once:
    ///  1. tool-call mechanics — the child's `<use_tool>` XML and the `read_file` tool_result payload
    ///     must not surface in the `result`; and
    ///  2. mid-turn preamble — the child's turn-1 natural-language preamble ("PREAMBLE-LET-ME-READ")
    ///     must be discarded, because it is NOT the final turn (the accumulator is cleared on every
    ///     `ToolStart`). Only turn 2's "FINAL-SKILL-ANSWER" is returned.
    /// The SKILL.md body is the child's prompt (read via SkillStore), never inlined into the parent.
    /// Spec: agent-infra-module.md §3.5 + §3.6.
    #[tokio::test]
    async fn run_skill_result_is_final_turn_text_only() {
        let (store, dir) = skill_store_with("beta", "do beta", "# Beta\n按流程产出。");
        let factory: ProviderFactory = Arc::new(|_ch: &ProviderChannel| {
            Ok(Box::new(NoisyThenFinalProvider { turn: 0 }) as Box<dyn ProviderStream>)
        });
        // Single parent registry carries read_file (inherited by the child so the child's
        // intermediate <use_tool name="read_file"> dispatches cleanly) — the handle inherits from it
        // and `register_subagent_tools` adds run_subagent/run_skill to the very same registry.
        let registry = Arc::new(ToolRegistry::new_without_persist());
        register_local_tools_for_test(&registry);
        let handle = ForkHandle::new(
            registry.clone(),
            factory,
            None,
            None,
            channel(),
            store,
            SubAgentTaskRegistry::new(),
        )
        .for_parent_run("parent-run", None);
        register_subagent_tools(&registry, handle).unwrap();

        let out = registry
            .dispatch_tool_call(
                "parent-run",
                "tc_skill_only".into(),
                "run_skill",
                serde_json::json!({ "name": "beta" }),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.output_summary);
        let result = out.output_summary["result"].as_str().unwrap();
        assert!(
            result.contains("FINAL-SKILL-ANSWER"),
            "run_skill must return the child's final answer, got: {result}"
        );
        // The child's tool-call mechanics must not surface in the parent-visible result.
        assert!(
            !result.contains("<use_tool") && !result.contains("read_file"),
            "child's <use_tool> XML must NOT leak into the run_skill result, got: {result}"
        );
        assert!(
            !result.contains("<tool_result") && !result.contains("truncated"),
            "child's tool_result payload must NOT leak into the run_skill result, got: {result}"
        );
        // The child's turn-1 natural-language preamble must NOT appear: only the final turn is kept.
        assert!(
            !result.contains("PREAMBLE-LET-ME-READ"),
            "non-final-turn preamble must be discarded (only末轮 text returned), got: {result}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Small test helpers to compose a registry carrying read_file for the run_skill test.
    fn register_local_tools_for_test(registry: &ToolRegistry) {
        let handler: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(|_inv: ToolInvocation| {
            Box::pin(async move {
                ToolHandlerOutput::ok(serde_json::json!({ "content": "x", "truncated": false }))
            }) as ToolHandlerFuture
        }));
        let _ = registry.register_tool(
            ToolSpec::new(
                "read_file",
                "read",
                serde_json::json!({"type":"object"}),
                vec![r#"<use_tool name="read_file">{"path":"x"}</use_tool>"#.into()],
                5000,
                SideEffect::None,
            ),
            handler,
        );
    }

    /// run_subagent (foreground, multi-turn) returns ONLY the child's final-turn text. Turn 1 emits a
    /// preamble + a read_file tool call (not the final turn → must be discarded); turn 2 emits the
    /// answer. Mirrors `run_skill_result_is_final_turn_text_only` for the run_subagent path.
    /// Spec: agent-infra-module.md §3.5 (只回末轮文本).
    #[tokio::test]
    async fn run_subagent_result_is_final_turn_text_only() {
        let factory: ProviderFactory = Arc::new(|_ch: &ProviderChannel| {
            Ok(Box::new(NoisyThenFinalProvider { turn: 0 }) as Box<dyn ProviderStream>)
        });
        let registry = Arc::new(ToolRegistry::new_without_persist());
        register_local_tools_for_test(&registry); // child inherits read_file for its turn-1 call.
        let handle = ForkHandle::new(
            registry.clone(),
            factory,
            None,
            None,
            channel(),
            empty_skill_store().0,
            SubAgentTaskRegistry::new(),
        )
        .for_parent_run("parent-run", None);
        register_subagent_tools(&registry, handle).unwrap();

        let out = registry
            .dispatch_tool_call(
                "parent-run",
                "tc_sub_final".into(),
                "run_subagent",
                serde_json::json!({ "prompt": "多轮跑，最后给结论" }),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.output_summary);
        let result = out.output_summary["result"].as_str().unwrap();
        assert!(
            result.contains("FINAL-SKILL-ANSWER"),
            "run_subagent must return the child's final-turn answer, got: {result}"
        );
        assert!(
            !result.contains("PREAMBLE-LET-ME-READ"),
            "turn-1 preamble must be discarded (only末轮 text), got: {result}"
        );
        assert!(
            !result.contains("<use_tool") && !result.contains("read_file"),
            "tool-call mechanics must not leak, got: {result}"
        );
    }

    /// Provider that captures the text of the FIRST user message it is seeded with (so a test can
    /// assert the fork system-prompt hint was injected), then emits a one-turn final answer.
    struct CaptureSeedProvider {
        seen: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait::async_trait]
    impl ProviderStream for CaptureSeedProvider {
        async fn next_turn(
            &mut self,
            messages: &[AgentMessage],
            _context: &ContextBundle,
            event_tx: &Sender<AgentEvent>,
            run_id: &str,
        ) -> Result<ProviderTurnOutcome, LoopError> {
            if let Some(first) = messages.first() {
                for b in &first.blocks {
                    if let AgentMessageBlock::Text { text } = b {
                        self.seen.lock().unwrap().push(text.clone());
                    }
                }
            }
            let _ = event_tx
                .send(AgentEvent::TextDelta {
                    run_id: run_id.to_string(),
                    delta: "ok".to_string(),
                })
                .await;
            Ok(scripted_outcome("ok", AgentStopReason::Completed))
        }
    }

    /// The fork system-prompt hint (FORK_FINAL_MESSAGE_HINT) is injected into the child's seed message
    /// on BOTH fork paths (run_subagent free prompt + run_skill SKILL.md), telling the child only its
    /// last message is returned. Spec: agent-infra-module.md §3.5.
    #[tokio::test]
    async fn fork_injects_final_message_hint_into_child_prompt() {
        // ---- run_subagent path ----
        let seen_sub: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_sub_f = seen_sub.clone();
        let factory: ProviderFactory = Arc::new(move |_ch: &ProviderChannel| {
            Ok(Box::new(CaptureSeedProvider { seen: seen_sub_f.clone() }) as Box<dyn ProviderStream>)
        });
        let handle = base_handle(factory, None);
        handle.tasks.register("sub_hint", "parent-run", "", "hint", None);
        run_forked_agent(&handle, &base_rt(false), "sub_hint", "去分析 600519", None, None)
            .await
            .unwrap();
        let seed = seen_sub.lock().unwrap().join("\n");
        assert!(
            seed.contains(FORK_FINAL_MESSAGE_HINT),
            "run_subagent seed must carry the fork final-message hint, got: {seed}"
        );
        assert!(seed.contains("去分析 600519"), "the user prompt must still be present");

        // ---- run_skill path ----
        let seen_skill: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_skill_f = seen_skill.clone();
        let factory2: ProviderFactory = Arc::new(move |_ch: &ProviderChannel| {
            Ok(Box::new(CaptureSeedProvider { seen: seen_skill_f.clone() }) as Box<dyn ProviderStream>)
        });
        let (store, dir) = skill_store_with("gamma", "do gamma", "# Gamma\n产出结论。");
        let mut handle2 = base_handle(factory2, None);
        handle2.skill_store = store;
        let registry = ToolRegistry::new_without_persist();
        register_subagent_tools(&registry, handle2).unwrap();
        let out = registry
            .dispatch_tool_call(
                "parent-run",
                "tc_skill_hint".into(),
                "run_skill",
                serde_json::json!({ "name": "gamma" }),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.output_summary);
        let seed2 = seen_skill.lock().unwrap().join("\n");
        assert!(
            seed2.contains(FORK_FINAL_MESSAGE_HINT),
            "run_skill seed must carry the fork final-message hint, got: {seed2}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
