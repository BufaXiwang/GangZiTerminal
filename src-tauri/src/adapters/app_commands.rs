//! App-level Tauri commands.

#[tauri::command]
pub fn open_external_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("只允许打开 http/https 原文链接。".to_string());
    }
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|err| err.to_string())
}
