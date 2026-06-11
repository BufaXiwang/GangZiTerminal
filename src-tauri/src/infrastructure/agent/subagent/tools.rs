//! `run_subagent` / `run_skill` / `subagent_output` / `stop_subagent` 工具：
//! ToolSpec 定义 + handler 分发 + 注册（bootstrap 调用）+ 管理 API。
//!
//! Spec: docs/design/agent-infra-module.md §3.5 管理 API / §3.6 Infra 默认 Tools

use std::sync::Arc;

use serde::Deserialize;

use crate::domain::agent::{SideEffect, ToolSpec};
use crate::domain::shared::ErrorCode;
use crate::infrastructure::agent::tool_registry::{
    DispatchExt, RegisterError, ToolHandler, ToolHandlerFuture, ToolHandlerOutput, ToolInvocation,
    ToolRegistry,
};

use super::fork::{handle_run_skill, handle_run_subagent, parse_input, err_out};
use super::runtime::ForkHandle;
use super::{SKILL_TIMEOUT_MS, SUBAGENT_TIMEOUT_MS};

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
    /// `subagent_output(agentId)`：读后台任务进度 / 已产出（spec §3.5 管理 API）。
    Output,
    /// `stop_subagent(agentId)`：abort 一个运行中的子 run（spec §3.5 管理 API）。
    Stop,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentIdInput {
    agent_id: String,
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
                ForkTool::Output => {
                    let input: AgentIdInput = match parse_input(&inv) {
                        Ok(v) => v,
                        Err(e) => return e,
                    };
                    match subagent_output(&handle, &input.agent_id) {
                        Some(v) => ToolHandlerOutput::ok(v),
                        None => err_out(
                            ErrorCode::NotFound,
                            format!("unknown agentId: {}", input.agent_id),
                        ),
                    }
                }
                ForkTool::Stop => {
                    let input: AgentIdInput = match parse_input(&inv) {
                        Ok(v) => v,
                        Err(e) => return e,
                    };
                    let stopped = stop_subagent(&handle, &input.agent_id);
                    ToolHandlerOutput::ok(serde_json::json!({
                        "agentId": input.agent_id,
                        "stopped": stopped,
                    }))
                }
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
            handle: handle.clone(),
            which: ForkTool::Skill,
        }),
    )?;
    registry.register_tool(
        tool_spec_subagent_output(),
        Arc::new(ForkToolHandler {
            handle: handle.clone(),
            which: ForkTool::Output,
        }),
    )?;
    registry.register_tool(
        tool_spec_stop_subagent(),
        Arc::new(ForkToolHandler {
            handle,
            which: ForkTool::Stop,
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
    .spawn()
}

fn tool_spec_subagent_output() -> ToolSpec {
    ToolSpec::new(
        "subagent_output",
        "读一个后台子 agent 任务的状态 / 进度 / 已产出结果。agentId 来自 run_subagent(runInBackground=true) \
         的返回。status ∈ running/completed/failed/killed；completed 时 result 是子 agent 的最终结论。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "agentId": { "type": "string", "description": "子 agent 任务 id（sub_...）" }
            },
            "required": ["agentId"]
        }),
        vec![r#"<use_tool name="subagent_output">{"agentId":"sub_xxx"}</use_tool>"#.into()],
        15_000,
        SideEffect::None,
    )
    // 标 spawn-class：子 agent 不该管理父层任务（与 run_subagent 一起从子 registry 剔除）。
    .spawn()
}

fn tool_spec_stop_subagent() -> ToolSpec {
    ToolSpec::new(
        "stop_subagent",
        "停止一个运行中的子 agent 任务（后台或前台）。stopped=false 表示任务不存在或已终态。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "agentId": { "type": "string", "description": "子 agent 任务 id（sub_...）" }
            },
            "required": ["agentId"]
        }),
        vec![r#"<use_tool name="stop_subagent">{"agentId":"sub_xxx"}</use_tool>"#.into()],
        15_000,
        SideEffect::None,
    )
    .spawn()
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
    .spawn()
}

