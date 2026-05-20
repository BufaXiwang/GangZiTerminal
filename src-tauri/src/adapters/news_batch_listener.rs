//! 监听 `news-batch-ready` 事件——`pipeline::news::batch_loop` 攒批 / 定时 / buffer overflow
//! 触发时 emit 该事件，本 listener 收到后构造 registry + 跑 `pipeline::agent::news_review::run`。
//!
//! pipeline 不允许 use adapters → registry 构造放在 adapter 层，通过 Tauri Event 解耦。
//!
//! **串行化保证**：batch_loop 在 claim 前已通过 REVIEW_IN_FLIGHT 锁阻挡了"上轮还
//! 没跑完就再 claim"——理论上 listener 永远不会收到重叠 event。但 emit 仍可能
//! 同瞬间被多次触发（refresh overflow + timer 撞车），所以这里再保险一层 Drop guard，
//! 跑完无论成功失败 / panic 都释放 REVIEW_IN_FLIGHT。
//!
//! **错误回退**：agent run 失败时不写 mark_failed——直接 revert 到 pending 让下次
//! batch 重试。Provider 超时 / 网络故障是常见且可恢复的，写 failed 会造成"瞬时
//! 故障 → 永久漏分析"。

use crate::adapters::agent_tools::build_chat_registry;
use crate::domain::news::NewsId;
use crate::infrastructure::news::batch;
use crate::pipeline::agent::news_review;
use crate::pipeline::news::batch_loop;
use serde::Deserialize;
use std::sync::Arc;
use tauri::{AppHandle, Listener};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
    batch_id: String,
    news_ids: Vec<String>,
    queued_remaining: u64,
    trigger_reason: String,
}

/// RAII guard——drop 时调 `batch_loop::mark_review_done()` 释放全局 in-flight 锁。
/// 保证无论 agent run 成功 / 失败 / async block 提前 return 都释放锁。
struct ReviewDoneGuard;

impl Drop for ReviewDoneGuard {
    fn drop(&mut self) {
        batch_loop::mark_review_done();
    }
}

pub fn spawn(app: AppHandle) {
    let app_for_handler = app.clone();
    app.listen("news-batch-ready", move |event| {
        // 注：batch_loop 在 emit 前已经 try_mark_review_starting 持有 REVIEW_IN_FLIGHT
        // 锁——本闭包**任何 early-return 路径**都必须释放它，否则锁永远泄露。
        // RAII guard 在 spawn 内部的 async block 才创建，所以 spawn 之前 return
        // 的两条路径在此处显式释放。
        let raw = event.payload();
        let parsed: Option<Payload> = serde_json::from_str(raw).ok();
        let Some(payload) = parsed else {
            tracing::warn!(payload = raw, "news-batch-ready payload 解析失败 → 释放锁");
            batch_loop::mark_review_done();
            return;
        };
        if payload.news_ids.is_empty() {
            tracing::warn!(batch_id = %payload.batch_id, "news-batch-ready news_ids 为空 → 释放锁");
            batch_loop::mark_review_done();
            return;
        }
        let app_clone = app_for_handler.clone();
        tauri::async_runtime::spawn(async move {
            // RAII：async block 结束时 drop → 释放 batch_loop 的 REVIEW_IN_FLIGHT
            let _guard = ReviewDoneGuard;

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
            // _guard drop here → REVIEW_IN_FLIGHT cleared
        });
    });
}
