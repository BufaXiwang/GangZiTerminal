//! Reflection scheduler——每个交易日 15:30 Asia/Shanghai 自动触发一次 reflection。
//!
//! 放在 adapters/ 是因为它要 import `adapters::agent_tools::build_chat_registry`
//! 构造 tool registry 注入 pipeline。`pipeline::scheduler` 不允许 use adapters。
//!
//! 实现：每 60s 检查一次「现在是否北京时间 15:30~15:35 且今天还没跑过」。
//! 用一个 in-memory cell 记录"今天已跑过的日期"——崩重启会重跑（幂等不强求）。
//!
//! 见 docs/design/agent-redesign.md § 6 触发模型 + § 4.2 reflection 用例。

use crate::adapters::agent_tools::build_chat_registry;
use crate::pipeline::agent::reflect::run_close_reflection;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::AppHandle;

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(close_reflection_loop(app));
}

async fn close_reflection_loop(app: AppHandle) {
    static LAST_FIRED_DATE: Mutex<Option<String>> = Mutex::new(None);

    // 启动延迟 30s，给前置 loop 一点时间稳定
    tokio::time::sleep(Duration::from_secs(30)).await;
    loop {
        let beijing = chrono::Utc::now() + chrono::Duration::hours(8);
        use chrono::{Datelike, Timelike};
        let weekday = beijing.weekday();
        let is_weekend = matches!(weekday, chrono::Weekday::Sat | chrono::Weekday::Sun);
        let hour = beijing.hour();
        let minute = beijing.minute();
        let today = beijing.format("%Y-%m-%d").to_string();

        // 触发窗口：15:30 ~ 15:35（盘后立刻跑；±5min 容错）
        let in_window = hour == 15 && (30..=35).contains(&minute);
        let already_fired = {
            let last = LAST_FIRED_DATE.lock().unwrap();
            last.as_deref() == Some(today.as_str())
        };

        if !is_weekend && in_window && !already_fired {
            tracing::info!(date = %today, "Close reflection tick 触发");
            let app_clone = app.clone();
            tauri::async_runtime::spawn(async move {
                let registry = Arc::new(build_chat_registry(&app_clone));
                match run_close_reflection(app_clone.clone(), registry).await {
                    Ok(res) => {
                        tracing::info!(
                            run_id = %res.run_id,
                            thesis_count = res.thesis_count,
                            outcome_len = res.outcome_summary.chars().count(),
                            "Close reflection 完成"
                        );
                        crate::infrastructure::scheduler_heartbeat::record_ok(
                            &app_clone,
                            crate::infrastructure::scheduler_heartbeat::LOOP_REFLECTION,
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Close reflection 失败");
                        crate::infrastructure::scheduler_heartbeat::record_err(
                            &app_clone,
                            crate::infrastructure::scheduler_heartbeat::LOOP_REFLECTION,
                            &e,
                        );
                    }
                }
            });
            *LAST_FIRED_DATE.lock().unwrap() = Some(today);
        }

        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
