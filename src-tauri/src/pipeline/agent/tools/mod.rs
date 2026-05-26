//! Agent local-tool 抽象层（trait + registry + per-run context）。
//!
//! 这层只放契约——任何 use case（chat / 将来可能的 briefing/review）都依赖这里的
//! `Tool` / `ToolRegistry` / `ToolContext`，**不知道**具体 tool 是怎么实现的。
//! 具体 tool 在 [`crate::adapters::agent_tools`]（LLM 工具协议层）里实现，
//! 通过 [`ToolRegistry::register`] 注入。
//!
//! Agent loop 收到 [`Block::ToolUse`](crate::domain::agent::types::Block) 后查 registry
//! 拿到 [`Tool`] 并执行；`server_side=true` 的 ToolUse 由 provider 执行，loop 永远不查
//! registry。
//!
//! 一个 ToolRegistry 是单 run 范围的——AppHandle 已经被各 Tool 持有，跨 run 共享
//! registry 实例没有性能价值，反而要担心生命周期问题。Pipeline 在每次 run 启动时
//! 由调用方（adapter）传入一个新构造的 registry。

use crate::domain::agent::types::{ToolDef, ToolResultContent};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tauri::AppHandle;

/// adapter 注入的 registry 构造函数。
/// 用 `OnceLock` 而不是 `Mutex` —— adapter 在 setup 一次就够，后续只读。
///
/// Profile-aware：spec §2 / §4 要求按 `AgentRunProfile.allowedTools` 注册，
/// 而 `scheduled_review` 的 `operate_account` 可由 KV `runtime.scheduled_review.
/// allow_trading_write` 显式开启。Factory 签名：
///   `(app, profile_id_str, allow_trading_write_override) -> ToolRegistry`
///
/// 用字符串传 profile 而不是 domain enum，避免 pipeline::agent::tools 反向
/// 依赖 `domain::agent_runtime`（保留 Infra 与 Runtime 解耦）。
type RegistryFactory =
    fn(&AppHandle, &str, Option<bool>) -> ToolRegistry;
static FACTORY: OnceLock<RegistryFactory> = OnceLock::new();

/// adapter 在 setup hook 调用一次注入工具注册工厂。
pub fn install_registry_factory(factory: RegistryFactory) {
    let _ = FACTORY.set(factory);
}

/// pipeline 用 —— 按 profile 构造 registry；profile 未知或工厂未装时返回 None。
pub fn build_registry_for(
    app: &AppHandle,
    profile_id: &str,
    allow_trading_write_override: Option<bool>,
) -> Option<ToolRegistry> {
    FACTORY
        .get()
        .map(|f| f(app, profile_id, allow_trading_write_override))
}

/// 单个工具的执行契约。
///
/// 实现者要保证 [`execute`](Tool::execute) 在内部捕获所有可恢复错误并以
/// [`Vec<ToolResultContent>`] + `is_error=true` 形式返回——这样 agent 能在下一轮
/// 看到错误并决定是否重试。
/// spec `agent-infra-module.md §2 ToolSpec.sideEffect` 闭集合。
/// trading_write 工具必须由 Runtime 显式允许（spec §2「sideEffect = `trading_write`
/// 的工具必须由 Runtime 显式允许，Infra 默认不得暴露」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideEffect {
    None,
    NonTradingWrite,
    TradingWrite,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;
    /// spec `agent-infra-module.md §2 ToolSpec.timeoutMs` —— 默认 30s；写工具可覆盖。
    fn timeout_ms(&self) -> u64 {
        30_000
    }
    /// spec `agent-infra-module.md §2 ToolSpec.sideEffect` —— 默认 none；写工具必须覆盖。
    fn side_effect(&self) -> SideEffect {
        SideEffect::None
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &ToolContext,
    ) -> (Vec<ToolResultContent>, bool /* is_error */);
}

/// 工具执行时拿得到的 per-run 句柄。
///
/// 故意不持有 AppHandle——每个 Tool 实现自己持有需要的句柄（每个 tool 对
/// SQLite / app_state / 远端 API 的需求差异大，让它们自定义比共享 ctx 干净）。
/// `ToolContext` 携带 run-scope 元数据 + 可选 AppHandle（供 audit / persistence 用）。
///
/// `app` 给 `pipeline::agent::loop_` 的 audit 写 `agent_tool_calls` 用；具体
/// tool 实现仍按 spec `agent-infra-module.md §2 ToolSpec.handler` 自行持有
/// 需要的 AppHandle。
#[derive(Clone, Debug)]
pub struct ToolContext {
    pub run_id: String,
    pub app: Option<AppHandle>,
    /// 当前 tool 调用的 tool_use_id（spec agent-infra-module.md §2 ToolCall.tool_call_id）。
    /// dispatcher 在 execute_single_tool 注入；具体 tool 用它做 audit 关联（spec §2
    /// 「需要恢复副作用或审计精确结果的 local tool 必须持久化结构化 input / output
    /// payload，并通过 outputPayloadRef 关联」）。
    pub tool_call_id: Option<String>,
}

/// 一组 Tool 的集合 + 名字索引。
pub struct ToolRegistry {
    tools: HashMap<&'static str, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name();
        if self.tools.insert(name, tool).is_some() {
            // 重名是配置错误——直接 panic 让开发期立刻发现，比 silent override 安全
            panic!("ToolRegistry: 重复注册工具 {name}");
        }
    }

    /// spec `agent-infra-module.md §2`「sideEffect = trading_write 的工具必须由
    /// Runtime 显式允许，Infra 默认不得暴露」。adapter 工厂调用 `register` 前应该
    /// 调本方法做 enforce；返回 false 时跳过注册。
    pub fn allow_register(tool: &Arc<dyn Tool>, allow_trading_write: bool) -> bool {
        match tool.side_effect() {
            SideEffect::TradingWrite => allow_trading_write,
            _ => true,
        }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// 把所有本地工具的 schema 导出成 [`ToolDef::Local`] 列表，
    /// 供 [`AgentRequest::tools`](crate::domain::agent::types::AgentRequest::tools) 使用。
    /// Pipeline 在这之外可以再 push 一些 [`ToolDef::ServerSide`]（如 web_search）。
    ///
    /// `cache_control_last=true` 时把最后一个工具的 cache_control 置 true，
    /// 让整个 tools 区进 cache。
    pub fn to_tool_defs(&self, cache_control_last: bool) -> Vec<ToolDef> {
        let mut defs: Vec<ToolDef> = self
            .tools
            .values()
            .map(|t| ToolDef::Local {
                name: t.name().into(),
                description: t.description().into(),
                input_schema: t.input_schema(),
                cache_control: false,
            })
            .collect();
        // HashMap 顺序不稳定——按 name 排序保证 cache prefix 字节稳定（不同次运行
        // tools 顺序一致，cache 才能命中）。
        defs.sort_by(|a, b| match (a, b) {
            (ToolDef::Local { name: a, .. }, ToolDef::Local { name: b, .. }) => a.cmp(b),
            _ => std::cmp::Ordering::Equal,
        });
        if cache_control_last {
            if let Some(ToolDef::Local { cache_control, .. }) = defs.last_mut() {
                *cache_control = true;
            }
        }
        defs
    }

    /// 注册的工具数（测试和单测用）。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.tools.len()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ===== 共用 helper：把 JSON 包成 ToolResultContent =======================

/// 大部分工具最终都把一个 JSON Value 序列化成文本返回——给 LLM 看的是字符串。
/// 这个 helper 统一格式：`{"type": "text", "text": "<json>"}`。
pub fn ok_json(value: Value) -> Vec<ToolResultContent> {
    let text = value.to_string();
    vec![ToolResultContent::Text { text }]
}

/// 工具内部可恢复错误（参数不合法 / 远端拉不到数据）走这条——
/// is_error=true 让 agent 看到错误并决定下一轮怎么补救。
pub fn err_text(msg: impl Into<String>) -> (Vec<ToolResultContent>, bool) {
    (vec![ToolResultContent::Text { text: msg.into() }], true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct DummyTool {
        name: &'static str,
    }
    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "dummy"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(
            &self,
            _input: Value,
            _ctx: &ToolContext,
        ) -> (Vec<ToolResultContent>, bool) {
            (vec![ToolResultContent::Text { text: "ok".into() }], false)
        }
    }

    #[test]
    fn registry_lookup_works() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool { name: "alpha" }));
        reg.register(Arc::new(DummyTool { name: "bravo" }));
        assert_eq!(reg.len(), 2);
        assert!(reg.get("alpha").is_some());
        assert!(reg.get("zulu").is_none());
    }

    #[test]
    fn to_tool_defs_sorted_and_cache_marks_last() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool { name: "zulu" }));
        reg.register(Arc::new(DummyTool { name: "alpha" }));
        reg.register(Arc::new(DummyTool { name: "mike" }));
        let defs = reg.to_tool_defs(true);
        let names: Vec<&str> = defs
            .iter()
            .map(|d| match d {
                ToolDef::Local { name, .. } => name.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(names, vec!["alpha", "mike", "zulu"]);
        match defs.last().unwrap() {
            ToolDef::Local { cache_control, .. } => assert!(cache_control),
            _ => panic!(),
        }
        match defs.first().unwrap() {
            ToolDef::Local { cache_control, .. } => assert!(!cache_control),
            _ => panic!(),
        }
    }

    #[test]
    #[should_panic(expected = "重复注册")]
    fn registry_panics_on_duplicate() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(DummyTool { name: "x" }));
        reg.register(Arc::new(DummyTool { name: "x" }));
    }
}
