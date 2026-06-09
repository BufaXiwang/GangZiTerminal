//! Agent Infra infrastructure 层 — provider channel / ToolRegistry / 持久化 / loop。
//!
//! Spec: docs/design/agent-infra-module.md §3 / §4 / §5
//!
//! 模块拆分：
//! - `migrations`         — agent_* 表 schema（messages / tool_calls / provider_channels / payloads）
//! - `messages_repo`      — `AgentMessage` / `ToolCall` 持久化
//! - `payload_store`      — PayloadStore（spec §2 双层存储）
//! - `channels_repo`      — `ProviderChannel` CRUD
//! - `tool_registry`     — Tool 注册 + dispatch + 超时控制 + PayloadStore 集成
//! - `tool_parser`       — `<use_tool>` 增量扫描状态机
//! - `system_prompt`      — `SystemPromptBuilder`：把 ToolSpec 编译成 system prompt tool 清单
//! - `context_compaction` — 易腐 tool 结果清理 + 上下文裁剪 + `compact_context(policy)`
//! - `providers`          — 三类 wire format adapter（纯 chat）
//! - `loop_executor`      — canonical Agent loop（含 ToolCallParser + reactive retry）

pub mod channels_repo;
pub mod context_compaction;
pub mod discovery;
pub mod http_provider;
pub mod local_tools;
pub mod loop_executor;
pub mod messages_repo;
pub mod migrations;
pub mod payload_store;
pub mod runtime_repo;
pub mod presets;
pub mod providers;
pub mod skill_store;
pub mod skill_tools;
pub mod subagent;
pub mod tool_parser;
pub mod tool_registry;
pub mod system_prompt;

/// LLM-as-Judge live test suite (test-only, all `#[ignore]`).
/// Spec: docs/design/agent-infra-module.md §2/§3/§4/§5 — semantic validation via a judge LLM.
#[cfg(test)]
mod llm_judge_tests;

/// LLM-as-Judge + adversarial test suite for the Tool subsystem (text protocol across 3 wires,
/// local file/bash tools + workspace sandbox, skill create/load + progressive disclosure).
/// Live tests `#[ignore]`; hermetic protocol/rename-regression tests run normally.
/// Spec: agent-infra-module.md §2/§3/§5 + agent-runtime-module.md §4.2/§Skills.
#[cfg(test)]
mod judge_tools_tests;

/// Stress suite for the text-protocol `<use_tool>` tool-use reliability under pressure
/// (multi-turn tool chains, skill→multi-tool orchestration, 12+ tool selection, mid-session error
/// recovery). Live tests `#[ignore]`; hermetic protocol-health-accounting checks run normally.
/// Spec: agent-infra-module.md §2/§3/§5 + agent-runtime-module.md §4.2/§Skills.
#[cfg(test)]
mod judge_stress_tests;

/// 端到端多轮对话调用链 live 测试：真实 provider 跑 N 轮，抓全 AgentEvent 链，断言每轮非空文本
/// （Anthropic「无文本输出」后端回归守卫）+ 跨轮 tool 链 + 持久化续接。全部 `#[ignore]`。
/// Spec: agent-infra-module.md §3 + agent-runtime-module.md §6。
#[cfg(test)]
mod e2e_dialogue_tests;

/// 联网搜索 web_search：可插拔多源（DuckDuckGo/Jina/Bocha/Tavily）+ 并行去重聚合。
/// Spec: agent-infra-module.md §3.6 web_search。
pub mod web_search;

/// 网页正文抽取 web_extract：读 URL → scraper 抽 main content（研究链：搜→读→综合）。
/// Spec: agent-infra-module.md §3.6 web_extract。
pub mod web_extract;

pub use channels_repo::{ChannelsRepoError, ProviderChannelsRepo};
pub use discovery::{discover_models, DiscoverError, DiscoveredModel};
pub use http_provider::HttpProvider;
pub use presets::{channel_presets, ChannelPreset};
pub use context_compaction::{
    compact_context, decide_tier, drop_oldest_round_messages, estimate_context_tokens,
    estimate_message_tokens, estimate_messages_tokens, message_is_durable, micro_clear_messages,
    CompactPolicy,
};
pub use local_tools::register_local_tools;
pub use skill_store::{parse_frontmatter, render_skill_md, SkillIndexEntry, SkillStore};
pub use skill_tools::register_skill_tools;
pub use subagent::{
    http_provider_factory, register_subagent_tools, run_forked_agent, stop_subagent,
    subagent_output, ForkHandle, ForkRuntime, ProviderFactory, SubAgentProgress, SubAgentStatus,
    SubAgentTaskRegistry,
};
pub use loop_executor::{
    run_agent_turn, run_agent_turn_cancellable, run_agent_turn_forked, LoopError, ProviderStream,
};
pub use messages_repo::AgentMessagesRepo;
pub use migrations::migrations;
pub use payload_store::{PayloadKind, PayloadStore, PayloadStoreEntry, PAYLOAD_INLINE_LIMIT_BYTES};
pub use runtime_repo::AgentRuntimeRepo;
pub use tool_parser::{ParserEvent, ToolCallParser};
pub use tool_registry::{
    DispatchError, DispatchExt, FnToolHandler, InputValidator, ToolHandler, ToolHandlerFuture,
    ToolHandlerOutput, ToolInvocation, ToolRegistry,
};
pub use system_prompt::{build_system_prompt, build_system_prompt_with_skills, PROTOCOL_PREAMBLE};

use std::sync::Arc;

/// Agent Infra bootstrap — 创建 Tauri State 用的 `AgentInfra`（repo + registry + payload store + channels repo）。
///
/// Runtime 在 setup 时调用，把结果 manage 进 Tauri State；
/// 之后用 `register_tool` 注入 Quotes / News / Account facade tool。
///
/// `workspace_dir` 是 Agent 本地通用 tool（read/write/edit/run_bash）的约定级沙箱根
/// （Spec: agent-runtime-module.md §4.2）。bootstrap 时默认注册这 4 个本地 tool。
/// Phase 3 / adapter 应注入 `<appData>/gangzi/workspace` 作为绝对路径；此处给占位默认。
///
/// `skills_dir` 是 Skill（playbook）存盘根（Spec: agent-runtime-module.md §Skills），独立于 workspace：
/// `<appData>/gangzi/skills/<name>/SKILL.md`。bootstrap 时默认注册 create_skill（编排 tool）+
/// run_skill / run_subagent（fork 子 agent，spec §3.5 / §3.6）。
/// skills 初始为空（没有 SKILL.md → 索引为空）。
pub fn bootstrap(
    db: crate::infrastructure::db::AppDb,
    workspace_dir: std::path::PathBuf,
    skills_dir: std::path::PathBuf,
) -> AgentInfra {
    let repo = AgentMessagesRepo::new(db.clone());
    let payload_store = PayloadStore::new(db.clone());
    let channels_repo = ProviderChannelsRepo::new(db);
    let registry = Arc::new(ToolRegistry::new(repo.clone(), payload_store.clone()));
    // 默认注册 4 个本地通用 tool（约定级沙箱）。重复注册会 fail closed，但 bootstrap 只调用一次。
    if let Err(e) = register_local_tools(&registry, workspace_dir) {
        tracing::warn!("register_local_tools failed: {e}");
    }
    // 默认注册 skill 编排 tool（create_skill）。
    if let Err(e) = register_skill_tools(&registry, skills_dir.clone()) {
        tracing::warn!("register_skill_tools failed: {e}");
    }
    let skill_store = SkillStore::new(skills_dir);
    // 默认注册 fork 子 agent tool（run_subagent / run_skill）（spec §3.5 / §3.6）。
    //
    // ForkHandle 只持有**静态依赖**（registry / provider 工厂 / repo / 任务注册表 / SkillStore）+ 一组
    // 占位默认运行时配置。真实的运行时上下文（channel / is_subagent / parent_run_id / event_tx）由发起 run 的
    // 入口（Tauri command / scheduler / Runtime）构造 `ForkRuntime` 在跑 run 时注入：调用
    // `run_agent_turn_forked(..., Some(ForkRuntime::new(channel, run_id).into_ext()))`。fork handler 在
    // dispatch 时把它 downcast 回来，于是子 run 用的是**当前 run 的真实 channel / is_subagent**——无需 registry
    // replace、也不会再有「占位 channel 被真用 / 子 agent 的 is_subagent 标记丢失」的 P0 漏洞。bootstrap 这里只把
    // tool 名注册进 registry（system prompt 需要它们出现在 tool 清单里），provider 工厂用
    // http_provider_factory（生产）；fork_channel 仅作无注入时的退路占位。
    let tasks = SubAgentTaskRegistry::new();
    let fork_channel = crate::domain::agent::ProviderChannel {
        channel_id: String::new(),
        provider: String::new(),
        wire_format: crate::domain::agent::WireFormat::Messages,
        base_url: None,
        api_key: String::new(),
        model: String::new(),
        stream: true,
        enabled: true,
        supports_vision: false,
        supports_thinking: false,
        max_output_tokens: None,
        context_window_tokens: None,
        thinking_budget_tokens: None,
    };
    let fork_handle = ForkHandle::new(
        registry.clone(),
        http_provider_factory(),
        Some(repo.clone()),
        Some(payload_store.clone()),
        fork_channel,
        skill_store.clone(),
        tasks.clone(),
    );
    if let Err(e) = register_subagent_tools(&registry, fork_handle) {
        tracing::warn!("register_subagent_tools failed: {e}");
    }
    AgentInfra {
        repo,
        registry,
        payload_store,
        channels_repo,
        skill_store,
        subagent_tasks: tasks,
    }
}

/// 本地 tool 工作区的占位默认根。
///
/// Phase 3 / adapter 接线时应替换为 `<appData>/gangzi/workspace`（由 adapter 注入绝对路径）。
pub fn default_workspace_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("gangzi-workspace")
}

/// Skill 存盘的占位默认根。
///
/// Phase 3 / adapter 接线时应替换为 `<appData>/gangzi/skills`（由 adapter 注入绝对路径）。
pub fn default_skills_dir() -> std::path::PathBuf {
    std::env::temp_dir().join("gangzi-skills")
}

/// Agent Infra 容器，由 setup 持有。
#[derive(Clone)]
pub struct AgentInfra {
    pub repo: AgentMessagesRepo,
    pub registry: Arc<ToolRegistry>,
    pub payload_store: PayloadStore,
    pub channels_repo: ProviderChannelsRepo,
    /// Skill（playbook）存盘访问。Runtime 构建 system prompt 时用 `skill_store.list_index()`
    /// 注入 skill 索引（渐进披露，Spec: agent-runtime-module.md §Skills）。
    pub skill_store: SkillStore,
    /// 子 agent 任务注册表（spec §3.5）。Runtime / adapter 据此实现 stop_subagent / subagent_output
    /// 的对外命令。
    pub subagent_tasks: SubAgentTaskRegistry,
}
