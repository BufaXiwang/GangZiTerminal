//! Chat reply 流水线——基于新 agent loop 的实现。
//!
//! 流程：
//! 1. 立刻写 user message（emit chat-message-appended，UI 即刻渲染）
//! 2. 读上下文（行情/持仓/记忆/学习/最近 briefing/最近消息）
//! 3. 构 AgentRequest（identity+instructions 进 system，上下文+用户输入进 user）
//! 4. 启 agent_run（agent_runs 表先插一行）
//! 5. spawn forwarder：把 AgentEvent 流转发给前端 + 累计文本
//! 6. await run_agent → 拿 RunSummary + 最终文本
//! 7. 写 assistant message + finalize agent_runs
//!
//! Memory 更新由 agent 通过 update_memory / remove_memory 工具自己写——
//! pipeline 不再 parse JSON。

use crate::pipeline::agent::config::{build_provider_for_channel, read_agent_config, ProviderKind};
use crate::pipeline::agent::observer;
use crate::infrastructure::agent::tools::{build_chat_registry, ToolContext};
use crate::domain::agent::types::{
    AgentEvent, AgentOptions, AgentRequest, Block, ContextBudget, Message, PipelineKind, Role,
    ServerSideTool, StopReason, SystemBlock, ToolDef,
};
use crate::pipeline::agent::{run_agent, SummarizeOptions};
use crate::infrastructure::account::watchlist;
use crate::pipeline::history::{
    build_assistant_content_json, build_compact_boundary_row, build_user_content_json,
    read_recent_chat_thread,
};
use crate::pipeline::{
    collect_relevant_codes, emit_status, fetch_market_overview, fetch_quotes_with_visibility,
    new_id, now_iso, read_investor_memory, read_position_events_for_open, read_positions,
};
use crate::pipeline::agent::prompt::{
    build_chat_dynamic_context, build_chat_system_context, ChatDynamicContextInput,
    ChatSystemContextInput, AGENT_IDENTITY, CHAT_SYSTEM_INSTRUCTIONS,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::AppHandle;
use tokio::sync::mpsc;

/// 单实例锁——和旧实现一样防用户连点"发送"
static CHAT_RUNNING: AtomicBool = AtomicBool::new(false);
struct ChatGuard;
impl Drop for ChatGuard {
    fn drop(&mut self) {
        CHAT_RUNNING.store(false, Ordering::SeqCst);
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatReplyResult {
    pub user_message_id: String,
    pub assistant_message_id: String,
    pub run_id: String,
}

#[tauri::command]
pub async fn send_chat_message_now(
    app: AppHandle,
    content: String,
    #[allow(non_snake_case)] images: Option<Vec<String>>,
) -> Result<ChatReplyResult, String> {
    if CHAT_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("上一次对话还在生成回复，请稍候。".into());
    }
    let _guard = ChatGuard;

    let trimmed = content.trim().to_string();
    let image_data_urls = images.unwrap_or_default();
    if trimmed.is_empty() && image_data_urls.is_empty() {
        return Err("对话内容为空".into());
    }

    // 配置完整性预检——缺 token / base_url 时立刻返错，不写消息也不起 run
    let cfg = read_agent_config(&app);
    cfg.ensure_ready()?;

    // 把粘贴/拖拽的 base64 落盘
    let image_paths = if image_data_urls.is_empty() {
        Vec::new()
    } else {
        crate::pipeline::chat_attachments::save_data_urls(&app, &image_data_urls)
    };

    // 1. 构造本轮 user message 的结构化 blocks（文本 + 可选图片），并落库。
    //    contentJson.blocks 留作下次加载历史时直接反序列化，不再二次 parse contentMd。
    let user_message_id = new_id();
    let user_text_for_record = if image_paths.is_empty() {
        trimmed.clone()
    } else {
        format!(
            "{}\n\n[用户附了 {} 张图片，请直接读图作为分析依据]",
            trimmed,
            image_paths.len()
        )
    };
    let mut user_blocks: Vec<Block> = Vec::new();
    user_blocks.push(Block::Text {
        text: user_text_for_record.clone(),
        cache_control: false,
    });
    for path in &image_paths {
        if let Some((mime, data)) = read_image_as_base64(path) {
            user_blocks.push(Block::Image { mime, data });
        }
    }
    let user_msg = json!({
        "id": user_message_id,
        "createdAt": now_iso(),
        "role": "user",
        "kind": "chat",
        // 兼容：contentMd 仍是用户原文（前端列表渲染用），结构化形态在 contentJson.blocks。
        "contentMd": trimmed,
        "contentJson": build_user_content_json(&user_blocks, &image_paths),
        "sourceTaskId": null,
        "sourceNewsIds": null,
        "sourceRecordId": null,
    });
    crate::infrastructure::agent::repository::append_chat_message(app.clone(), user_msg).map_err(|e| format!("写 user 消息失败：{e}"))?;

    // 2. 读上下文。
    //    - history（结构化）：DB 里最近的真实对话，若有 compact_boundary 会优先吃边界后的全部
    //    - dynamic：盘口/持仓/最近 briefing——每次 chat 重建，不进 cache prefix
    //    - static system：identity + 系统指令 + 投资者记忆 + 学习画像——打 cache_control
    emit_status(&app, "loading", "整理对话上下文…");
    // 不在加载阶段按条数砍历史——读全量历史，由 compact tier（MicroClear →
    // Summarize → Drop → HardLimit）按 token 决定保留多少。compact_boundary
    // 命中后只读 boundary-after 切片，工作集很小。
    //
    // exclude 掉本轮刚刚写入的 user_message_id，否则当前提问会在 messages 里出现两次。
    let (history_messages, boundary_summary) =
        read_recent_chat_thread(&app, Some(&user_message_id));
    let memory = read_investor_memory(&app);
    let positions = read_positions(&app).unwrap_or_default();
    let position_events = read_position_events_for_open(&app, &positions);
    let watchlist = watchlist::list_strings();
    let codes = collect_relevant_codes(&watchlist, &positions);
    let quotes_status = fetch_quotes_with_visibility(&app, "chat", codes).await;
    let quotes_availability = quotes_status.to_prompt_section();
    let market = fetch_market_overview(&app).await;

    // 3. 构 AgentRequest——multi-turn 结构化形态
    let dynamic_context = build_chat_dynamic_context(&ChatDynamicContextInput {
        market_overview: market.as_ref(),
        simulated_positions: &positions,
        position_events: &position_events,
        quotes_availability: quotes_availability.as_deref(),
    });
    let static_system_context = build_chat_system_context(&ChatSystemContextInput {
        investor_memory: Some(&memory),
    });

    // messages 拼装顺序（旧 → 新）：
    // [0] dynamic context（盘口/持仓/briefing；user）
    // [1] 若有 compact_boundary：摘要文本（user）
    // [2..N] 结构化历史（user/assistant 真实对话，带 tool_use/tool_result）
    // [last] 本轮新 user 提问（含图片 block）
    //
    // history_messages 已经通过 exclude_id 排掉了本轮刚写入的 user 消息——
    // 这里直接 extend 即可，不会重复。
    let mut messages: Vec<Message> = Vec::with_capacity(history_messages.len() + 3);
    messages.push(Message {
        role: Role::User,
        content: vec![Block::Text {
            text: dynamic_context,
            cache_control: false,
        }],
    });
    if let Some(summary) = boundary_summary {
        messages.push(Message {
            role: Role::User,
            content: vec![Block::Text {
                text: format!(
                    "[历史压缩边界——以下是早期对话的摘要，请视作既成事实]\n\n{summary}\n\n[摘要结束，下面是边界之后的真实对话]"
                ),
                cache_control: false,
            }],
        });
    }
    messages.extend(history_messages);
    messages.push(Message {
        role: Role::User,
        content: user_blocks.clone(),
    });

    let registry = Arc::new(build_chat_registry(&app));

    // 解析 chat pipeline 用的 (channel, model)——决定 provider build + tools 里要不要加
    // server-side web_search。
    let (chat_channel_ref, chat_model_ref) =
        cfg.resolve_pipeline(PipelineKind::Chat).map_err(|e| e)?;
    let chat_channel = chat_channel_ref.clone();
    let chat_model = chat_model_ref.to_string();

    // tools = 所有本地工具 + 可选的 server-side web_search。
    // 这里仍然用 AnthropicWebSearch variant 作为 canonical——OpenAI provider 在
    // serialize 时会把它降级翻译成 `{type:"web_search"}`。
    let mut tools = registry.to_tool_defs(true);
    let want_web_search = match chat_channel.wire_format {
        ProviderKind::Anthropic => chat_channel.enable_native_web_search,
        ProviderKind::OpenAIResponses => chat_channel.enable_web_search,
        ProviderKind::OpenAIChatCompletions => false,
    };
    if want_web_search {
        tools.push(ToolDef::ServerSide(ServerSideTool::AnthropicWebSearch {
            name: "web_search".into(),
            max_uses: Some(cfg.agent.max_search_calls_per_run),
            allowed_domains: vec![],
            blocked_domains: vec![],
        }));
    }

    let req = AgentRequest {
        // system 三段：identity → 指令 → 半静态投资上下文。
        // cache_control 只打在最后一段末尾——整段 system 形成一个 cache prefix，
        // 跨多轮 chat 复用（直到 update_memory / remove_memory 触发记忆变化才失效）。
        system: vec![
            SystemBlock {
                text: AGENT_IDENTITY.to_string(),
                cache_control: false,
            },
            SystemBlock {
                text: CHAT_SYSTEM_INSTRUCTIONS.to_string(),
                cache_control: false,
            },
            SystemBlock {
                text: static_system_context,
                cache_control: true,
            },
        ],
        tools,
        messages,
        options: AgentOptions {
            model: chat_model.clone(),
            max_tokens: 4096,
            temperature: Some(0.7),
            top_p: None,
            // 沿用 chat channel 的 thinking + effort 配置——adaptive 模式下模型自决
            // 是否真的开思考，简单问题（"茅台多少钱"）会自动跳过 thinking，复杂问题
            // （"帮我分析这条新闻的链路"）则启用，没有"chat 一定关 thinking"的硬约束。
            thinking: chat_channel.thinking_config(),
            effort: chat_channel.default_effort,
            max_turns: cfg.agent.max_turns_per_run,
            stop_sequences: vec![],
            tool_timeout_secs: Some(cfg.agent.tool_timeout_secs),
        },
        budget: ContextBudget {
            soft_limit_tokens: cfg.agent.context_soft_limit_tokens,
            hard_limit_tokens: cfg.agent.context_hard_limit_tokens,
            compact_keep_last_n: cfg.agent.compact_keep_last_n_turns,
            max_search_calls: cfg.agent.max_search_calls_per_run,
        },
        trigger_message_id: Some(user_message_id.clone()),
        pipeline: PipelineKind::Chat,
    };

    // 4. 启 run + observer
    let run_id = uuid::Uuid::new_v4().to_string();
    observer::start_run(
        &app,
        &run_id,
        PipelineKind::Chat,
        chat_channel.wire_format.as_str(),
        &chat_model,
        Some(&user_message_id),
    )?;
    let provider = build_provider_for_channel(&chat_channel)
        .map_err(|e| format!("构建 provider 失败：{e}"))?;

    // 5. 起一对 channel：一条给 forwarder（emit 给前端），一条给本地累积文本
    let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
    let app_for_collector = app.clone();
    // collector 累积四件事：
    //   (a) TextDelta 拼成 assistant 最终文本
    //   (b) update_memory / remove_memory 工具的 input —— 写到 assistant 消息的
    //       contentJson 里让 MemoryChips 仍然能展示这条对话沉淀的记忆增量
    //   (c) 把每条事件 emit 给前端供 UI 流式渲染
    //   (d) 用 TurnAccumulator 把 turn 边界识别出来，落 agent_run_turns 表
    let collector_run_id = run_id.clone();
    let collector = tokio::spawn(async move {
        use tauri::Emitter;
        let mut answer = String::new();
        let mut memory_updates: Vec<Value> = Vec::new();
        let mut memory_removals: Vec<Value> = Vec::new();
        let mut acc = observer::TurnAccumulator::new(collector_run_id);
        while let Some(ev) = rx.recv().await {
            let _ = app_for_collector.emit(observer::AGENT_EVENT, &ev);
            if let Some(rec) = acc.consume(&ev) {
                if let Err(e) = observer::record_turn(
                    &app_for_collector,
                    acc.run_id(),
                    rec.turn,
                    &rec.started_at,
                    &rec.ended_at,
                    rec.stop_reason,
                    rec.input_tokens,
                    rec.output_tokens,
                    rec.cache_read_tokens,
                    rec.local_tool_calls,
                    rec.server_tool_calls,
                    None,
                ) {
                    tracing::warn!(error = %e, run_id = acc.run_id(), turn = rec.turn, "落 agent_run_turns 失败");
                }
            }
            match &ev {
                AgentEvent::TextDelta { delta, .. } => answer.push_str(delta),
                AgentEvent::ToolStart { name, input, .. } => match name.as_str() {
                    "update_memory" => memory_updates.push(input.clone()),
                    "remove_memory" => memory_removals.push(input.clone()),
                    _ => {}
                },
                _ => {}
            }
        }
        (answer, memory_updates, memory_removals)
    });

    emit_status(&app, "running", "Agent 正在回复…");
    let ctx = ToolContext {
        run_id: run_id.clone(),
    };
    // chat 是唯一会跨 turn 累积长上下文的 pipeline——启用 LLM 摘要兜底。
    // compact assignment 可能在不同渠道；如果同 chat 渠道则复用 provider，否则单独 build。
    let summarize_opts = if let Some((compact_chan, compact_model)) = cfg.resolve_compact() {
        let compact_provider = if compact_chan.id == chat_channel.id {
            None // 同渠道复用主 provider
        } else {
            match build_provider_for_channel(compact_chan) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "compact provider 构建失败——本 run 不启用 LLM 摘要");
                    None
                }
            }
        };
        Some(SummarizeOptions {
            provider: compact_provider,
            model: compact_model.to_string(),
            trigger_threshold_tokens: cfg.agent.context_summarize_threshold,
            max_consecutive_failures: cfg.agent.summarize_max_consecutive_failures,
            keep_last_n_turns: cfg.agent.compact_keep_last_n_turns,
        })
    } else {
        // compact 没分配——长上下文撞 threshold 时直接走 Drop tier
        None
    };
    let summary_result = run_agent(provider, summarize_opts, registry, req, ctx, tx).await;

    // 6. drain collector（tx drop 后 rx 自然 close）
    let (assistant_text, memory_updates, memory_removals) = collector
        .await
        .map_err(|e| format!("collector join 失败：{e}"))?;

    let summary = match summary_result {
        Ok(s) => s,
        Err(e) => {
            // run 失败——发一条 Error 事件（让前端 streaming 卡片消失）+ 补 ended_at + error，
            // 再写一条系统消息让用户看到。注意 collector 已经退出了，这条 Error 事件
            // 直接通过 Tauri emit 而不经 mpsc。
            let err_msg = format!("agent run 失败：{e}");
            use tauri::Emitter;
            let _ = app.emit(
                observer::AGENT_EVENT,
                &AgentEvent::Error {
                    run_id: run_id.clone(),
                    message: err_msg.clone(),
                },
            );
            let _ = observer::finalize_failure(&app, &run_id, &err_msg);
            let sys = json!({
                "id": new_id(),
                "createdAt": now_iso(),
                "role": "system",
                "kind": "system",
                "contentMd": format!("对话失败：{err_msg}"),
                "contentJson": null,
                "sourceTaskId": null, "sourceNewsIds": null, "sourceRecordId": null,
            });
            let _ = crate::infrastructure::agent::repository::append_chat_message(app.clone(), sys);
            return Err(err_msg);
        }
    };

    // 7. 写 assistant message + finalize run
    let assistant_message_id = new_id();
    let truncated = matches!(summary.stop_reason, StopReason::MaxTokens);
    let final_text = if assistant_text.trim().is_empty() {
        // 没有 TextDelta（极少见——agent 全程只调工具没说话）
        "（Agent 未输出文本回复）".to_string()
    } else if truncated {
        // chat 不像 briefing/review 要求结构化输出——截断的回复仍有用，
        // 保留已经流出的文本，尾部加一行提示让用户知道为什么戛然而止。
        format!("{assistant_text}\n\n_（回复被 max_tokens 截断，可发送「继续」让我接着写。）_")
    } else {
        assistant_text
    };
    // memory_updates / memory_removals 是 ToolStart input 的合并形态——单条 chat 内
    // agent 可能调多次工具，每次 input 是一条 InvestorMemoryUpdate。前端 MemoryChips
    // 历来按"单一 update + removal"渲染，这里把多条 input merge 成一条用于展示。
    // 注意：merge 不影响 DB——DB 已经被 update_memory 工具逐次写入。
    let merged_updates_for_display = merge_memory_inputs_for_display(&memory_updates);
    let merged_removals_for_display = merge_memory_inputs_for_display(&memory_removals);

    // assistant 持久化形态：
    // - contentMd：最终展示文本（含 max_tokens 截断提示）——前端列表渲染用
    // - contentJson.blocks：完整结构化 final_message.content（含 tool_use 块），
    //   下次 chat 加载时直接反序列化，恢复多轮工具调用上下文
    // - contentJson.{runId, turns, ...}：运行元数据 + memory chip 显示数据
    // 优先用 summary.final_message 的结构化 blocks（包含 tool_use 块——下次加载历史
    // 时能让模型看到上一轮调用了哪些工具）。极端情况（流早断、无 MessageComplete）
    // 退回到 final_text 兜成单 Text block，保证下次 row_to_message 不会因为空 blocks
    // 而把这条消息丢掉。
    let assistant_blocks: Vec<Block> = match summary.final_message.as_ref() {
        Some(m) if !m.content.is_empty() => m.content.clone(),
        _ => vec![Block::Text {
            text: final_text.clone(),
            cache_control: false,
        }],
    };
    let assistant_extras = json!({
        "runId": run_id,
        "turns": summary.turns,
        "localToolCalls": summary.local_tool_calls,
        "serverToolCalls": summary.server_tool_calls,
        // MemoryChips 兼容：保持和老版本同字段名
        "memoryUpdates": merged_updates_for_display,
        "memoryRemovals": merged_removals_for_display,
    });
    let assistant_msg = json!({
        "id": assistant_message_id,
        "createdAt": now_iso(),
        "role": "assistant",
        "kind": "chat",
        "contentMd": final_text,
        "contentJson": build_assistant_content_json(&assistant_blocks, assistant_extras),
        "sourceTaskId": null,
        "sourceNewsIds": null,
        "sourceRecordId": null,
    });
    crate::infrastructure::agent::repository::append_chat_message(app.clone(), assistant_msg)
        .map_err(|e| format!("写 assistant 消息失败：{e}"))?;

    // 若本 run 触发过 Summarize tier 且摘要成功，落一行 compact_boundary——
    // 下次 chat 加载历史时 read_recent_chat_thread 会优先取这行之后的对话，
    // 把摘要文本作为开头的 user 消息 prepend，让上下文不丢。
    // 注意：这条边界**写在 assistant 消息之后**——它的语义是"到此（含 assistant 回复）
    // 为止的早期对话已经被压缩"，下一次 chat 起步时之前的全部都是边界 prefix。
    if let Some(summary_text) = summary.last_summary_text.as_ref() {
        if !summary_text.trim().is_empty() {
            let compact_model_label = cfg
                .resolve_compact()
                .map(|(_, m)| m.to_string())
                .unwrap_or_default();
            let mut row = build_compact_boundary_row(
                summary_text,
                summary.total_input_tokens,
                &compact_model_label,
                summary.summarize_dropped_messages,
            );
            // build 出来的 row 缺 id / createdAt / 三个 source 字段——补上。
            if let Value::Object(map) = &mut row {
                map.insert("id".into(), Value::String(new_id()));
                map.insert("createdAt".into(), Value::String(now_iso()));
                map.insert("sourceTaskId".into(), Value::Null);
                map.insert("sourceNewsIds".into(), Value::Null);
                map.insert("sourceRecordId".into(), Value::Null);
            }
            if let Err(e) = crate::infrastructure::agent::repository::append_chat_message(app.clone(), row) {
                tracing::warn!(error = %e, "落 compact_boundary 行失败——下次 chat 加载会回到 N 条 history");
            }
        }
    }

    let _ = observer::finalize(&app, &summary, None);

    emit_status(&app, "done", "");

    Ok(ChatReplyResult {
        user_message_id,
        assistant_message_id,
        run_id,
    })
}

/// 把多次 update_memory / remove_memory 工具调用的 input 合并成一条
/// `InvestorMemoryUpdate` 形态（list 字段 concat 去重，单字符串字段用最后一条）。
/// 仅用于 MemoryChips 展示——DB 实际写入已经由工具完成。
fn merge_memory_inputs_for_display(inputs: &[Value]) -> Value {
    if inputs.is_empty() {
        return Value::Null;
    }
    let mut out = serde_json::Map::new();
    let list_fields = [
        "focusThemes",
        "preferredMarkets",
        "learningGoals",
        "knownBiases",
        "investmentPrinciples",
        "watchQuestions",
        "recentInsights",
    ];
    for input in inputs {
        let Some(obj) = input.as_object() else {
            continue;
        };
        for (key, value) in obj {
            if list_fields.contains(&key.as_str()) {
                if let Some(arr) = value.as_array() {
                    let entry = out
                        .entry(key.clone())
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let Value::Array(existing) = entry {
                        for item in arr {
                            if !existing.contains(item) {
                                existing.push(item.clone());
                            }
                        }
                    }
                }
            } else {
                // 单字符串字段（riskPreference）——取最后一条
                out.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(out)
}

/// 把磁盘上的图读成 base64 + 推断 mime——chat 的 user 图片附件转成 Block::Image。
fn read_image_as_base64(path: &str) -> Option<(String, String)> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let bytes = std::fs::read(path).ok()?;
    let mime = if path.ends_with(".png") || path.ends_with(".PNG") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else if path.ends_with(".gif") {
        "image/gif"
    } else {
        // Anthropic 只支持 png/jpeg/webp/gif——其他格式丢弃
        return None;
    };
    Some((mime.to_string(), STANDARD.encode(bytes)))
}
