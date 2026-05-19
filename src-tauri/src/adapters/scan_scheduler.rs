//! Scan scheduler——v3 expectation-driven 自驱观察循环。
//!
//! 见 docs/design/agent-v3-expectation-driven.md § 5。
//!
//! 9 ticks（Asia/Shanghai 北京时间）：
//! - 盘前 09:15
//! - 盘中 09:40 / 10:10 / 10:40 / 11:10
//! - 盘中 13:10 / 13:40 / 14:10（跳过 14:40 / 15:00 防尾盘 noise）
//! - 盘后 15:30
//!
//! 周末跳过。每个 tick 通过 in-memory `LAST_FIRED` 防同窗口重复触发。
//!
//! 放在 adapters/ 因为它要 import `adapters::agent_tools::build_chat_registry` 构造
//! ToolRegistry 注入 pipeline::scan。

use crate::adapters::agent_tools::build_chat_registry;
use crate::pipeline::agent::scan::{run_tick, ScanBudget};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::AppHandle;

/// 9 个 tick 时间表：(hour, minute, label)
const TICKS: &[(u32, u32, &str)] = &[
    (9, 15, "pre-open"),
    (9, 40, "10:40-10min"),
    (10, 10, "10:10"),
    (10, 40, "10:40"),
    (11, 10, "11:10-am-close"),
    (13, 10, "13:10-pm-open"),
    (13, 40, "13:40"),
    (14, 10, "14:10"),
    (15, 30, "post-close"),
];

/// 触发窗口宽度（分钟）——±2 分钟内算同一 tick。
const TICK_WINDOW_MIN: i64 = 2;

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(scan_loop(app));
}

async fn scan_loop(app: AppHandle) {
    // last fired key = (date_str, tick_label) → bool
    let last_fired: Arc<Mutex<HashMap<String, bool>>> = Arc::new(Mutex::new(HashMap::new()));

    // 启动延迟 60s
    tokio::time::sleep(Duration::from_secs(60)).await;

    loop {
        let beijing = chrono::Utc::now() + chrono::Duration::hours(8);
        use chrono::{Datelike, Timelike};
        let weekday = beijing.weekday();
        let is_weekend = matches!(weekday, chrono::Weekday::Sat | chrono::Weekday::Sun);

        if !is_weekend {
            let hour = beijing.hour();
            let minute = beijing.minute();
            let today = beijing.format("%Y-%m-%d").to_string();

            for (h, m, label) in TICKS {
                let key = format!("{}|{}", today, label);
                let in_window = hour == *h && (minute as i64 - *m as i64).abs() <= TICK_WINDOW_MIN;
                let already = {
                    let map = last_fired.lock().unwrap();
                    map.get(&key).copied().unwrap_or(false)
                };
                if in_window && !already {
                    let app_clone = app.clone();
                    let label_owned = label.to_string();
                    tauri::async_runtime::spawn(async move {
                        let registry = Arc::new(build_chat_registry(&app_clone));
                        match run_tick(
                            app_clone.clone(),
                            registry,
                            "scan",
                            &label_owned,
                            ScanBudget::default(),
                        )
                        .await
                        {
                            Ok(r) => {
                                tracing::info!(
                                    tick = %label_owned,
                                    tick_id = %r.tick_id,
                                    stocks = r.stocks_scanned,
                                    signals = r.signals_detected,
                                    mini_scans = r.mini_scans_triggered,
                                    skipped = r.mini_scans_skipped_budget,
                                    "Scan tick 完成"
                                );
                                crate::infrastructure::scheduler_heartbeat::record_ok(
                                    &app_clone,
                                    crate::infrastructure::scheduler_heartbeat::LOOP_SCAN,
                                );
                            }
                            Err(e) => {
                                tracing::warn!(tick = %label_owned, error = %e, "Scan tick 失败");
                                crate::infrastructure::scheduler_heartbeat::record_err(
                                    &app_clone,
                                    crate::infrastructure::scheduler_heartbeat::LOOP_SCAN,
                                    &e,
                                );
                            }
                        }
                    });
                    let mut map = last_fired.lock().unwrap();
                    map.insert(key, true);
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
