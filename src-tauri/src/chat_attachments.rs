//! 把 chat 输入框粘/拖进来的图片落盘，存到 app 数据目录下，返回绝对路径。
//!
//! 设计：图本身只在磁盘上存一份；chat_messages.content_json.images 只存绝对路径列表。
//! 这样 SQLite 不会因为多张图爆体积；前端通过 Tauri assetProtocol 直接加载文件 URL。
//!
//! 文件路径：`<app_data_dir>/chat-images/<uuid>.<ext>`
//!
//! 接受的输入：browser 给的标准 data URL（`data:image/png;base64,iVBORw0...`）。
//! 检测出 mime 后选合适的扩展名，然后 base64 解码写盘。
//! 失败的图（坏的 data URL、不是图片）静默跳过——不阻塞用户的对话发送。

use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

const MAX_IMAGES_PER_MESSAGE: usize = 4;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_BASE64_CHARS: usize = (MAX_IMAGE_BYTES * 4 / 3) + 4096;

/// 接受 data URL 列表，落盘后返回每张图的绝对路径。
/// 失败的项被跳过，返回 Vec 长度可能小于入参——caller 可以对比识别。
pub fn save_data_urls(app: &AppHandle, data_urls: &[String]) -> Vec<String> {
    let dir = match images_dir(app) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "chat-images 目录不可用");
            return Vec::new();
        }
    };
    let mut out = Vec::with_capacity(data_urls.len());
    for url in data_urls.iter().take(MAX_IMAGES_PER_MESSAGE) {
        match decode_data_url(url) {
            Some((ext, bytes)) => {
                let filename = format!("{}.{}", uuid::Uuid::new_v4(), ext);
                let path = dir.join(&filename);
                if let Err(err) = std::fs::write(&path, &bytes) {
                    tracing::warn!(path = %path.display(), error = %err, "chat 图写盘失败");
                    continue;
                }
                out.push(path.to_string_lossy().into_owned());
            }
            None => {
                tracing::debug!(prefix = %url.chars().take(40).collect::<String>(), "跳过无效的 chat data URL");
            }
        }
    }
    out
}

fn images_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法定位 app_data_dir：{e}"))?;
    let dir = base.join("chat-images");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建 chat-images 目录失败：{e}"))?;
    Ok(dir)
}

/// 解析 `data:image/<ext>;base64,<payload>` → (扩展名, 字节)。
/// 不是 image/* 直接拒绝。识别 png/jpg/jpeg/webp/gif 几种常见格式。
fn decode_data_url(url: &str) -> Option<(String, Vec<u8>)> {
    let stripped = url.strip_prefix("data:")?;
    let (header, payload) = stripped.split_once(',')?;
    if !header.contains(";base64") {
        return None;
    }
    let mime = header.split(';').next()?;
    let ext = match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => return None,
    };
    if payload.len() > MAX_BASE64_CHARS {
        return None;
    }
    let bytes = STANDARD.decode(payload.as_bytes()).ok()?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    Some((ext.to_string(), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_valid_png_data_url() {
        // 1×1 透明 PNG
        let url = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
        let (ext, bytes) = decode_data_url(url).unwrap();
        assert_eq!(ext, "png");
        assert!(bytes.len() > 50);
        // PNG 文件头 magic
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn rejects_non_image_data_url() {
        let url = "data:text/plain;base64,SGVsbG8=";
        assert!(decode_data_url(url).is_none());
    }

    #[test]
    fn rejects_non_base64_payload() {
        let url = "data:image/png;utf8,foo";
        assert!(decode_data_url(url).is_none());
    }

    #[test]
    fn rejects_garbage_input() {
        assert!(decode_data_url("not a data url").is_none());
        assert!(decode_data_url("data:image/png").is_none());
    }

    #[test]
    fn rejects_non_whitelisted_image_type() {
        // 之前的实现给 image/heic / image/svg+xml 落 .bin 后端用 anthropic 模型用不了；
        // 现在统一拒绝，前后端 + 模型白名单一致：png/jpeg/webp/gif。
        let heic = "data:image/heic;base64,AAAA";
        assert!(decode_data_url(heic).is_none());
        let svg = "data:image/svg+xml;base64,PHN2Zz48L3N2Zz4=";
        assert!(decode_data_url(svg).is_none());
    }

    #[test]
    fn rejects_oversize_payload_before_decoding() {
        // 构造一个 base64 长度超过 MAX_BASE64_CHARS 的 payload——short-circuit 在解码前拒绝
        let big = "A".repeat(MAX_BASE64_CHARS + 1);
        let url = format!("data:image/png;base64,{big}");
        assert!(decode_data_url(&url).is_none());
    }

    #[test]
    fn accepts_jpeg_and_webp_and_gif() {
        for mime in ["image/jpeg", "image/jpg", "image/webp", "image/gif"] {
            let url = format!("data:{mime};base64,AAAA"); // 4 字节 base64 → 3 字节
            let (ext, _) = decode_data_url(&url).unwrap();
            // 扩展名走预定义映射，不直接用 mime 子串
            assert!(matches!(ext.as_str(), "jpg" | "webp" | "gif"));
        }
    }
}
