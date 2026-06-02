//! ToolRegistry — tool 注册 / 校验 / 分发 / 超时控制。
//!
//! Spec: docs/design/agent-infra-module.md §3 / §5 Tool Registry API
//!
//! 关键约束：
//! - 同名 tool 只能注册一次；重复注册 fail closed（§2 不变量）。
//! - dispatch 必须按 `ToolSpec.timeout_ms` 超时（§3）。
//! - dispatch 必须记录 `ToolCall` 开始和结束（§5）。
//! - tool input 必须按 `inputSchema` 通过 caller 提供的校验函数验证；
//!   失败作为 `<tool_error code="invalid_input">` 回传给模型，不调用 handler。
//! - PayloadStore 双层存储（spec §2）：input / output > 8KB 走 ref，summary 留截断摘要。

use crate::domain::agent::{
    JsonSummary, ToolCall, ToolCallId, ToolCallResult, ToolSpec,
};
use crate::domain::shared::{ErrorCode, OccurredAt};
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use crate::infrastructure::agent::payload_store::{
    PayloadKind, PayloadStore, PAYLOAD_INLINE_LIMIT_BYTES,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use uuid::Uuid;

/// Tool handler 的输入。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ToolInvocation {
    pub run_id: String,
    pub tool_call_id: ToolCallId,
    pub name: String,
    pub input: JsonSummary,
}

/// Tool handler 的输出（Infra 视角）。
///
/// `output_summary` 必须可摘要展示（spec §2 ToolSpec rules）。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ToolHandlerOutput {
    pub output_summary: JsonSummary,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ErrorCode>,
}

impl ToolHandlerOutput {
    pub fn ok(summary: JsonSummary) -> Self {
        Self {
            output_summary: summary,
            is_error: false,
            error_code: None,
        }
    }
    pub fn err(summary: JsonSummary, code: ErrorCode) -> Self {
        Self {
            output_summary: summary,
            is_error: true,
            error_code: Some(code),
        }
    }
}

/// Async handler future。
pub type ToolHandlerFuture =
    Pin<Box<dyn Future<Output = ToolHandlerOutput> + Send + 'static>>;

/// Tool handler signature。Runtime / adapter 注册时提供。
///
/// 必须 `Send + Sync + 'static`，以便 `Arc<ToolRegistry>` 跨任务共享。
pub trait ToolHandler: Send + Sync + 'static {
    fn invoke(&self, inv: ToolInvocation) -> ToolHandlerFuture;
}

/// 函数 closure -> ToolHandler adapter。
pub struct FnToolHandler<F>(pub F);

impl<F> ToolHandler for FnToolHandler<F>
where
    F: Fn(ToolInvocation) -> ToolHandlerFuture + Send + Sync + 'static,
{
    fn invoke(&self, inv: ToolInvocation) -> ToolHandlerFuture {
        (self.0)(inv)
    }
}

/// Input 校验闭包；返回 None = 通过，返回 Some(message) = 拒绝。
///
/// Spec §2: tool input 必须按 `inputSchema` 校验。Infra 默认行为是"接受任何 JSON"，
/// 由调用方通过 `register_tool_with_validator` 注入实际 schema 校验器。
pub type InputValidator =
    Arc<dyn Fn(&JsonSummary) -> Option<String> + Send + Sync + 'static>;

struct ToolEntry {
    spec: ToolSpec,
    handler: Arc<dyn ToolHandler>,
    input_validator: Option<InputValidator>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    #[error("tool name '{0}' already registered")]
    Duplicate(String),
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("tool '{0}' not registered")]
    NotRegistered(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("repo: {0}")]
    Repo(#[from] crate::infrastructure::agent::messages_repo::RepoError),
    #[error("payload store: {0}")]
    Payload(#[from] crate::infrastructure::agent::payload_store::PayloadStoreError),
}

/// 注册表实例。运行时持有为 Arc。
///
/// Spec §3/§5: dispatch 时必须先验证 input，再调用 handler，再持久化 ToolCall。
pub struct ToolRegistry {
    tools: RwLock<HashMap<String, ToolEntry>>,
    repo: Option<AgentMessagesRepo>,
    payload_store: Option<PayloadStore>,
}

impl ToolRegistry {
    /// 创建不带持久化的注册表（仅用于测试 / 单元）。
    pub fn new_without_persist() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            repo: None,
            payload_store: None,
        }
    }

    /// 生产入口：注入持久化 repo 和 PayloadStore。
    pub fn new(repo: AgentMessagesRepo, payload_store: PayloadStore) -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            repo: Some(repo),
            payload_store: Some(payload_store),
        }
    }

    /// 注册 tool。
    ///
    /// Spec §5 Tool Registry API: `register_tool(spec, handler) -> Result<()>`。
    pub fn register_tool(
        &self,
        spec: ToolSpec,
        handler: Arc<dyn ToolHandler>,
    ) -> Result<(), RegisterError> {
        self.register_tool_with_validator(spec, handler, None)
    }

    /// 注册 tool 并附带 input schema 校验器。
    pub fn register_tool_with_validator(
        &self,
        spec: ToolSpec,
        handler: Arc<dyn ToolHandler>,
        input_validator: Option<InputValidator>,
    ) -> Result<(), RegisterError> {
        let mut g = self.tools.write().expect("ToolRegistry RwLock poisoned");
        if g.contains_key(&spec.name) {
            return Err(RegisterError::Duplicate(spec.name));
        }
        g.insert(
            spec.name.clone(),
            ToolEntry {
                spec,
                handler,
                input_validator,
            },
        );
        Ok(())
    }

    /// 是否注册了该 tool。
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools
            .read()
            .expect("RwLock poisoned")
            .contains_key(name)
    }

    /// 返回某 tool 的 `sideEffect`（spec §4 通用压缩信号；`trading_write` = 结果不可丢）。
    /// 未注册返回 None。
    pub fn tool_side_effect(&self, name: &str) -> Option<crate::domain::agent::SideEffect> {
        self.tools
            .read()
            .expect("RwLock poisoned")
            .get(name)
            .map(|e| e.spec.side_effect)
    }

    /// 取出某 tool 的 handler `Arc` 克隆（用于构造收紧的子 registry：fork 子 agent 的
    /// `allowed_tools` 子集复用父 handler）。未注册返回 None。
    ///
    /// Spec: agent-infra-module.md §3.5（fork 工具集默认继承父，可收紧到子集）。
    pub fn clone_handler(&self, name: &str) -> Option<Arc<dyn ToolHandler>> {
        self.tools
            .read()
            .expect("RwLock poisoned")
            .get(name)
            .map(|e| e.handler.clone())
    }

    /// 列出所有 ToolSpec — 喂 SystemPromptBuilder 用。
    pub fn list_tools(&self) -> Vec<ToolSpec> {
        self.tools
            .read()
            .expect("RwLock poisoned")
            .values()
            .map(|e| e.spec.clone())
            .collect()
    }

    /// Spec §5: `validate_tool_input(tool_name, input)`。
    pub fn validate_tool_input(
        &self,
        tool_name: &str,
        input: &JsonSummary,
    ) -> Result<(), DispatchError> {
        let g = self.tools.read().expect("RwLock poisoned");
        let entry = g
            .get(tool_name)
            .ok_or_else(|| DispatchError::NotRegistered(tool_name.into()))?;
        if let Some(v) = &entry.input_validator {
            if let Some(msg) = v(input) {
                return Err(DispatchError::InvalidInput(msg));
            }
        }
        Ok(())
    }

    /// 生成新的 tool_call_id（spec §2: `tc_<uuid>`）。
    pub fn new_tool_call_id() -> ToolCallId {
        format!("tc_{}", Uuid::new_v4())
    }

    /// Spec §5: `dispatch_tool_call`。
    ///
    /// 行为：
    /// 1. 查表；未注册 → `NotRegistered`，作为 `<tool_error code="invalid_input">` 返回（caller 决定）。
    /// 2. 校验 input；失败 → `InvalidInput`。
    /// 3. 持久化 ToolCall 起始记录（input 大则走 PayloadStore ref + 截断 summary）。
    /// 4. 按 spec `timeout_ms` 执行 handler；超时 → `tool_timeout`。
    /// 5. 持久化 ToolCall 结束 + 写入 summary / payload_ref。
    ///
    /// `tool_call_id` 由 caller 提供（loop_executor 在 parser 检测到 `<use_tool>` 闭合时生成）。
    pub async fn dispatch_tool_call(
        &self,
        run_id: &str,
        tool_call_id: ToolCallId,
        tool_name: &str,
        input: JsonSummary,
    ) -> Result<ToolCallResult, DispatchError> {
        let (handler, spec, validator) = {
            let g = self.tools.read().expect("RwLock poisoned");
            let entry = g
                .get(tool_name)
                .ok_or_else(|| DispatchError::NotRegistered(tool_name.into()))?;
            (
                entry.handler.clone(),
                entry.spec.clone(),
                entry.input_validator.clone(),
            )
        };

        if let Some(v) = validator {
            if let Some(msg) = v(&input) {
                return Err(DispatchError::InvalidInput(msg));
            }
        }

        let started_at: OccurredAt = Utc::now();

        // PayloadStore 决策（input 端）
        let (input_summary, input_payload_ref) =
            self.split_payload(PayloadKind::ToolInput, &input)?;

        // Persist initial ToolCall row (no output yet).
        let initial = ToolCall {
            tool_call_id: tool_call_id.clone(),
            run_id: run_id.to_string(),
            name: tool_name.to_string(),
            input_summary: input_summary.clone(),
            input_payload_ref: input_payload_ref.clone(),
            output_summary: None,
            output_payload_ref: None,
            is_error: false,
            error_code: None,
            started_at,
            ended_at: None,
            duration_ms: None,
        };
        if let Some(repo) = &self.repo {
            repo.upsert_tool_call(&initial)?;
        }

        let invocation = ToolInvocation {
            run_id: run_id.to_string(),
            tool_call_id: tool_call_id.clone(),
            name: tool_name.to_string(),
            input: input.clone(),
        };

        let timeout = Duration::from_millis(spec.timeout_ms);
        let started_instant = std::time::Instant::now();
        let result = tokio::time::timeout(timeout, handler.invoke(invocation)).await;
        let duration_ms = started_instant.elapsed().as_millis() as u64;

        let output: ToolHandlerOutput = match result {
            Ok(out) => out,
            Err(_) => ToolHandlerOutput::err(
                serde_json::json!({ "reason": "tool_timeout", "timeoutMs": spec.timeout_ms }),
                ErrorCode::ToolTimeout,
            ),
        };

        // PayloadStore 决策（output 端）
        let (output_summary, output_payload_ref) =
            self.split_payload(PayloadKind::ToolOutput, &output.output_summary)?;

        let ended_at = Utc::now();
        let final_call = ToolCall {
            tool_call_id: tool_call_id.clone(),
            run_id: run_id.to_string(),
            name: tool_name.to_string(),
            input_summary,
            input_payload_ref,
            output_summary: Some(output_summary.clone()),
            output_payload_ref: output_payload_ref.clone(),
            is_error: output.is_error,
            error_code: output.error_code,
            started_at,
            ended_at: Some(ended_at),
            duration_ms: Some(duration_ms),
        };
        if let Some(repo) = &self.repo {
            repo.upsert_tool_call(&final_call)?;
        }

        Ok(ToolCallResult {
            tool_call_id,
            output_summary: output.output_summary,
            output_payload_ref,
            is_error: output.is_error,
            error_code: output.error_code,
            duration_ms,
        })
    }

    /// 按 spec §2 PayloadStore 阈值规则切分 summary / payload_ref。
    /// - 序列化后 ≤ 8KB：summary = 完整 payload，ref = None。
    /// - > 8KB：summary = 截断摘要（前 1KB + `"[truncated, see ref]"`），完整数据写 PayloadStore，ref = `pl_...`。
    fn split_payload(
        &self,
        kind: PayloadKind,
        content: &JsonSummary,
    ) -> Result<(JsonSummary, Option<String>), DispatchError> {
        let serialized = serde_json::to_string(content).unwrap_or_default();
        if serialized.len() <= PAYLOAD_INLINE_LIMIT_BYTES {
            return Ok((content.clone(), None));
        }
        // 走 PayloadStore
        if let Some(store) = &self.payload_store {
            let payload_id = store.put_json(kind, content)?;
            let truncated_text: String =
                serialized.chars().take(1024).collect::<String>() + "[truncated, see ref]";
            let summary = serde_json::json!({
                "_truncated": true,
                "preview": truncated_text,
                "payloadRef": payload_id,
            });
            Ok((summary, Some(payload_id)))
        } else {
            // 没有 store（测试场景）：保留原样，无 ref。
            Ok((content.clone(), None))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::SideEffect;
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::db::{run_migrations, AppDb};

    fn echo_handler() -> Arc<dyn ToolHandler> {
        Arc::new(FnToolHandler(|inv: ToolInvocation| {
            Box::pin(async move { ToolHandlerOutput::ok(inv.input) }) as ToolHandlerFuture
        }))
    }

    fn spec(name: &str, timeout_ms: u64) -> ToolSpec {
        ToolSpec::new(
            name,
            "test",
            serde_json::json!({"type":"object"}),
            vec![format!(r#"<use_tool name="{}">{{}}</use_tool>"#, name)],
            timeout_ms,
            SideEffect::None,
        )
    }

    #[test]
    fn register_rejects_duplicate() {
        let r = ToolRegistry::new_without_persist();
        r.register_tool(spec("echo", 5000), echo_handler()).unwrap();
        let err = r
            .register_tool(spec("echo", 5000), echo_handler())
            .expect_err("dup must error");
        assert!(matches!(err, RegisterError::Duplicate(_)));
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_errors() {
        let r = ToolRegistry::new_without_persist();
        let err = r
            .dispatch_tool_call("r1", "tc_1".into(), "missing", serde_json::json!({}))
            .await
            .expect_err("unknown");
        assert!(matches!(err, DispatchError::NotRegistered(_)));
    }

    #[tokio::test]
    async fn dispatch_runs_handler() {
        let r = ToolRegistry::new_without_persist();
        r.register_tool(spec("echo", 5000), echo_handler()).unwrap();
        let out = r
            .dispatch_tool_call("r1", "tc_1".into(), "echo", serde_json::json!({"a":1}))
            .await
            .unwrap();
        assert_eq!(out.output_summary["a"], 1);
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn dispatch_enforces_timeout() {
        let r = ToolRegistry::new_without_persist();
        let slow: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(|_inv| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                ToolHandlerOutput::ok(serde_json::json!({"never":true}))
            }) as ToolHandlerFuture
        }));
        r.register_tool(spec("slow", 50), slow).unwrap();
        let out = r
            .dispatch_tool_call("r1", "tc_slow".into(), "slow", serde_json::json!({}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::ToolTimeout));
    }

    #[tokio::test]
    async fn dispatch_validates_input_when_validator_registered() {
        let r = ToolRegistry::new_without_persist();
        let validator: InputValidator = Arc::new(|v: &JsonSummary| -> Option<String> {
            if v.is_object() {
                None
            } else {
                Some("expected object".into())
            }
        });
        r.register_tool_with_validator(spec("needs_obj", 5000), echo_handler(), Some(validator))
            .unwrap();
        let err = r
            .dispatch_tool_call("r1", "tc_x".into(), "needs_obj", serde_json::json!("not-obj"))
            .await
            .expect_err("must reject");
        assert!(matches!(err, DispatchError::InvalidInput(_)));
        let err = r
            .validate_tool_input("needs_obj", &serde_json::json!(123))
            .expect_err("must reject");
        assert!(matches!(err, DispatchError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn dispatch_preserves_caller_tool_call_id() {
        let r = ToolRegistry::new_without_persist();
        r.register_tool(spec("echo", 5000), echo_handler()).unwrap();
        let out = r
            .dispatch_tool_call("r1", "tc_specific".into(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(out.tool_call_id, "tc_specific");
    }

    #[tokio::test]
    async fn list_tools_returns_registered() {
        let r = ToolRegistry::new_without_persist();
        r.register_tool(spec("a", 1000), echo_handler()).unwrap();
        let specs = r.list_tools();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "a");
        assert!(r.has_tool("a"));
        assert!(!r.has_tool("b"));
    }

    #[tokio::test]
    async fn large_payload_routes_through_payload_store() {
        // Spec §2: > 8KB 走 ref。
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = AgentMessagesRepo::new(db.clone());
        let store = PayloadStore::new(db);
        let r = ToolRegistry::new(repo.clone(), store.clone());
        // Big handler returns > 8KB
        let big: Arc<dyn ToolHandler> = Arc::new(FnToolHandler(|_inv| {
            Box::pin(async move {
                let big_text = "x".repeat(10 * 1024); // 10KB
                ToolHandlerOutput::ok(serde_json::json!({"big": big_text}))
            }) as ToolHandlerFuture
        }));
        r.register_tool(spec("big", 5000), big).unwrap();
        let out = r
            .dispatch_tool_call("r1", "tc_big".into(), "big", serde_json::json!({}))
            .await
            .unwrap();
        // ToolCallResult always has full output_summary (LLM 视野 inline 全文)
        assert!(out.output_summary["big"].as_str().unwrap().len() >= 10 * 1024);
        // But the persisted ToolCall row's summary is the truncated stub, and payload_ref is set
        let persisted = repo.load_tool_call("tc_big").unwrap().unwrap();
        assert!(persisted.output_payload_ref.is_some());
        let ref_id = persisted.output_payload_ref.unwrap();
        let payload = store.get(&ref_id).unwrap().unwrap();
        let full = payload.content_json.unwrap();
        assert!(full["big"].as_str().unwrap().len() >= 10 * 1024);
    }

    #[tokio::test]
    async fn small_payload_kept_inline() {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        let repo = AgentMessagesRepo::new(db.clone());
        let store = PayloadStore::new(db);
        let r = ToolRegistry::new(repo.clone(), store);
        r.register_tool(spec("echo", 5000), echo_handler()).unwrap();
        let _ = r
            .dispatch_tool_call("r1", "tc_small".into(), "echo", serde_json::json!({"a":1}))
            .await
            .unwrap();
        let persisted = repo.load_tool_call("tc_small").unwrap().unwrap();
        assert!(persisted.output_payload_ref.is_none());
        assert!(persisted.input_payload_ref.is_none());
    }
}
