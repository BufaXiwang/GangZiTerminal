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
pub mod presets;
pub mod providers;
pub mod skill_store;
pub mod skill_tools;
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
pub use loop_executor::{run_agent_turn, LoopError, ProviderStream};
pub use messages_repo::AgentMessagesRepo;
pub use migrations::migrations;
pub use payload_store::{PayloadKind, PayloadStore, PayloadStoreEntry, PAYLOAD_INLINE_LIMIT_BYTES};
pub use tool_parser::{ParserEvent, ToolCallParser};
pub use tool_registry::{
    DispatchError, FnToolHandler, InputValidator, ToolHandler, ToolHandlerFuture,
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
/// `<appData>/gangzi/skills/<name>/SKILL.md`。bootstrap 时默认注册 create_skill / load_skill 两个编排 tool。
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
    // 默认注册 skill 编排 tool（create_skill / load_skill）。
    if let Err(e) = register_skill_tools(&registry, skills_dir.clone()) {
        tracing::warn!("register_skill_tools failed: {e}");
    }
    let skill_store = SkillStore::new(skills_dir);
    AgentInfra {
        repo,
        registry,
        payload_store,
        channels_repo,
        skill_store,
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
}
