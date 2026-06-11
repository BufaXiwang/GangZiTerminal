//! Dialogue orchestration — 用户消息 → 连续对话线程 run。
//!
//! Spec: docs/design/agent-runtime-module.md §6 编排流（dialogue trigger）

use crate::domain::agent::messages::AgentMessageBlock;
use crate::domain::agent::runtime::{AgentRun, AgentRunTrigger};

use super::{
    parse_data_url_image, user_msg_with_id,
    OrchestrationError, RuntimeServices,
};
use crate::pipeline::agent_runtime::executor::{execute_run, ExecuteRunParams};

/// dialogue run 结果：用户消息 id + 终态 run（spec §9 send_agent_message → `{messageId, runId}`）。
#[derive(Debug, Clone)]
pub struct DialogueRunResult {
    pub message_id: String,
    pub run: AgentRun,
}

impl RuntimeServices {
    /// dialogue：用户消息 → 连续对话线程。L3 = 当日已下单意图。
    pub async fn run_dialogue(
        &self,
        conversation_id: String,
        user_message: String,
    ) -> Result<AgentRun, OrchestrationError> {
        Ok(self.run_dialogue_detailed(conversation_id, user_message, Vec::new()).await?.run)
    }

    /// dialogue run，返回 `{messageId, run}`（spec §9 send_agent_message 回 `{messageId, runId}`）。
    ///
    /// `images`：多模态附件（data-URL 格式 `data:image/png;base64,...`）。解析后存入 PayloadStore、
    /// 构造 `AgentMessageBlock::Image`，由 HttpProvider build wire 时 deref 成 base64 发给模型。
    /// 非法 data-URL 静默跳过（warn）。`message_id` 同时作 trigger 的 messageId 与用户消息 id。
    pub async fn run_dialogue_detailed(
        &self,
        conversation_id: String,
        user_message: String,
        images: Vec<String>,
    ) -> Result<DialogueRunResult, OrchestrationError> {
        let channel = self.active_channel()?;
        let providers = self.providers(&channel)?;
        let intents = self.collect_intraday_intents().await;
        let message_id = format!("msg_{}", uuid::Uuid::new_v4());
        let mut msg = user_msg_with_id(&message_id, &user_message);
        if !images.is_empty() {
            if let Some((_, payload_store)) = self.deps.persist.as_ref() {
                for (i, data_url) in images.iter().enumerate() {
                    match parse_data_url_image(data_url) {
                        Some((mime, bytes)) => match payload_store.put_image(bytes, &mime) {
                            Ok(payload_id) => {
                                msg.blocks.push(AgentMessageBlock::Image {
                                    mime_type: mime,
                                    data_ref: format!("payload://{payload_id}"),
                                });
                            }
                            Err(e) => tracing::warn!(target: "runtime.dialogue", i, error=%e, "image put_image failed, skipped"),
                        },
                        None => tracing::warn!(target: "runtime.dialogue", i, "image not a valid data-URL, skipped"),
                    }
                }
            } else {
                tracing::warn!(target: "runtime.dialogue", images=images.len(), "images received but no PayloadStore (test env), skipped");
            }
        }
        // 图片-only 消息：去掉空文本 block（部分 provider 拒绝空 text block），只留 image。
        if msg.blocks.len() > 1 {
            msg.blocks.retain(|b| {
                !matches!(b, AgentMessageBlock::Text { text } if text.trim().is_empty())
            });
        }
        let input = vec![msg];

        let params = ExecuteRunParams {
            trigger: AgentRunTrigger::UserChat {
                message_id: message_id.clone(),
            },
            channel,
            providers,
            deps: self.deps.clone(),
            augment_registry: self.augment.clone(),
            realtime: vec![intents],
            input,
            conversation_id: Some(conversation_id),
            max_turns: self.max_turns,
            parent_run_id: None,
            repo: Some(self.messages_repo.clone()),
            event_sink: self.event_sink.clone(),
        cancel_registry: Some(self.cancel_registry.clone()),
        token_budget: self.token_budget,
        on_run_created: None,
        };
        let run = execute_run(&self.runs, &self.strategy, params).await?;
        Ok(DialogueRunResult { message_id, run })
    }
}
