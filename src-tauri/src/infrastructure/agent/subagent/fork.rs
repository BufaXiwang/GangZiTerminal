//! fork 执行底座：`run_forked_agent`（隔离子 run + 只回末轮文本 + 活动转发）+
//! 前台/后台两种跑法（`spawn_or_run` / `run_fork_detached`）+ `run_subagent`/`run_skill` handler 逻辑。
//!
//! Spec: docs/design/agent-infra-module.md §3.5（机制 / 三种执行模式 / 不变量）

use chrono::Utc;
use serde::Deserialize;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::domain::agent::{
    AgentEvent, AgentMessage, AgentMessageBlock, AgentMessageRole, AgentRunRequest,
    AgentStopReason, ContextBundle, SubAgentActivityKind,
};
use crate::domain::shared::ErrorCode;
use crate::infrastructure::agent::loop_executor::{run_agent_turn_forked, LoopError, ProviderStream};
use crate::infrastructure::agent::tool_registry::{ToolHandlerOutput, ToolInvocation};

use super::runtime::{ForkHandle, ForkRuntime};
use super::task_registry::{SubAgentProgress, SubAgentStatus};
use super::FORK_FINAL_MESSAGE_HINT;

// ───────────────────────── run_forked_agent（核心底座） ─────────────────────────

/// fork 执行底座（spec §3.5 `run_forked_agent`）。
///
/// 给定 `{ rt(子 run 的运行时上下文), prompt, system_prompt?, allowed_tools? }` → 起一个**子 run**
/// （复用 `run_agent_turn_forked`），全新 `conversation_id`（带 parent 关联），跑完返回
/// **子 run 末轮文本**（每遇 `ToolStart` 清空累加器 → 只剩不再调工具的末轮；对齐 Claude Code「最后一条
/// 消息」）。子的中间轮铺垫 / 工具过程不回给父（隔离）。并在子 prompt 末尾拼 `FORK_FINAL_MESSAGE_HINT`
/// 提示子把完整结论放最后一条消息。
///
/// `rt` 是**发起这次 fork 的运行时**（即当前正在执行的 run 的运行时）。顶层 run = `is_subagent=false`；
/// 若它本身已是子 agent（`is_subagent == true`）→ 拒绝（不允许嵌套，兜底守卫，对齐 CC `isInForkChild`）。
/// 通过守卫后，本函数内部调 `rt.child(agent_id)` 派生子 run 的运行时（置 `is_subagent=true`、
/// parent_run_id=子 run id、不回灌父 event_tx），原样包成 `DispatchExt` 透传进子 run 的 loop——于是子 run
/// 内部若再触发 fork，`spawn_or_run` 拿到的 rt 已是 `is_subagent=true`，被预检拒绝。
///
/// 子 run 的 registry 已剔除 spawn 工具，**正常不会再嵌套 fork**；本函数的布尔守卫只是兜底（防工具未被
/// 正确剔除）。没有深度计数。
///
/// `agent_id` 是子 run 标识（也是子 run 的 run_id）。
///
/// 返回 `Ok(final_text)` 或 `Err(...)`（子 run 失败 / 嵌套被拒 / provider 构造失败）。
pub async fn run_forked_agent(
    handle: &ForkHandle,
    rt: &ForkRuntime,
    agent_id: &str,
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
) -> Result<String, LoopError> {
    // 不嵌套兜底守卫（spec §3.5 不变量，对齐 CC `isInForkChild` 布尔）。`rt` 是**发起方**的运行时：
    // 若发起方本身已是子 agent（is_subagent==true）→ 拒绝。只有顶层 agent 能 fork。正常路径子 registry
    // 已剔除 spawn 工具不会走到这里；这是防工具未被正确剔除的兜底。没有任何数字比较。
    if rt.is_subagent {
        return Err(LoopError::Provider(
            "nested sub-agent not allowed; only the top-level agent can fork".to_string(),
        ));
    }
    // 通过守卫后派生子 run 的运行时（is_subagent=true、parent 关联子 run id、隔离 event_tx）。
    let child_rt = rt.child(agent_id);

    // 隔离上下文：全新 conversation_id，带 parent_run_id 关联（命名编码，无需 DB schema 改动）。
    let conversation_id = format!("fork:{}:{}", child_rt.parent_run_id, Uuid::new_v4());
    // 回写注册表：审计可从 SubAgentTask 定位子 run 的 agent_messages（spec §3.5 审计独立）。
    handle.tasks.set_conversation_id(agent_id, &conversation_id);

    // 子 registry：默认继承**父 run 的 per-run registry**（rt.registry；spec §3.5）；allowed_tools 收紧。
    let registry = handle
        .child_registry(handle.source_registry(rt), allowed_tools)
        .map_err(LoopError::Provider)?;

    // 子 run 的 providers：channel + fallback（继承父）。
    let mut providers: Vec<Box<dyn ProviderStream>> = Vec::new();
    providers.push((handle.provider_factory)(&child_rt.channel)?);
    for fb in &child_rt.fallback_channels {
        providers.push((handle.provider_factory)(fb)?);
    }

    // 子 run 的 input：一条 user message = prompt。统一在此拼上 fork 提示（spec §3.5「只回末轮文本」）：
    // 告诉子 agent 只有最后一条消息会被返回，必须把完整结论放进末轮。两条 fork 路径
    // （run_subagent 自由 prompt / run_skill SKILL.md 作 prompt）都经由此处，故提示统一注入最稳妥。
    let mut blocks = Vec::new();
    if let Some(sys) = system_prompt {
        // system_prompt 作为子 run 的引导：拼进 user prompt 前缀（隔离上下文，独立 system 由
        // SystemPromptBuilder 在 loop 内注入 tool 清单；这里把 skill 正文 / 引导塞进 user turn）。
        blocks.push(AgentMessageBlock::Text {
            text: format!("{sys}\n\n{prompt}\n\n{FORK_FINAL_MESSAGE_HINT}"),
        });
    } else {
        blocks.push(AgentMessageBlock::Text {
            text: format!("{prompt}\n\n{FORK_FINAL_MESSAGE_HINT}"),
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
        channel: child_rt.channel.clone(),
        max_turns: child_rt.max_turns,
        input,
        conversation_id: Some(conversation_id),
        compaction: child_rt.compaction.clone(),
        fallback_channels: child_rt.fallback_channels.clone(),
        retry: child_rt.retry.clone(),
        token_budget: None,
    };

    // 子 run 用**自己的** event 通道（隔离）：聚合 TextDelta 为最终文本；不回灌父。
    let (child_tx, mut child_rx) = mpsc::channel::<AgentEvent>(256);
    let context = ContextBundle::new(agent_id);

    // 子 run 内部再 fork 时，dispatch 读到的运行时上下文 = `child_rt`（is_subagent=true，由 child() 置）。
    // 经 `<use_tool name="run_subagent">` 触发的嵌套 fork 据此被 `spawn_or_run` 预检拒绝（无深度计数）。
    let child_ext = child_rt.clone().into_ext();

    let started = std::time::Instant::now();
    let run = run_agent_turn_forked(
        request,
        registry,
        context,
        providers,
        child_tx,
        handle.repo.clone(),
        Some(child_ext),
        None,
        // 父取消传播：父 run 被取消时，跑着的子 run 在 turn 边界 / provider stream 中停下
        // （spec §3 可取消）。stop_subagent 的 AbortHandle 是另一条独立停止通道。
        rt.cancel.clone(),
    );

    // 并行消费子事件 + 等子 run 完成。
    //
    // **只回末轮文本**（对齐 Claude Code「只取子 run 最后一条消息」，spec §3.5）：
    // 子 loop 的事件顺序在每个 turn 内固定为「该 turn 的全部 `TextDelta` → 该 turn 的 `ToolStart`/
    // `ToolEnd`」（provider 在 `next_turn` 里先 emit 文本，loop 随后逐个 dispatch tool）。一个**发起了
    // 工具调用**的 turn 不是末轮（loop 会继续推进下一轮）；**没有任何工具调用**的 turn 才是末轮（loop
    // `break`）。`TextDelta` 不带 turn 标记，故用 `ToolStart` 作 turn 边界信号：**每遇到一个 `ToolStart`
    // 就清空文本累加器**——于是任何带工具调用的 turn 的铺垫文本都被丢弃，loop 结束时累加器里只剩末轮
    // （不再发起工具调用、给出最终答案那轮）的文本。`tool_uses` 仍累计全程（进度统计不变）。
    // 前端可见性（spec §3.5）：把子 run 的活动 tagged 转发给前端（父 event_tx = 前端通道），
    // 让用户在子 agent 阻塞跑时看到它在搜什么 / 调什么工具 / 输出什么。**仅前端展示**——
    // 子的完整事件不进父 LLM 上下文（父只在 fork 完成拿末轮结论），故上下文卫生不变。
    let fwd_tx = rt.parent_event_tx.clone();
    let parent_run = rt.parent_run_id.clone();
    let parent_shared = rt.shared.clone();
    let sub_id = agent_id.to_string();
    let collector = async {
        let send_activity = |kind: SubAgentActivityKind, text: String| {
            let tx = fwd_tx.clone();
            let (run_id, agent_id) = (parent_run.clone(), sub_id.clone());
            async move {
                if let Some(tx) = tx {
                    let _ = tx
                        .send(AgentEvent::SubAgentActivity { run_id, agent_id, kind, text })
                        .await;
                }
            }
        };
        send_activity(SubAgentActivityKind::Started, String::new()).await;

        let mut text = String::new();
        let mut tool_uses: u32 = 0;
        let mut tokens: u32 = 0;
        while let Some(ev) = child_rx.recv().await {
            // 转发 tagged 活动给前端（不影响下方「只回末轮文本」聚合）。
            match &ev {
                AgentEvent::TextDelta { delta, .. } => {
                    send_activity(SubAgentActivityKind::Text, delta.clone()).await
                }
                AgentEvent::ToolStart { name, .. } => {
                    send_activity(SubAgentActivityKind::ToolStart, name.clone()).await
                }
                AgentEvent::ToolEnd { name, is_error, .. } => {
                    let t = if *is_error { format!("{name} ✗") } else { name.clone() };
                    send_activity(SubAgentActivityKind::ToolEnd, t).await
                }
                _ => {}
            }
            match ev {
                AgentEvent::TextDelta { delta, .. } => text.push_str(&delta),
                AgentEvent::ToolStart { .. } => {
                    // 进入工具调用 = 当前累计的文本属于非末轮的铺垫 → 丢弃，只保留之后（末轮）的文本。
                    text.clear();
                    tool_uses += 1;
                    handle.tasks.update_progress(
                        &sub_id,
                        SubAgentProgress {
                            tokens,
                            tool_uses,
                            duration_ms: started.elapsed().as_millis() as u64,
                        },
                    );
                }
                AgentEvent::Usage { input_tokens, output_tokens, .. } => {
                    // spec §3.5：子 usage 经 progress 累计并**回灌父 run 预算**（父在 turn 边界把
                    // extra_tokens 计入累计；超 budget → token_budget_exceeded）。
                    let turn_total = input_tokens.saturating_add(output_tokens);
                    tokens = tokens.saturating_add(turn_total);
                    if let Some(s) = &parent_shared {
                        s.add_tokens(u64::from(turn_total));
                    }
                    handle.tasks.update_progress(
                        &sub_id,
                        SubAgentProgress {
                            tokens,
                            tool_uses,
                            duration_ms: started.elapsed().as_millis() as u64,
                        },
                    );
                }
                _ => {}
            }
        }
        // 完成：回传末轮结论预览（前端把子 agent 面板标完成 + 显示结论摘要）。
        let preview: String = text.chars().take(200).collect();
        send_activity(SubAgentActivityKind::Done, preview).await;
        (text, tool_uses, tokens)
    };

    let (summary_res, (text, tool_uses, _evt_tokens)) = tokio::join!(run, collector);
    let duration_ms = started.elapsed().as_millis() as u64;
    let summary = summary_res?;
    let progress = SubAgentProgress {
        tokens: summary.input_tokens.saturating_add(summary.output_tokens),
        tool_uses,
        duration_ms,
    };
    // 终态记进注册表（前台 / 后台共用此函数；前台 caller 已 register 过）。
    let status = match summary.stop_reason {
        AgentStopReason::Error | AgentStopReason::ToolError => SubAgentStatus::Failed,
        AgentStopReason::Cancelled => SubAgentStatus::Killed,
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

pub(crate) fn parse_input<T: for<'de> Deserialize<'de>>(inv: &ToolInvocation) -> Result<T, ToolHandlerOutput> {
    serde_json::from_value::<T>(inv.input.clone()).map_err(|e| {
        ToolHandlerOutput::err(
            serde_json::json!({ "reason": "invalid_input", "message": e.to_string() }),
            ErrorCode::InvalidInput,
        )
    })
}

pub(crate) fn err_out(code: ErrorCode, msg: impl Into<String>) -> ToolHandlerOutput {
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

pub(crate) async fn handle_run_subagent(
    handle: ForkHandle,
    rt: ForkRuntime,
    inv: ToolInvocation,
) -> ToolHandlerOutput {
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
        &rt,
        &description,
        &input.prompt,
        None,
        input.allowed_tools.as_deref(),
        input.run_in_background,
    )
    .await
}

// ───────────────────────── run_skill handler ─────────────────────────

pub(crate) async fn handle_run_skill(
    handle: ForkHandle,
    rt: ForkRuntime,
    inv: ToolInvocation,
) -> ToolHandlerOutput {
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

    // run_skill 永远前台（产品契约：`{name, result}`）。spawn-detached：dispatch 超时不破坏终态记账。
    let agent_id = new_agent_id();
    handle.tasks.register(
        &agent_id,
        &rt.parent_run_id,
        "", // conversation_id 在 run_forked_agent 内生成后回写。
        &format!("skill {}", input.name),
        None,
    );
    match run_fork_detached(&handle, &rt, &agent_id, prompt.to_string(), Some(sys), None).await {
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

/// 在独立 task 里跑 `run_forked_agent` 并 await（前台 / skill 路径共用）。
///
/// 为什么 spawn 而不是直接 await：dispatch 层有 `ToolSpec.timeoutMs` 超时——超时时 registry 会
/// **drop handler future**。若子 run 直接跑在 handler future 里，drop 会把 `finish()` 终态记账一并
/// 砍掉 → 任务永久卡 `running`、子 run 消息戛然而止。spawn 后子 run 自治：handler future 被 drop
/// 只是没人等结果，子 run 照常跑完并把终态 / 结果落进任务注册表（`subagent_output` 可查）。
/// 同时把 JoinHandle 的 abort 挂进注册表 → 前台任务也能被 `stop_subagent` 停。
pub(crate) async fn run_fork_detached(
    handle: &ForkHandle,
    rt: &ForkRuntime,
    agent_id: &str,
    prompt: String,
    system_prompt: Option<String>,
    allowed_tools: Option<Vec<String>>,
) -> Result<String, LoopError> {
    let h = handle.clone();
    let r = rt.clone();
    let aid = agent_id.to_string();
    let join = tokio::spawn(async move {
        run_forked_agent(
            &h,
            &r,
            &aid,
            &prompt,
            system_prompt.as_deref(),
            allowed_tools.as_deref(),
        )
        .await
    });
    handle.tasks.set_abort(agent_id, join.abort_handle());
    match join.await {
        Ok(res) => res,
        Err(e) if e.is_cancelled() => Err(LoopError::Provider(
            "subagent stopped (stop_subagent)".to_string(),
        )),
        Err(e) => Err(LoopError::Provider(format!("subagent task panicked: {e}"))),
    }
}

/// 前台阻塞跑 / 后台 spawn——`run_subagent` 两种模式共用。`rt` 是**当前 run** 的 fork 运行时上下文
/// （由 dispatch 注入 / 退回 handle 默认解析得到）。
pub(crate) async fn spawn_or_run(
    handle: &ForkHandle,
    rt: &ForkRuntime,
    description: &str,
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    background: bool,
) -> ToolHandlerOutput {
    // 不嵌套预检（对齐 CC `isInForkChild` 布尔）：`rt` 是**当前发起 run** 的运行时。若它本身已是
    // 子 agent（is_subagent==true）→ 拒绝再 spawn（run_forked_agent 内会再判一次兜底）。只有顶层能 fork。
    // 没有任何数字比较。
    if rt.is_subagent {
        return err_out(
            ErrorCode::InvalidInput,
            "nested sub-agent not allowed; only the top-level agent can fork".to_string(),
        );
    }
    // spec §3.6：allowedTools 含未注册 tool name → invalid_input（不静默忽略）。在 register 任务
    // 之前预检，让模型直接收到可纠偏的错误。
    if let Err(msg) = handle.child_registry(handle.source_registry(rt), allowed_tools) {
        return err_out(ErrorCode::InvalidInput, msg);
    }

    let agent_id = new_agent_id();

    if background {
        // 后台：spawn 子 run，立即返回 agentId；完成时把 <task-notification> 推进父 run 的共享
        // 通知队列（父 loop 下一 turn 边界注入为 user message，进 LLM 上下文——spec §3.5）。
        // 先 register（避免子 run 瞬时完成时回调早于 register 的竞态），spawn 后再补挂 abort 句柄。
        handle.tasks.register(
            &agent_id,
            &rt.parent_run_id,
            "", // conversation_id 在 run_forked_agent 内生成后回写。
            description,
            None,
        );
        // 把**发起方**运行时（is_subagent=false）移进后台任务；run_forked_agent 内派生子 run 运行时。
        let initiator_rt = rt.clone();
        let child_handle = handle.clone();
        let prompt_owned = prompt.to_string();
        let sys_owned = system_prompt.map(|s| s.to_string());
        let allowed_owned: Option<Vec<String>> = allowed_tools.map(|a| a.to_vec());
        let tasks = handle.tasks.clone();
        let parent_shared = rt.shared.clone();
        let aid = agent_id.clone();

        let join = tokio::spawn(async move {
            let res = run_forked_agent(
                &child_handle,
                &initiator_rt,
                &aid,
                &prompt_owned,
                sys_owned.as_deref(),
                allowed_owned.as_deref(),
            )
            .await;
            // 完成通知（防重复）：push <task-notification> 进父 loop 的共享通知队列。
            if tasks.take_notify_flag(&aid) {
                let (status_str, usage, result_preview) = match &res {
                    Ok(text) => {
                        let (st, pr, _) = tasks
                            .snapshot(&aid)
                            .unwrap_or((SubAgentStatus::Completed, SubAgentProgress::default(), None));
                        let preview: String = text.chars().take(2000).collect();
                        (st.as_str().to_string(), pr, preview)
                    }
                    Err(e) => {
                        tasks.finish(&aid, SubAgentStatus::Failed, SubAgentProgress::default(), Some(e.to_string()));
                        ("failed".to_string(), SubAgentProgress::default(), e.to_string())
                    }
                };
                let note = format!(
                    "<task-notification agent_id=\"{aid}\"><status>{status_str}</status>\
                     <usage tokens=\"{}\" tool_uses=\"{}\"/><result>{result_preview}</result></task-notification>",
                    usage.tokens, usage.tool_uses
                );
                match &parent_shared {
                    Some(s) => s.push_notification(note),
                    None => tracing::warn!(
                        agent_id = %aid,
                        "background subagent finished but parent run has no shared notification \
                         queue — notification dropped (parent likely already ended)"
                    ),
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
        // 前台：阻塞等结果作为 tool_result 返回（spawn-detached：dispatch 超时不破坏终态记账）。
        handle.tasks.register(
            &agent_id,
            &rt.parent_run_id,
            "",
            description,
            None,
        );
        match run_fork_detached(
            handle,
            rt,
            &agent_id,
            prompt.to_string(),
            system_prompt.map(|s| s.to_string()),
            allowed_tools.map(|a| a.to_vec()),
        )
        .await
        {
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

// ───────────────────────── helpers ─────────────────────────

pub(crate) fn new_agent_id() -> String {
    format!("sub_{}", Uuid::new_v4())
}

pub(crate) fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    line.chars().take(max).collect()
}

/// skill name slug 校验（与 skill_tools 一致）：`^[a-z0-9][a-z0-9-]*$`。
pub(crate) fn is_valid_skill_name(name: &str) -> bool {
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

