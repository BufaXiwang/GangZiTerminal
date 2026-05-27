//! ToolRegistry — local tool 注册 / 校验 / 分发 / 超时控制。
//!
//! Spec: docs/design/agent-infra-module.md §3 / §4 / §5 Tool Registry API
//!
//! 关键约束：
//! - 同名 local tool 只能注册一次；重复注册 fail closed（§2 不变量）。
//! - dispatch 必须按 `ToolSpec.timeout_ms` 超时（§3）。
//! - dispatch 必须记录 `ToolCall` 开始和结束（§5）。
//! - tool input 必须按 `inputSchema` 通过 caller 提供的校验函数验证；
//!   失败作为 tool error 返回模型，不调用 handler。
//!
//! 注：本注册表 *不* 负责 server-side tools；那些由 ProviderChannel 配置直接下发。

use crate::domain::agent::{
    JsonSummary, ToolCall, ToolCallId, ToolCallResult, ToolCallSource, ToolSpec,
};
use crate::domain::shared::{ErrorCode, OccurredAt};
use crate::infrastructure::agent::messages_repo::AgentMessagesRepo;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// 工具 handler 的输入。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ToolInvocation {
    pub run_id: String,
    pub tool_call_id: ToolCallId,
    pub name: String,
    pub input: JsonSummary,
}

/// 工具 handler 的输出（Infra 视角）。
///
/// `output_summary` 必须可摘要展示（spec §2 ToolSpec rules）。
/// 副作用工具必须设置 `output_payload_ref`；只读工具可省略。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ToolHandlerOutput {
    pub output_summary: JsonSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_payload_ref: Option<String>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ErrorCode>,
}

impl ToolHandlerOutput {
    pub fn ok(summary: JsonSummary) -> Self {
        Self {
            output_summary: summary,
            output_payload_ref: None,
            is_error: false,
            error_code: None,
        }
    }
    pub fn err(summary: JsonSummary, code: ErrorCode) -> Self {
        Self {
            output_summary: summary,
            output_payload_ref: None,
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
/// Spec §2: Tool input 必须按 `inputSchema` 校验。Infra 默认行为是"接受任何 JSON"，
/// 由调用方通过 `set_input_validator` 注入实际 schema 校验器。
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
    #[error("tool spec must have source = local_tool, got {0:?}")]
    NotLocal(ToolCallSource),
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("tool '{0}' not registered")]
    NotRegistered(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("tool timeout after {0}ms")]
    Timeout(u64),
    #[error("repo: {0}")]
    Repo(#[from] crate::infrastructure::agent::messages_repo::RepoError),
}

/// 注册表实例。运行时持有为 Arc。
///
/// Spec §3/§4: dispatch 时必须先验证 input，再调用 handler，再持久化 ToolCall。
pub struct ToolRegistry {
    tools: RwLock<HashMap<String, ToolEntry>>,
    repo: Option<AgentMessagesRepo>,
}

impl ToolRegistry {
    /// 创建不带持久化的注册表（仅用于测试 / 单元）。
    pub fn new_without_persist() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            repo: None,
        }
    }

    /// 生产入口：注入持久化 repo（AgentMessagesRepo）。
    pub fn new(repo: AgentMessagesRepo) -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            repo: Some(repo),
        }
    }

    /// 注册 local tool。
    ///
    /// Spec §5 Tool Registry API: `register_tool(spec, handler) -> Result<()>`。
    pub fn register_tool(
        &self,
        spec: ToolSpec,
        handler: Arc<dyn ToolHandler>,
    ) -> Result<(), RegisterError> {
        self.register_tool_with_validator(spec, handler, None)
    }

    /// 注册 local tool 并附带 input schema 校验器。
    pub fn register_tool_with_validator(
        &self,
        spec: ToolSpec,
        handler: Arc<dyn ToolHandler>,
        input_validator: Option<InputValidator>,
    ) -> Result<(), RegisterError> {
        if !matches!(spec.source, ToolCallSource::LocalTool) {
            return Err(RegisterError::NotLocal(spec.source));
        }
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
        self.tools.read().expect("RwLock poisoned").contains_key(name)
    }

    /// 列出所有 ToolSpec — 喂 provider request 用。
    pub fn snapshot_specs(&self) -> Vec<ToolSpec> {
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

    /// Spec §5: `dispatch_tool_call`。
    ///
    /// 行为：
    /// 1. 查表；未注册 → `NotRegistered`，作为 tool error 返回模型（caller 决定）。
    /// 2. 校验 input；失败 → `InvalidInput`，作为 tool error 返回。
    /// 3. 持久化 ToolCall 起始记录。
    /// 4. 按 spec `timeout_ms` 执行 handler；超时 → 记录 `tool_timeout` 错误。
    /// 5. 持久化 ToolCall 结束 + 写入 summary / payload_ref。
    ///
    /// `tool_call_id` 必须由 caller 提供（spec §2 不变量：tool_use / tool_result 必须保留
    /// provider 原始 id 配对；loop executor 直接传 provider 给的 id）。
    pub async fn dispatch_tool_call(
        &self,
        run_id: &str,
        tool_call_id: ToolCallId,
        tool_name: &str,
        input: JsonSummary,
    ) -> Result<ToolCallResult, DispatchError> {
        // Snapshot handler + spec out of the lock to allow concurrent dispatch.
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

        // Persist initial ToolCall row (no output yet).
        let initial = ToolCall {
            tool_call_id: tool_call_id.clone(),
            run_id: run_id.to_string(),
            name: tool_name.to_string(),
            source: ToolCallSource::LocalTool,
            input_summary: input.clone(),
            input_payload_ref: None,
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
        let result =
            tokio::time::timeout(timeout, handler.invoke(invocation)).await;
        let duration_ms = started_instant.elapsed().as_millis() as u64;

        let output: ToolHandlerOutput = match result {
            Ok(out) => out,
            Err(_) => ToolHandlerOutput::err(
                serde_json::json!({ "reason": "tool_timeout", "timeoutMs": spec.timeout_ms }),
                ErrorCode::ToolTimeout,
            ),
        };

        let ended_at = Utc::now();
        let final_call = ToolCall {
            tool_call_id: tool_call_id.clone(),
            run_id: run_id.to_string(),
            name: tool_name.to_string(),
            source: ToolCallSource::LocalTool,
            input_summary: input,
            input_payload_ref: None,
            output_summary: Some(output.output_summary.clone()),
            output_payload_ref: output.output_payload_ref.clone(),
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
            is_error: output.is_error,
            error_code: output.error_code,
            duration_ms,
            output_payload_ref: output.output_payload_ref,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::ToolSideEffect;

    fn echo_handler() -> Arc<dyn ToolHandler> {
        Arc::new(FnToolHandler(|inv: ToolInvocation| {
            Box::pin(async move { ToolHandlerOutput::ok(inv.input) }) as ToolHandlerFuture
        }))
    }

    #[test]
    fn register_rejects_duplicate() {
        let r = ToolRegistry::new_without_persist();
        let spec = ToolSpec::new_local(
            "echo",
            "echo",
            serde_json::json!({"type":"object"}),
            5000,
            ToolSideEffect::None,
        );
        r.register_tool(spec.clone(), echo_handler()).unwrap();
        let err = r
            .register_tool(spec, echo_handler())
            .expect_err("dup must error");
        assert!(matches!(err, RegisterError::Duplicate(_)));
    }

    #[test]
    fn register_rejects_non_local_source() {
        let r = ToolRegistry::new_without_persist();
        let mut spec = ToolSpec::new_local(
            "x",
            "x",
            serde_json::json!({}),
            1,
            ToolSideEffect::None,
        );
        spec.source = ToolCallSource::ServerSideTool;
        let err = r.register_tool(spec, echo_handler()).expect_err("not local");
        assert!(matches!(err, RegisterError::NotLocal(_)));
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_errors() {
        let r = ToolRegistry::new_without_persist();
        let err = r
            .dispatch_tool_call("r1", "tc1".into(), "missing", serde_json::json!({}))
            .await
            .expect_err("unknown");
        assert!(matches!(err, DispatchError::NotRegistered(_)));
    }

    #[tokio::test]
    async fn dispatch_runs_handler() {
        let r = ToolRegistry::new_without_persist();
        let spec = ToolSpec::new_local(
            "echo",
            "echo",
            serde_json::json!({"type":"object"}),
            5000,
            ToolSideEffect::None,
        );
        r.register_tool(spec, echo_handler()).unwrap();
        let out = r
            .dispatch_tool_call("r1", "tc1".into(), "echo", serde_json::json!({"a":1}))
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
        let spec = ToolSpec::new_local(
            "slow",
            "slow",
            serde_json::json!({}),
            50, // 50ms timeout
            ToolSideEffect::None,
        );
        r.register_tool(spec, slow).unwrap();
        let out = r
            .dispatch_tool_call("r1", "tc-slow".into(), "slow", serde_json::json!({}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert_eq!(out.error_code, Some(ErrorCode::ToolTimeout));
    }

    #[tokio::test]
    async fn dispatch_validates_input_when_validator_registered() {
        let r = ToolRegistry::new_without_persist();
        let spec = ToolSpec::new_local(
            "needs_obj",
            "x",
            serde_json::json!({"type":"object"}),
            5000,
            ToolSideEffect::None,
        );
        let validator: InputValidator = Arc::new(|v: &JsonSummary| -> Option<String> {
            if v.is_object() {
                None
            } else {
                Some("expected object".into())
            }
        });
        r.register_tool_with_validator(spec, echo_handler(), Some(validator))
            .unwrap();
        let err = r
            .dispatch_tool_call("r1", "tcX".into(), "needs_obj", serde_json::json!("not-obj"))
            .await
            .expect_err("must reject");
        assert!(matches!(err, DispatchError::InvalidInput(_)));
        // also via standalone validate
        let err = r
            .validate_tool_input("needs_obj", &serde_json::json!(123))
            .expect_err("must reject");
        assert!(matches!(err, DispatchError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn dispatch_preserves_caller_tool_call_id() {
        // Spec §2 不变量：tool_use / tool_result 必须保留 provider 原始 id 配对。
        let r = ToolRegistry::new_without_persist();
        let spec = ToolSpec::new_local(
            "echo",
            "echo",
            serde_json::json!({}),
            5000,
            ToolSideEffect::None,
        );
        r.register_tool(spec, echo_handler()).unwrap();
        let out = r
            .dispatch_tool_call("r1", "provider-tc-xyz".into(), "echo", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(out.tool_call_id, "provider-tc-xyz");
    }

    #[tokio::test]
    async fn snapshot_specs_returns_registered() {
        let r = ToolRegistry::new_without_persist();
        let spec = ToolSpec::new_local(
            "a",
            "a",
            serde_json::json!({}),
            1000,
            ToolSideEffect::None,
        );
        r.register_tool(spec, echo_handler()).unwrap();
        let specs = r.snapshot_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "a");
        assert!(r.has_tool("a"));
        assert!(!r.has_tool("b"));
    }
}
