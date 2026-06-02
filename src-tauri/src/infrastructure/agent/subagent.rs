//! 子 Agent / Fork —— execution 底座（Infra 层）。
//!
//! Spec: docs/design/agent-infra-module.md §3.5 子 Agent / Fork + §3.6 Infra 默认 Tools
//!
//! fork 子 agent 是 Infra 的执行能力：在隔离上下文里再跑一遍 loop（复用 `run_agent_turn`），
//! 跑完只把**最终结果文本**带回父。纯执行机制，与业务无关。
//!
//! 本模块提供：
//! - `ForkHandle`：fork 执行底座 + 依赖注入（registry / 子 provider 工厂 / repo / channel / 注册表）。
//!   `run_subagent` / `run_skill` 两个 tool 的 handler 持有它的 `Arc`，在 `dispatch_tool_call`
//!   里触发子 run loop。
//! - `run_forked_agent`：起一个**子 run**（全新 `conversation_id` + `parent_run_id` 关联），
//!   继承父 channel / model / effort / 工具集（可用 `allowed_tools` 收紧），返回子 run 最终文本。
//! - `SubAgentTask` + `SubAgentTaskRegistry`：子 agent 任务注册表 + 生命周期（spec §3.5）。
//! - 三种执行模式：前台（阻塞）/ 后台（`run_in_background` → 立即返回 agentId + 完成时发通知）/ 并行
//!   （后台 spawn 天然并发）。
//! - `register_subagent_tools`：把 `run_subagent` / `run_skill` 注册进 registry（bootstrap 调用）。
//!
//! 隔离 / 继承 / 限深（spec §3.5 不变量）：
//! - 隔离：子用全新 `conversation_id`（`fork:<parent_run_id>:<uuid>`），不与父共享消息历史；
//!   中间 tool 调用 / 试错不进父上下文。
//! - 继承：channel / model / effort / 工具集默认继承父；`allowed_tools` 给了就收紧到子集。
//! - 限深：带 `query_depth`，超 `MAX_FORK_DEPTH` 拒绝再 fork，防无限递归。
//! - 只回结果：父对话只拿子 run 的最终 assistant 文本（后台另发 `<task-notification>`）。

use crate::domain::agent::{
    AgentEvent, AgentMessage, AgentMessageBlock, AgentMessageRole, AgentRunRequest,
    AgentStopReason, CompactionConfig, ContextBundle, ProviderChannel, RetryConfig, SideEffect,
    ToolSpec,
};
use crate::domain::shared::ErrorCode;
use crate::infrastructure::agent::http_provider::HttpProvider;
use crate::infrastructure::agent::loop_executor::{
    run_agent_turn, LoopError, ProviderStream,
};
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use crate::infrastructure::agent::payload_store::PayloadStore;
use crate::infrastructure::agent::skill_store::SkillStore;
use crate::infrastructure::agent::tool_registry::{
    FnToolHandler, RegisterError, ToolHandlerFuture, ToolHandlerOutput, ToolInvocation,
    ToolRegistry,
};
use chrono::Utc;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use uuid::Uuid;

/// 递归限深：子 agent 也能再 fork，但 `query_depth >= MAX_FORK_DEPTH` 时拒绝（spec §3.5 不变量）。
pub const MAX_FORK_DEPTH: u32 = 5;

const SUBAGENT_TIMEOUT_MS: u64 = 600_000; // 10 分钟：fork 子 run 是长任务，远超普通 tool。
const SKILL_TIMEOUT_MS: u64 = 600_000;

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

// ───────────────────────── Fork 句柄（依赖注入） ─────────────────────────

/// fork 执行底座 + 依赖注入。
///
/// `run_subagent` / `run_skill` 的 handler 持有它的 `Arc`，在 dispatch 时触发子 run loop。
/// 持有：父 registry（默认继承的工具集）、子 provider 工厂、repo、父 channel / 容错 / 压缩配置、
/// 任务注册表、SkillStore、当前 fork 深度、父 run 的 event_tx（后台完成通知用）。
#[derive(Clone)]
pub struct ForkHandle {
    /// 默认继承的工具集（父 registry）。`allowed_tools` 给了就构造收紧的子 registry。
    registry: Arc<ToolRegistry>,
    /// 子 run 的 provider 工厂（生产 = HttpProvider；测试 = scripted）。
    provider_factory: ProviderFactory,
    /// 审计落库 repo（子 run 用自己的 conversation_id + parent 关联）。
    repo: Option<AgentMessagesRepo>,
    payload_store: Option<PayloadStore>,
    /// 继承父：channel / 容错 / 压缩 / max_turns。
    channel: ProviderChannel,
    fallback_channels: Vec<ProviderChannel>,
    retry: Option<RetryConfig>,
    compaction: Option<CompactionConfig>,
    max_turns: u32,
    /// 子 agent 任务注册表（spec §3.5）。
    tasks: SubAgentTaskRegistry,
    /// Skill 存盘访问（`run_skill` 读 SKILL.md 全文作 prompt）。
    skill_store: SkillStore,
    /// 当前 fork 深度（spec §3.5 限深）。父 run = 0；每 fork 一层 +1。
    depth: u32,
    /// 父 run 标识（子 run 用它做 parent 关联）。
    parent_run_id: String,
    /// 父 run 的 event_tx（后台任务完成时 emit `<task-notification>`）。
    parent_event_tx: Option<mpsc::Sender<AgentEvent>>,
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
            channel,
            fallback_channels: vec![],
            retry: None,
            compaction: None,
            max_turns: 12,
            tasks,
            skill_store,
            depth: 0,
            parent_run_id: String::new(),
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
    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }
    /// 绑定父 run 上下文（parent_run_id + event_tx），供后台完成通知。
    pub fn for_parent_run(
        mut self,
        parent_run_id: impl Into<String>,
        parent_event_tx: Option<mpsc::Sender<AgentEvent>>,
    ) -> Self {
        self.parent_run_id = parent_run_id.into();
        self.parent_event_tx = parent_event_tx;
        self
    }

    pub fn tasks(&self) -> &SubAgentTaskRegistry {
        &self.tasks
    }
    pub fn skill_store(&self) -> &SkillStore {
        &self.skill_store
    }
    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// 给子 run 用的 ForkHandle：深度 +1，parent_run_id 改为子 run 的 id（子的子 fork 关联到子）。
    fn child_handle(&self, child_run_id: &str) -> ForkHandle {
        let mut h = self.clone();
        h.depth = self.depth + 1;
        h.parent_run_id = child_run_id.to_string();
        // 子 run 内部的 fork 完成通知不回灌父——子 run 自己消费（隔离）。
        h.parent_event_tx = None;
        h
    }

    /// 构造子 run 的 registry：`allowed_tools=None` 直接复用父；给了就收紧到子集。
    ///
    /// 收紧实现：新建一个不带持久化的 registry，把父中名字落在 `allowed` 内的 ToolSpec + handler
    /// 重新注册（含 run_subagent / run_skill 自身，若在 allowed 内）。子 registry 仍走父 repo /
    /// payload_store（dispatch 持久化），故新 registry 用父的 repo/payload_store 构造。
    fn child_registry(&self, allowed: Option<&[String]>) -> Result<Arc<ToolRegistry>, LoopError> {
        let Some(allowed) = allowed else {
            return Ok(self.registry.clone());
        };
        let child = match (&self.repo, &self.payload_store) {
            (Some(r), Some(p)) => ToolRegistry::new(r.clone(), p.clone()),
            _ => ToolRegistry::new_without_persist(),
        };
        let allow: std::collections::HashSet<&str> = allowed.iter().map(|s| s.as_str()).collect();
        for spec in self.registry.list_tools() {
            if allow.contains(spec.name.as_str()) {
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
/// 给定 `{ prompt, system_prompt?, allowed_tools?, channel(继承), parent_run_id, ... }` →
/// 起一个**子 run**（复用 `run_agent_turn`），全新 `conversation_id`（带 parent 关联），跑完返回
/// **最终 assistant 文本**。子的中间过程不回给父（隔离）。
///
/// `description` 进任务注册表；`agent_id` 是子 run 标识（也是子 run 的 run_id）。
///
/// 返回 `Ok(final_text)` 或 `Err(...)`（子 run 失败 / 限深拒绝 / provider 构造失败）。
pub async fn run_forked_agent(
    handle: &ForkHandle,
    agent_id: &str,
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
) -> Result<String, LoopError> {
    // 限深（spec §3.5 不变量）。
    if handle.depth >= MAX_FORK_DEPTH {
        return Err(LoopError::Provider(format!(
            "fork depth limit reached (depth={}, max={}); refusing to fork further",
            handle.depth, MAX_FORK_DEPTH
        )));
    }

    // 隔离上下文：全新 conversation_id，带 parent_run_id 关联（命名编码，无需 DB schema 改动）。
    let conversation_id = format!("fork:{}:{}", handle.parent_run_id, Uuid::new_v4());

    // 子 registry：默认继承父；allowed_tools 收紧。
    let registry = handle.child_registry(allowed_tools)?;

    // 子 run 的 providers：channel + fallback（继承父）。
    let mut providers: Vec<Box<dyn ProviderStream>> = Vec::new();
    providers.push((handle.provider_factory)(&handle.channel)?);
    for fb in &handle.fallback_channels {
        providers.push((handle.provider_factory)(fb)?);
    }

    // 子 run 的 input：一条 user message = prompt。
    let mut blocks = Vec::new();
    if let Some(sys) = system_prompt {
        // system_prompt 作为子 run 的引导：拼进 user prompt 前缀（隔离上下文，独立 system 由
        // SystemPromptBuilder 在 loop 内注入 tool 清单；这里把 skill 正文 / 引导塞进 user turn）。
        blocks.push(AgentMessageBlock::Text {
            text: format!("{sys}\n\n{prompt}"),
        });
    } else {
        blocks.push(AgentMessageBlock::Text {
            text: prompt.to_string(),
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
        channel: handle.channel.clone(),
        max_turns: handle.max_turns,
        input,
        conversation_id: Some(conversation_id),
        compaction: handle.compaction.clone(),
        fallback_channels: handle.fallback_channels.clone(),
        retry: handle.retry.clone(),
    };

    // 子 run 用**自己的** event 通道（隔离）：聚合 TextDelta 为最终文本；不回灌父。
    let (child_tx, mut child_rx) = mpsc::channel::<AgentEvent>(256);
    let context = ContextBundle::new(agent_id);

    let run = run_agent_turn(request, registry, context, providers, child_tx, handle.repo.clone());

    // 并行消费子事件 + 等子 run 完成。
    let collector = async {
        let mut text = String::new();
        let mut tool_uses: u32 = 0;
        while let Some(ev) = child_rx.recv().await {
            match ev {
                AgentEvent::TextDelta { delta, .. } => text.push_str(&delta),
                AgentEvent::ToolStart { .. } => tool_uses += 1,
                _ => {}
            }
        }
        (text, tool_uses)
    };

    let (summary_res, (text, tool_uses)) = tokio::join!(run, collector);
    let summary = summary_res?;
    let progress = SubAgentProgress {
        tokens: summary.input_tokens.saturating_add(summary.output_tokens),
        tool_uses,
        duration_ms: 0,
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

async fn handle_run_subagent(handle: ForkHandle, inv: ToolInvocation) -> ToolHandlerOutput {
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
        &description,
        &input.prompt,
        None,
        input.allowed_tools.as_deref(),
        input.run_in_background,
    )
    .await
}

// ───────────────────────── run_skill handler ─────────────────────────

async fn handle_run_skill(handle: ForkHandle, inv: ToolInvocation) -> ToolHandlerOutput {
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
        &handle.parent_run_id,
        &conv_preview,
        &format!("skill {}", input.name),
        None,
    );
    let child = handle.child_handle(&agent_id);
    match run_forked_agent(&child, &agent_id, prompt, Some(&sys), None).await {
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

/// 前台阻塞跑 / 后台 spawn——`run_subagent` 两种模式共用。
async fn spawn_or_run(
    handle: &ForkHandle,
    description: &str,
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    background: bool,
) -> ToolHandlerOutput {
    // 限深预检（即将 fork 一层）：depth 已是「将要起的子」的父深度，run_forked_agent 内会再判一次。
    if handle.depth >= MAX_FORK_DEPTH {
        return err_out(
            ErrorCode::InvalidInput,
            format!(
                "fork depth limit reached (depth={}, max={})",
                handle.depth, MAX_FORK_DEPTH
            ),
        );
    }

    let agent_id = new_agent_id();

    if background {
        // 后台：spawn 子 run，立即返回 agentId；完成时 emit <task-notification>。
        // 先 register（避免子 run 瞬时完成时回调早于 register 的竞态），spawn 后再补挂 abort 句柄。
        handle.tasks.register(
            &agent_id,
            &handle.parent_run_id,
            "", // 后台任务的 conversation_id 在子 run 内部生成；注册表只记 parent 关联。
            description,
            None,
        );
        let child = handle.child_handle(&agent_id);
        let prompt_owned = prompt.to_string();
        let sys_owned = system_prompt.map(|s| s.to_string());
        let allowed_owned: Option<Vec<String>> = allowed_tools.map(|a| a.to_vec());
        let tasks = handle.tasks.clone();
        let parent_tx = handle.parent_event_tx.clone();
        let parent_run_id = handle.parent_run_id.clone();
        let aid = agent_id.clone();

        let join = tokio::spawn(async move {
            let res = run_forked_agent(
                &child,
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
            &handle.parent_run_id,
            "",
            description,
            None,
        );
        let child = handle.child_handle(&agent_id);
        match run_forked_agent(&child, &agent_id, prompt, system_prompt, allowed_tools).await {
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

fn handler_for<F, Fut>(
    handle: ForkHandle,
    f: F,
) -> Arc<FnToolHandler<impl Fn(ToolInvocation) -> ToolHandlerFuture + Send + Sync + 'static>>
where
    F: Fn(ForkHandle, ToolInvocation) -> Fut + Send + Sync + Clone + 'static,
    Fut: std::future::Future<Output = ToolHandlerOutput> + Send + 'static,
{
    Arc::new(FnToolHandler(move |inv: ToolInvocation| {
        let h = handle.clone();
        let f = f.clone();
        Box::pin(async move { f(h, inv).await }) as ToolHandlerFuture
    }))
}

/// 把 `run_subagent` / `run_skill` 注册进 registry（spec §3.6，bootstrap 默认注册）。
///
/// `handle` 注入 fork 执行底座所需的全部依赖（registry / provider 工厂 / repo / channel / 任务注册表 /
/// SkillStore）。注意：传入的 `registry` 通常**就是** `handle.registry`——`run_subagent`/`run_skill`
/// 注册进同一个父 registry，子 run 默认继承它（含这两个 tool 自身，支持嵌套 fork，受限深保护）。
pub fn register_subagent_tools(
    registry: &ToolRegistry,
    handle: ForkHandle,
) -> Result<(), RegisterError> {
    registry.register_tool(
        tool_spec_run_subagent(),
        handler_for(handle.clone(), |h, inv| handle_run_subagent(h, inv)),
    )?;
    registry.register_tool(
        tool_spec_run_skill(),
        handler_for(handle, |h, inv| handle_run_skill(h, inv)),
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
    use crate::infrastructure::agent::tool_registry::ToolHandler;
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

    // ---- run_forked_agent: returns the child's final assistant text ----
    #[tokio::test]
    async fn run_forked_agent_returns_child_final_text() {
        let handle = base_handle(fixed_factory("子 agent 的结论：买入 600519"), None);
        // register so run_forked_agent's terminal `finish` has a task to update.
        handle.tasks.register("sub_1", "parent-run", "", "analyze", None);
        let out = run_forked_agent(&handle, "sub_1", "去分析", None, None)
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
        let child = handle.child_handle("sub_iso");
        run_forked_agent(&child, "sub_iso", "hi", None, None)
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

    // ---- depth limit: refuse to fork beyond MAX_FORK_DEPTH ----
    #[tokio::test]
    async fn refuses_to_fork_past_depth_limit() {
        let handle = base_handle(fixed_factory("x"), None).with_depth(MAX_FORK_DEPTH);
        let err = run_forked_agent(&handle, "sub_deep", "go", None, None)
            .await
            .expect_err("must refuse at depth limit");
        match err {
            LoopError::Provider(msg) => assert!(msg.contains("depth limit")),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    // ---- depth increments per fork ----
    #[tokio::test]
    async fn child_handle_increments_depth() {
        let handle = base_handle(fixed_factory("x"), None);
        assert_eq!(handle.depth(), 0);
        let child = handle.child_handle("sub_x");
        assert_eq!(child.depth(), 1);
        assert_eq!(child.child_handle("sub_y").depth(), 2);
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
}
