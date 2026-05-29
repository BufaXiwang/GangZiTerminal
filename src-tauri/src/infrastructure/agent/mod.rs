//! Agent Infra infrastructure 层 — provider channel / SkillRegistry / 持久化 / loop。
//!
//! Spec: docs/design/agent-infra-module.md §3 / §4 / §5
//!
//! 模块拆分：
//! - `migrations`         — agent_* 表 schema（messages / skill_calls / provider_channels / payloads）
//! - `messages_repo`      — `AgentMessage` / `SkillCall` 持久化
//! - `payload_store`      — PayloadStore（spec §2 双层存储）
//! - `channels_repo`      — `ProviderChannel` CRUD
//! - `skill_registry`     — Skill 注册 + dispatch + 超时控制 + PayloadStore 集成
//! - `skill_parser`       — `<use_skill>` 增量扫描状态机
//! - `system_prompt`      — `SystemPromptBuilder`：把 SkillSpec 编译成 system prompt skill 清单
//! - `context_compaction` — 易腐 skill 结果清理 + 上下文裁剪 + `compact_context(policy)`
//! - `providers`          — 三类 wire format adapter（纯 chat）
//! - `loop_executor`      — canonical Agent loop（含 SkillCallParser + reactive retry）

pub mod channels_repo;
pub mod context_compaction;
pub mod discovery;
pub mod http_provider;
pub mod loop_executor;
pub mod messages_repo;
pub mod migrations;
pub mod payload_store;
pub mod presets;
pub mod providers;
pub mod skill_parser;
pub mod skill_registry;
pub mod system_prompt;

pub use channels_repo::{ChannelsRepoError, ProviderChannelsRepo};
pub use discovery::{discover_models, DiscoverError, DiscoveredModel};
pub use http_provider::HttpProvider;
pub use presets::{channel_presets, ChannelPreset};
pub use context_compaction::{
    compact_context, decide_tier, estimate_context_tokens, CompactPolicy,
};
pub use messages_repo::AgentMessagesRepo;
pub use migrations::migrations;
pub use payload_store::{PayloadKind, PayloadStore, PayloadStoreEntry, PAYLOAD_INLINE_LIMIT_BYTES};
pub use skill_parser::{ParserEvent, SkillCallParser};
pub use skill_registry::{
    DispatchError, FnSkillHandler, InputValidator, SkillHandler, SkillHandlerFuture,
    SkillHandlerOutput, SkillInvocation, SkillRegistry,
};
pub use system_prompt::{build_system_prompt, PROTOCOL_PREAMBLE};

use std::sync::Arc;

/// Agent Infra bootstrap — 创建 Tauri State 用的 `AgentInfra`（repo + registry + payload store + channels repo）。
///
/// Runtime 在 setup 时调用，把结果 manage 进 Tauri State；
/// 之后用 `register_skill` 注入 Quotes / News / Account facade skill。
pub fn bootstrap(db: crate::infrastructure::db::AppDb) -> AgentInfra {
    let repo = AgentMessagesRepo::new(db.clone());
    let payload_store = PayloadStore::new(db.clone());
    let channels_repo = ProviderChannelsRepo::new(db);
    let registry = Arc::new(SkillRegistry::new(repo.clone(), payload_store.clone()));
    AgentInfra {
        repo,
        registry,
        payload_store,
        channels_repo,
    }
}

/// Agent Infra 容器，由 setup 持有。
#[derive(Clone)]
pub struct AgentInfra {
    pub repo: AgentMessagesRepo,
    pub registry: Arc<SkillRegistry>,
    pub payload_store: PayloadStore,
    pub channels_repo: ProviderChannelsRepo,
}
