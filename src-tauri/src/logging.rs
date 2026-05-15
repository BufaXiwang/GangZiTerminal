//! 结构化日志初始化——tracing + tracing-subscriber + 文件 appender。
//!
//! 写入位置：app data dir 下 `logs/gangzi-terminal.YYYY-MM-DD.log`，按天滚动。
//! 默认 level 是 `info`；可通过环境变量 `GANGZI_LOG=debug` 覆盖。
//!
//! 同时输出到 stderr（dev 时方便看），生产时只看文件。
//!
//! 关键 span 由各模块自己加 `#[instrument]` 或 `tracing::info!`：
//! - agent::loop_::run_agent — pipeline + run_id + model + turn
//! - pipeline::briefing/review/chat — pipeline 启动 + 关键阶段
//! - scheduler 4 个 loop — tick 失败时 warn
//! - provider retry — attempt + delay + error

use tauri::{AppHandle, Manager};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Registry};

const LOG_DIR_NAME: &str = "logs";
const LOG_FILE_PREFIX: &str = "gangzi-terminal";

/// 初始化全局 tracing subscriber——返回 WorkerGuard 必须保留到进程结束，
/// 否则后台 flush 线程被 drop，未刷盘的日志丢失。
pub fn init(app: &AppHandle) -> Option<WorkerGuard> {
    let app_data_dir = match app.path().app_data_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[gangzi] 拿不到 app data dir，日志只走 stderr: {e}");
            init_stderr_only();
            return None;
        }
    };
    let log_dir = app_data_dir.join(LOG_DIR_NAME);
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!("[gangzi] 建日志目录失败 {}: {e}", log_dir.display());
        init_stderr_only();
        return None;
    }

    let file_appender = tracing_appender::rolling::daily(&log_dir, LOG_FILE_PREFIX);
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_from_env("GANGZI_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,gangzi_terminal=debug"));

    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_writer(file_writer);
    let stderr_layer = fmt::layer()
        .with_ansi(true)
        .with_target(false)
        .with_writer(std::io::stderr);

    if Registry::default()
        .with(env_filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .is_ok()
    {
        tracing::info!(
            log_dir = %log_dir.display(),
            "tracing 初始化完成，日志按天滚动写入"
        );
        Some(guard)
    } else {
        // 测试场景下 init 多次会失败——忽略
        None
    }
}

fn init_stderr_only() {
    let _ = Registry::default()
        .with(EnvFilter::new("info"))
        .with(fmt::layer().with_writer(std::io::stderr))
        .try_init();
}
