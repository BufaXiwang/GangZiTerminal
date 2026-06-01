//! PayloadStore — skill input / output / image 完整 payload 持久化。
//!
//! Spec: docs/design/agent-infra-module.md §2 PayloadStore
//!
//! 写入触发（spec §2 规则）：
//! - skill input / output JSON 序列化后超过 **8KB** 时；
//! - 图片 attachment（任何尺寸都进 PayloadStore）。
//!
//! 第一阶段不实现 GC / retention policy；payload 永久保留，用于 decision episode replay。

use crate::domain::shared::OccurredAt;
use crate::infrastructure::db::AppDb;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 阈值（字节）。spec §2: > 8KB 走 ref。
pub const PAYLOAD_INLINE_LIMIT_BYTES: usize = 8 * 1024;

/// Payload 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadKind {
    SkillInput,
    SkillOutput,
    Image,
}

impl PayloadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PayloadKind::SkillInput => "skill_input",
            PayloadKind::SkillOutput => "skill_output",
            PayloadKind::Image => "image",
        }
    }

    // Intentionally an inherent `Option`-returning parser, not the std `FromStr` trait
    // (no Err type needed — unknown kinds are simply None).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "skill_input" => Some(PayloadKind::SkillInput),
            "skill_output" => Some(PayloadKind::SkillOutput),
            "image" => Some(PayloadKind::Image),
            _ => None,
        }
    }
}

/// PayloadStore 条目。
///
/// JSON 走 TEXT 列；image 走 BLOB 列。
#[derive(Debug, Clone)]
pub struct PayloadStoreEntry {
    pub payload_id: String,
    pub kind: PayloadKind,
    pub content_json: Option<serde_json::Value>,
    pub content_bytes: Option<Vec<u8>>,
    pub content_type: Option<String>,
    pub byte_size: usize,
    pub created_at: OccurredAt,
}

#[derive(Debug, thiserror::Error)]
pub enum PayloadStoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid kind '{0}'")]
    InvalidKind(String),
}

/// PayloadStore — 写入 / 读取 payload 全文。
#[derive(Clone)]
pub struct PayloadStore {
    db: AppDb,
}

impl PayloadStore {
    pub fn new(db: AppDb) -> Self {
        Self { db }
    }

    /// 把 JSON payload 写入 PayloadStore。返回 payload_id（`pl_<uuid>`）。
    pub fn put_json(
        &self,
        kind: PayloadKind,
        content: &serde_json::Value,
    ) -> Result<String, PayloadStoreError> {
        let payload_id = format!("pl_{}", Uuid::new_v4());
        let json_text = serde_json::to_string(content)?;
        let byte_size = json_text.len();
        let now = Utc::now();
        self.db.with(|c| {
            c.execute(
                "INSERT INTO agent_payloads
                  (payload_id, kind, content_json, byte_size, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    payload_id,
                    kind.as_str(),
                    json_text,
                    byte_size as i64,
                    now.to_rfc3339(),
                ],
            )?;
            Ok::<_, PayloadStoreError>(())
        })?;
        Ok(payload_id)
    }

    /// 把 image bytes 写入 PayloadStore。
    pub fn put_image(
        &self,
        bytes: Vec<u8>,
        content_type: impl Into<String>,
    ) -> Result<String, PayloadStoreError> {
        let payload_id = format!("pl_{}", Uuid::new_v4());
        let ct = content_type.into();
        let byte_size = bytes.len();
        let now = Utc::now();
        self.db.with(|c| {
            c.execute(
                "INSERT INTO agent_payloads
                  (payload_id, kind, content_bytes, content_type, byte_size, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    payload_id,
                    PayloadKind::Image.as_str(),
                    bytes,
                    ct,
                    byte_size as i64,
                    now.to_rfc3339(),
                ],
            )?;
            Ok::<_, PayloadStoreError>(())
        })?;
        Ok(payload_id)
    }

    /// 读取 payload。
    pub fn get(&self, payload_id: &str) -> Result<Option<PayloadStoreEntry>, PayloadStoreError> {
        let row = self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT payload_id, kind, content_json, content_bytes, content_type,
                        byte_size, created_at
                 FROM agent_payloads WHERE payload_id = ?1",
            )?;
            stmt.query_row(params![payload_id], |r| {
                let payload_id: String = r.get(0)?;
                let kind_s: String = r.get(1)?;
                let content_json_s: Option<String> = r.get(2)?;
                let content_bytes: Option<Vec<u8>> = r.get(3)?;
                let content_type: Option<String> = r.get(4)?;
                let byte_size_i: i64 = r.get(5)?;
                let created_at_s: String = r.get(6)?;
                Ok((
                    payload_id,
                    kind_s,
                    content_json_s,
                    content_bytes,
                    content_type,
                    byte_size_i,
                    created_at_s,
                ))
            })
            .optional()
            .map_err(PayloadStoreError::from)
        })?;
        let Some((
            payload_id,
            kind_s,
            content_json_s,
            content_bytes,
            content_type,
            byte_size_i,
            created_at_s,
        )) = row
        else {
            return Ok(None);
        };
        let kind =
            PayloadKind::from_str(&kind_s).ok_or(PayloadStoreError::InvalidKind(kind_s.clone()))?;
        let content_json = match content_json_s {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        };
        let created_at: DateTime<Utc> = DateTime::parse_from_rfc3339(&created_at_s)
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?
            .with_timezone(&Utc);
        Ok(Some(PayloadStoreEntry {
            payload_id,
            kind,
            content_json,
            content_bytes,
            content_type,
            byte_size: byte_size_i as usize,
            created_at,
        }))
    }

    /// 把 `payload://pl_xxx` URI 转成 `pl_xxx` payload id。
    pub fn parse_uri(uri: &str) -> Option<&str> {
        uri.strip_prefix("payload://")
    }

    /// 构造 `payload://pl_xxx` URI。
    pub fn make_uri(payload_id: &str) -> String {
        format!("payload://{}", payload_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::db::run_migrations;

    fn fresh_store() -> PayloadStore {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        PayloadStore::new(db)
    }

    #[test]
    fn put_get_json_roundtrip() {
        let store = fresh_store();
        let v = serde_json::json!({"k": [1,2,3], "name": "x"});
        let id = store.put_json(PayloadKind::SkillOutput, &v).unwrap();
        assert!(id.starts_with("pl_"));
        let got = store.get(&id).unwrap().unwrap();
        assert_eq!(got.kind, PayloadKind::SkillOutput);
        assert_eq!(got.content_json.unwrap(), v);
        assert!(got.byte_size > 0);
    }

    #[test]
    fn put_get_image_bytes_roundtrip() {
        let store = fresh_store();
        let bytes = vec![0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        let id = store
            .put_image(bytes.clone(), "image/png".to_string())
            .unwrap();
        let got = store.get(&id).unwrap().unwrap();
        assert_eq!(got.kind, PayloadKind::Image);
        assert_eq!(got.content_bytes.as_deref(), Some(bytes.as_slice()));
        assert_eq!(got.content_type.as_deref(), Some("image/png"));
    }

    #[test]
    fn inline_threshold_is_8kb() {
        // Spec §2: > 8KB 走 ref。这里验证常量值。
        assert_eq!(PAYLOAD_INLINE_LIMIT_BYTES, 8 * 1024);
    }

    #[test]
    fn payload_uri_round_trip() {
        let id = "pl_abc";
        let uri = PayloadStore::make_uri(id);
        assert_eq!(uri, "payload://pl_abc");
        assert_eq!(PayloadStore::parse_uri(&uri), Some("pl_abc"));
        assert_eq!(PayloadStore::parse_uri("file:///x"), None);
    }

    #[test]
    fn get_missing_returns_none() {
        let store = fresh_store();
        assert!(store.get("pl_missing").unwrap().is_none());
    }
}
