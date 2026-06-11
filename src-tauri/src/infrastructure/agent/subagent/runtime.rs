//! Fork 运行时上下文（每-run 注入）+ Fork 句柄（静态依赖注入）。
//!
//! Spec: docs/design/agent-infra-module.md §3.5（fork 上下文 run 时注入 + 工具集继承父 + 不嵌套）
//!
//! - `ForkRuntime`：发起 run 的入口（executor）构造，经 `DispatchExt` 透传到 dispatch；
//!   携带当前 run 的真实 channel / per-run registry / 共享状态 / 取消令牌 / event_tx。
//! - `ForkHandle`：bootstrap 装配的静态依赖（registry / provider 工厂 / repo / SkillStore /
//!   任务注册表）+ 无注入时的占位默认运行时。`child_registry` 构造子 registry（剔除 spawn 类 +
//!   allowedTools 校验/收紧）。

use std::sync::Arc;
use tokio::sync::mpsc;

use crate::domain::agent::{AgentEvent, CompactionConfig, ProviderChannel, RetryConfig};
use crate::infrastructure::agent::loop_executor::{LoopError, ProviderStream, RunSharedState};
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use crate::infrastructure::agent::payload_store::PayloadStore;
use crate::infrastructure::agent::skill_store::SkillStore;
use crate::infrastructure::agent::tool_registry::{DispatchExt, ToolRegistry};
use crate::infrastructure::agent::http_provider::HttpProvider;

use super::task_registry::SubAgentTaskRegistry;

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
    /// 父 run 的 event_tx（子 run 活动 `SubAgentActivity` 转发前端；**纯展示通道**）。
    pub parent_event_tx: Option<mpsc::Sender<AgentEvent>>,
    /// **父 run 的 per-run registry**（spec §3.5「工具集默认继承父」——继承的是父 run 真实工具集，
    /// 含 Runtime 注入的领域工具，不是 bootstrap 全局 registry）。None → 退回 `ForkHandle.registry`。
    pub registry: Option<Arc<ToolRegistry>>,
    /// 父 run 的共享状态（子 usage 回灌预算 + 后台 `<task-notification>` 注入父下一轮，spec §3.5）。
    pub shared: Option<RunSharedState>,
    /// 父 run 的取消令牌：取消父 run 时传播给跑着的子 run（子 loop 在 turn 边界 / stream 中响应）。
    pub cancel: tokio_util::sync::CancellationToken,
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
            registry: None,
            shared: None,
            cancel: tokio_util::sync::CancellationToken::new(),
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
    /// 注入父 run 的 per-run registry（fork 子 agent 默认继承的真实工具集，spec §3.5）。
    pub fn with_registry(mut self, registry: Arc<ToolRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }
    /// 注入父 run 的共享状态（token 回灌 + 通知队列，spec §3.5）。
    pub fn with_shared(mut self, shared: RunSharedState) -> Self {
        self.shared = Some(shared);
        self
    }
    /// 注入父 run 的取消令牌（取消传播给子 run）。
    pub fn with_cancel(mut self, cancel: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn is_subagent(&self) -> bool {
        self.is_subagent
    }

    /// 给子 run 用的运行时上下文：is_subagent=true、parent_run_id=子 run id、event_tx 不回灌父（隔离）。
    /// 置 is_subagent=true（对齐 CC `isInForkChild`）使子 run 一旦因某种原因仍触发 fork 时被布尔守卫兜底拒绝。
    /// `cancel` **保留**（父取消传播给子）；registry / shared 清空（子不能再 fork，不需要）。
    pub fn child(&self, child_run_id: &str) -> ForkRuntime {
        let mut c = self.clone();
        c.is_subagent = true;
        c.parent_run_id = child_run_id.to_string();
        c.parent_event_tx = None;
        c.registry = None;
        c.shared = None;
        c
    }

    /// 包装成 dispatch 用的不透明 `DispatchExt`（registry 透传，handler downcast）。
    pub fn into_ext(self) -> DispatchExt {
        Arc::new(self)
    }

    /// 从 dispatch 透传来的 `ext` 还原 `ForkRuntime`（downcast；非 fork 上下文 → None）。
    pub(crate) fn from_ext(ext: &Option<DispatchExt>) -> Option<ForkRuntime> {
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
    pub(crate) registry: Arc<ToolRegistry>,
    /// 子 run 的 provider 工厂（生产 = HttpProvider；测试 = scripted）。
    pub(crate) provider_factory: ProviderFactory,
    /// 审计落库 repo（子 run 用自己的 conversation_id + parent 关联）。
    pub(crate) repo: Option<AgentMessagesRepo>,
    pub(crate) payload_store: Option<PayloadStore>,
    /// 子 agent 任务注册表（spec §3.5）。
    pub(crate) tasks: SubAgentTaskRegistry,
    /// Skill 存盘访问（`run_skill` 读 SKILL.md 全文作 prompt）。
    pub(crate) skill_store: SkillStore,
    /// 默认运行时上下文（无 `ForkRuntime` 注入时的退路：bootstrap 占位 / 测试手工配置）。
    pub(crate) default_rt: ForkRuntime,
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
    pub(crate) fn resolve_rt(&self, ext: &Option<DispatchExt>) -> ForkRuntime {
        ForkRuntime::from_ext(ext).unwrap_or_else(|| self.default_rt.clone())
    }

    /// 解析子 run 默认继承的「源」registry：优先 `ForkRuntime.registry`（**父 run 的 per-run
    /// registry**，含 Runtime 注入的领域工具——spec §3.5「继承父」的真意）；没注入 → 退回
    /// `ForkHandle.registry`（bootstrap 全局，测试 / 无 per-run 场景）。
    pub(crate) fn source_registry<'a>(&'a self, rt: &'a ForkRuntime) -> &'a Arc<ToolRegistry> {
        rt.registry.as_ref().unwrap_or(&self.registry)
    }

    /// 构造子 run 的 registry（从 `source` 继承）。
    ///
    /// **不允许嵌套（首选机制）**：无论 `allowed_tools` 是否给定，子 registry **都不含**
    /// `run_subagent` / `run_skill`——这俩是 spawn 类工具，剔除后子 agent 的 system prompt 里根本没有
    /// 它们 → 不能再 fork（对齐 Claude Code 的「不给/拒绝 fork 工具」）。其它工具（含 `create_skill`
    /// = 写文件、本地 tool、领域 tool）保留。`create_skill` 不算 spawn（不递归），保留。
    ///
    /// - `allowed_tools=None`：继承源全部工具，**但仍剔除 spawn 类**。
    /// - `allowed_tools=Some(..)`：收紧到子集，**且剔除其中的 spawn 类**（即便被显式列入也不给）。
    ///   含未注册 tool name → `Err(invalid_input 消息)`（spec §3.6，不静默忽略）。
    ///
    /// 子 registry 仍走父 repo / payload_store（dispatch 持久化），故用父的 repo/payload_store 构造。
    pub(crate) fn child_registry(
        &self,
        source: &Arc<ToolRegistry>,
        allowed: Option<&[String]>,
    ) -> Result<Arc<ToolRegistry>, String> {
        // spec §3.6：allowedTools 出现未注册（且非 spawn 类——spawn 类按"剔除"语义处理）名字 → 拒绝。
        if let Some(a) = allowed {
            let unknown: Vec<&str> = a
                .iter()
                .map(|s| s.as_str())
                .filter(|n| !source.has_tool(n))
                .collect();
            if !unknown.is_empty() {
                return Err(format!(
                    "allowedTools contains unregistered tool(s): {}",
                    unknown.join(", ")
                ));
            }
        }
        let child = match (&self.repo, &self.payload_store) {
            (Some(r), Some(p)) => ToolRegistry::new(r.clone(), p.clone()),
            _ => ToolRegistry::new_without_persist(),
        };
        // allowed=None → 继承源全部；Some → 收紧到子集。两种情况都剔除 spawn 类工具。
        let allow: Option<std::collections::HashSet<&str>> =
            allowed.map(|a| a.iter().map(|s| s.as_str()).collect());
        for spec in source.list_tools() {
            // 从根上剔除 spawn 类工具（按 `ToolSpec.is_spawn` 标记，不靠 name 白名单 →
            // 覆盖 run_subagent / run_skill 等 fork 类工具），禁止嵌套 fork。
            if spec.is_spawn {
                continue;
            }
            if allow
                .as_ref()
                .map(|a| a.contains(spec.name.as_str()))
                .unwrap_or(true)
            {
                if let Some(handler) = source.clone_handler(&spec.name) {
                    // 重注册同名 tool 到子 registry（不会 Duplicate：子是新空表）。
                    let _ = child.register_tool(spec, handler);
                }
            }
        }
        Ok(Arc::new(child))
    }
}

