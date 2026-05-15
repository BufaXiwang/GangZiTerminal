//! 本地工具注册表。
//!
//! Agent loop 收到 [`Block::ToolUse`] 后查 registry 拿到 [`Tool`] 并执行；
//! `server_side=true` 的 ToolUse 由 provider 执行，loop 永远不查 registry。
//!
//! 一个 ToolRegistry 是单 run 范围的——AppHandle 已经被各 Tool 持有，
//! 跨 run 共享 registry 实例没有性能价值，反而要担心生命周期问题。
//! Pipeline 在每次 run 启动时调用 [`build_chat_registry`] 或 [`build_readonly_registry`]。

use crate::agent::types::{ToolDef, ToolResultContent};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tauri::AppHandle;

// account 工具屏蔽中——账户模块重构期间不接通；完工后重新启用
// pub mod account;
pub mod memory;
pub mod news;
pub mod positions;
pub mod quotes;
// funds / research / scanner 工具已删除——依赖旧 crate::quotes 子模块，
// 等迁移到 infrastructure 后重写

/// 单个工具的执行契约。
///
/// 实现者要保证 [`execute`](Tool::execute) 在内部捕获所有可恢复错误并以
/// [`Vec<ToolResultContent>`] + `is_error=true` 形式返回——这样 agent 能在下一轮
/// 看到错误并决定是否重试。
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;

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
/// 这里只放 run_id，让 observer 把 tool 内部的细粒度事件关联回 run。
#[derive(Clone, Debug)]
pub struct ToolContext {
    pub run_id: String,
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

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// 把所有本地工具的 schema 导出成 [`ToolDef::Local`] 列表，
    /// 供 [`AgentRequest::tools`] 使用。Pipeline 在这之外可以再 push 一些
    /// [`ToolDef::ServerSide`]（如 web_search）。
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

/// 通用只读工具——chat/briefing/review 都注册。
fn register_readonly_tools(reg: &mut ToolRegistry, app: &AppHandle) {
    reg.register(Arc::new(quotes::GetQuoteTool::new(app.clone())));
    reg.register(Arc::new(quotes::GetKlineTool::new(app.clone())));
    reg.register(Arc::new(quotes::GetMarketOverviewTool::new(app.clone())));
    reg.register(Arc::new(positions::GetPositionTool::new(app.clone())));
    reg.register(Arc::new(news::SearchNewsTool::new(app.clone())));
    // account::GetAccountTool 暂未注册——等 account 重构稳定后改成读 ACCOUNT_SNAPSHOT
    // scanner / research / funds / get_indicators 工具暂未注册——
    // 数据源迁到 infrastructure 后重接（旧实现依赖已删除的 crate::quotes::tushare）
}

/// 模拟账户写工具——A5 重构期间**全部屏蔽**。
/// 完工后改成调 `pipeline::account::AccountService`（已实现）。
#[allow(dead_code, unused_variables)]
fn register_account_write_tools(
    reg: &mut ToolRegistry,
    app: &AppHandle,
    source_kind: &'static str,
) {
    // 屏蔽：account 模块重构中，agent 暂不能开仓 / 平仓 / 加减仓 / 调止损
    // reg.register(Arc::new(account::OpenPositionTool::new(app.clone(), source_kind)));
    // reg.register(Arc::new(account::ClosePositionTool::new(app.clone(), source_kind)));
    // reg.register(Arc::new(account::ScalePositionTool::new(app.clone(), source_kind)));
    // reg.register(Arc::new(account::AdjustStopsTool::new(app.clone(), source_kind)));
}

/// Chat pipeline 用：只读工具 + memory 写工具 + 账户写工具。
/// P1 暂时只在 chat 注册账户工具，briefing/review 仍走老 JSON tradeCalls 协议；
/// P2 再把账户工具铺到那两条 pipeline。
pub fn build_chat_registry(app: &AppHandle) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    register_readonly_tools(&mut reg, app);
    reg.register(Arc::new(memory::UpdateMemoryTool::new(app.clone())));
    reg.register(Arc::new(memory::RemoveMemoryTool::new(app.clone())));
    register_account_write_tools(&mut reg, app, "chat");
    reg
}

/// Briefing/Review pipeline 用：只读工具，**不**注册 memory 工具——这两条流水线
/// 通过 prompt 要求 agent 在最终 JSON 里输出 memoryUpdates/memoryRemovals 字段，
/// 由 pipeline 在 parse 后统一 merge。如果同时给它 update_memory 工具，agent 中途
/// 调工具写入的条目会被 pipeline 用"run 之前的 memory + JSON updates"覆写丢失。
pub fn build_readonly_registry(app: &AppHandle) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    register_readonly_tools(&mut reg, app);
    reg
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
