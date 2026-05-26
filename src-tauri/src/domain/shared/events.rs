//! 应用事件 envelope + JSON payload。
//!
//! Spec: docs/design/shared-types.md §6
//!
//! 跨模块事件 payload 由各 BC 在自己的 domain 模块中定义（NewsRefreshedPayload 等），
//! 引用本模块的 `AppEventEnvelope<T>` 和 `JsonValue`。

use super::types::OccurredAt;
use serde::{Deserialize, Serialize};
use specta::Type;

/// 通用 JSON value，对应 spec §6 中的 `JsonValue`。
pub type JsonValue = serde_json::Value;

/// 跨模块事件 envelope。
///
/// Spec: shared-types.md §6
/// - `eventId` 和 `occurredAt` 由事件发布 helper 在 emit 时生成。
/// - `correlationId` / `causationId` 由调用方按 spec 规则传入。
/// - 生产者只表达事实，不指定消费者；消费者必须以模块规定的 event key 做幂等。
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AppEventEnvelope<T> {
    pub event_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub occurred_at: OccurredAt,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<String>,
    pub payload: T,
}
