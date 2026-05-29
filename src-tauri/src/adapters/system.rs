//! 系统级 Tauri command（不属于任何 BC）。
//!
//! `open_external`：用系统默认浏览器打开一个 http(s) 外链。
//! 前端不直接发外部请求 / 不裸调 plugin invoke——统一走这里转 `tauri_plugin_opener`。
//! 仅放行 http/https，避免被诱导打开 file:// / 自定义协议等本地 scheme。

use crate::adapters::error::CommandError;
use crate::domain::shared::ErrorCode;
use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

#[tauri::command]
#[specta::specta]
pub fn open_external(app: AppHandle, url: String) -> Result<(), CommandError> {
    let lower = url.trim().to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(CommandError::with_message(
            ErrorCode::InvalidInput,
            "open_external 仅允许 http(s) 外链",
        ));
    }
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| CommandError::with_message(ErrorCode::ProviderUnavailable, e.to_string()))
}
