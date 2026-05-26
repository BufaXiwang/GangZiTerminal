//! Tracing 初始化。
//!
//! Spec: AGENTS.md（tracing / tracing-subscriber / tracing-appender）
//!
//! 默认行为：
//! - stderr 输出，按 `RUST_LOG` 过滤。
//! - 默认级别 `info`；设置 `GANGZI_LOG=debug` 走 debug。
//!
//! 后续可扩展文件 appender；目前先单 stderr。

use std::sync::Once;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

static INIT: Once = Once::new();

pub fn init() {
    INIT.call_once(|| {
        let filter = EnvFilter::try_from_env("GANGZI_LOG")
            .or_else(|_| EnvFilter::try_from_default_env())
            .unwrap_or_else(|_| EnvFilter::new("info"));

        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().with_target(true).with_thread_ids(false))
            .init();
    });
}
