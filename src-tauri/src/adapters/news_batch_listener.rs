//! 监听 `news-batch-ready` 事件——`pipeline::news::batch_loop` 攒批 / 定时 / buffer overflow
//! 触发时 emit 该事件，本 listener 收到后构造 registry + 跑 `pipeline::agent::news_review::run`。
//!
//! pipeline 不允许 use adapters → registry 构造放在 adapter 层，通过 Tauri Event 解耦。
//!
//! **串行化保证**：同一时间只允许 1 个 news_review run。多个 event 同时到达时，
//! 后到的等前一个 release。理由：
//! - 决策竞态——多个 agent 同时看 open positions 做相反决定
//! - Provider 限流——并发 LLM 调用容易撞 rate limit
//! - 多耗 token 无收益（news 已经按 batch 分组，没有抢资源的必要）
//!
//! **错误回退**：agent run 失败时**不写 mark_failed**——直接 revert 到 pending
//! 让下次 batch 重试。Provider 超时 / 网络故障是常见且可恢复的，写 failed 会
//! 造成"瞬时故障 → 永久漏分析"。真要标 failed 留给 agent 自己显式判定（未来扩展）。

use crate::adapters::agent_tools::build_chat_registry;
use crate::domain::news::NewsId;
use crate::infrastructure::news::batch;
use crate::pipeline::agent::news_review;
use serde::Deserialize;
use std::sync::Arc;
use tauri::{AppHandle, Listener};
use tokio::sync::Semaphore;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
    batch_id: String,
    news_ids: Vec<String>,
    queued_remaining: u64,
    trigger_reason: String,
}

pub fn spawn(app: AppHandle) {
    // 全局单次许可——保证同一时间只有 1 个 news_review run。
    // Arc 共享到每个 event handler 的 spawn 闭包里。
    let permit: Arc<Semaphore> = Arc::new(Semaphore::new(1));

    let app_for_handler = app.clone();
    app.listen("news-batch-ready", move |event| {
        let raw = event.payload();
        let parsed: Option<Payload> = serde_json::from_str(raw).ok();
        let Some(payload) = parsed else {
            tracing::warn!(payload = raw, "news-batch-ready payload 解析失败");
            return;
        };
        if payload.news_ids.is_empty() {
            return;
        }
        let app_clone = app_for_handler.clone();
        let permit = permit.clone();
        tauri::async_runtime::spawn(async move {
            // 等许可——前一个 news_review 跑完才能拿到
            let _guard = match permit.acquire_owned().await {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!(error = %e, "news_review semaphore acquire 失败（极罕见）");
                    return;
                }
            };
            let news_ids: Vec<NewsId> =
                payload.news_ids.iter().map(|s| NewsId::new(s.clone())).collect();
            let registry = Arc::new(build_chat_registry(&app_clone));
            match news_review::run(
                app_clone.clone(),
                registry,
                payload.batch_id.clone(),
                news_ids.clone(),
                payload.queued_remaining,
                payload.trigger_reason.clone(),
            )
            .await
            {
                Ok(run_id) => {
                    tracing::info!(
                        batch_id = %payload.batch_id,
                        run_id = %run_id,
                        count = news_ids.len(),
                        "news_review 完成 → mark_consumed"
                    );
                    if let Err(e) = batch::mark_consumed(&app_clone, &news_ids) {
                        tracing::warn!(error = %e, batch = %payload.batch_id, "mark_consumed 失败");
                    }
                }
                Err(e) => {
                    // 系统级错误（provider 超时 / 网络 / 模型配置）→ revert 到 pending
                    // 让下次 batch 重试。不写 failed 是为了避免"瞬时故障永久漏分析"。
                    tracing::warn!(
                        batch_id = %payload.batch_id,
                        error = %e,
                        count = news_ids.len(),
                        "news_review 失败 → revert 到 pending（等下次重试）"
                    );
                    if let Err(e2) = batch::revert_processing_to_pending(&app_clone, &news_ids) {
                        tracing::warn!(error = %e2, batch = %payload.batch_id, "revert_pending 失败");
                    }
                }
            }
            // _guard drop 在这里——许可释放，下一个 event 可以拿
        });
    });
}
