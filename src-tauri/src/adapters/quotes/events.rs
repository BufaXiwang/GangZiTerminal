//! Quotes 对外事件 envelope helper。
//!
//! Spec: docs/design/quotes-module.md §5；shared-types.md §6

use crate::domain::quotes::{MarketQuotesRefreshProgressPayload, MarketQuotesRefreshedPayload};
use crate::domain::shared::AppEventEnvelope;
use chrono::Utc;
use uuid::Uuid;

pub use crate::domain::quotes::{
    MARKET_QUOTES_REFRESHED_EVENT, MARKET_QUOTES_REFRESH_PROGRESS_EVENT,
};

pub fn wrap_market_quotes_refreshed(
    payload: MarketQuotesRefreshedPayload,
    correlation_id: Option<String>,
) -> AppEventEnvelope<MarketQuotesRefreshedPayload> {
    AppEventEnvelope {
        event_id: Uuid::new_v4().to_string(),
        event_type: MARKET_QUOTES_REFRESHED_EVENT.to_string(),
        occurred_at: Utc::now(),
        correlation_id,
        causation_id: None,
        payload,
    }
}

pub fn wrap_market_quotes_refresh_progress(
    payload: MarketQuotesRefreshProgressPayload,
    correlation_id: Option<String>,
) -> AppEventEnvelope<MarketQuotesRefreshProgressPayload> {
    AppEventEnvelope {
        event_id: Uuid::new_v4().to_string(),
        event_type: MARKET_QUOTES_REFRESH_PROGRESS_EVENT.to_string(),
        occurred_at: Utc::now(),
        correlation_id,
        causation_id: None,
        payload,
    }
}

#[cfg(test)]
mod tests {
    //! 盲区③ — 事件 envelope helper（hermetic，纯函数）。
    //!
    //! Spec: docs/design/quotes-module.md §5；shared-types.md §6。
    //! 断言：event_type 锁定常量 / payload 字段无损透传 / id 与时间戳由 helper 生成 /
    //! correlationId 透传 + causationId 恒为 None / 序列化形如 spec（camelCase + `type` 键）。
    use super::*;
    use crate::domain::quotes::{RefreshPurpose, RefreshScopeKind};
    use crate::domain::shared::{TradeDate, TsCode};

    fn sample_refreshed() -> MarketQuotesRefreshedPayload {
        MarketQuotesRefreshedPayload {
            scope: RefreshScopeKind::Universe,
            purpose: RefreshPurpose::Close,
            trade_date: Some(TradeDate::parse("20260529").unwrap()),
            affected_ts_codes: Some(vec![TsCode::parse("600519.SH").unwrap()]),
            total: 10,
            success: 9,
            failed_batches: 1,
            captured_at: Utc::now(),
        }
    }

    fn sample_progress() -> MarketQuotesRefreshProgressPayload {
        MarketQuotesRefreshProgressPayload {
            scope: RefreshScopeKind::Subscribed,
            purpose: RefreshPurpose::Intraday,
            trade_date: Some(TradeDate::parse("20260529").unwrap()),
            completed: 5,
            success: 4,
            total: 8,
            affected_ts_codes: vec![TsCode::parse("000001.SZ").unwrap()],
            captured_at: Utc::now(),
        }
    }

    #[test]
    fn refreshed_envelope_uses_locked_event_type_and_preserves_payload() {
        let p = sample_refreshed();
        let env = wrap_market_quotes_refreshed(p.clone(), Some("corr-1".into()));
        // event_type 必须锁定常量（消费者按这个 key 做幂等）。
        assert_eq!(env.event_type, MARKET_QUOTES_REFRESHED_EVENT);
        assert_eq!(env.event_type, "market-quotes-refreshed");
        // payload 字段无损透传。
        assert_eq!(env.payload.total, 10);
        assert_eq!(env.payload.success, 9);
        assert_eq!(env.payload.failed_batches, 1);
        assert_eq!(env.payload.scope, RefreshScopeKind::Universe);
        assert_eq!(env.payload.purpose, RefreshPurpose::Close);
        assert_eq!(
            env.payload.affected_ts_codes.as_ref().unwrap()[0].as_str(),
            "600519.SH"
        );
        // helper 生成 event_id（非空 UUID）+ correlation 透传 + causation 恒 None。
        assert!(!env.event_id.is_empty());
        assert!(uuid::Uuid::parse_str(&env.event_id).is_ok());
        assert_eq!(env.correlation_id.as_deref(), Some("corr-1"));
        assert!(env.causation_id.is_none());
    }

    #[test]
    fn refreshed_envelope_none_correlation_stays_none() {
        let env = wrap_market_quotes_refreshed(sample_refreshed(), None);
        assert!(env.correlation_id.is_none());
        assert!(env.causation_id.is_none());
    }

    #[test]
    fn refreshed_envelope_each_call_has_unique_event_id() {
        let a = wrap_market_quotes_refreshed(sample_refreshed(), None);
        let b = wrap_market_quotes_refreshed(sample_refreshed(), None);
        assert_ne!(a.event_id, b.event_id, "event_id 应每次唯一");
    }

    #[test]
    fn progress_envelope_uses_locked_event_type_and_preserves_payload() {
        let p = sample_progress();
        let env = wrap_market_quotes_refresh_progress(p.clone(), Some("corr-2".into()));
        assert_eq!(env.event_type, MARKET_QUOTES_REFRESH_PROGRESS_EVENT);
        assert_eq!(env.event_type, "market-quotes-refresh-progress");
        assert_eq!(env.payload.completed, 5);
        assert_eq!(env.payload.success, 4);
        assert_eq!(env.payload.total, 8);
        assert_eq!(env.payload.scope, RefreshScopeKind::Subscribed);
        assert_eq!(env.payload.purpose, RefreshPurpose::Intraday);
        assert_eq!(env.payload.affected_ts_codes[0].as_str(), "000001.SZ");
        assert_eq!(env.correlation_id.as_deref(), Some("corr-2"));
        assert!(env.causation_id.is_none());
    }

    #[test]
    fn refreshed_event_names_are_distinct() {
        // 两个 channel name 不能相撞，否则前端 listen 会串台。
        assert_ne!(
            MARKET_QUOTES_REFRESHED_EVENT,
            MARKET_QUOTES_REFRESH_PROGRESS_EVENT
        );
    }

    #[test]
    fn refreshed_envelope_serializes_camel_case_with_type_key() {
        // spec shared-types §6：envelope 用 camelCase + `type` 作为 event_type 键。
        let env = wrap_market_quotes_refreshed(sample_refreshed(), Some("c".into()));
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["type"], "market-quotes-refreshed");
        assert!(v["eventId"].is_string());
        assert!(v["occurredAt"].is_string());
        assert_eq!(v["correlationId"], "c");
        // causationId 为 None → skip_serializing_if 省略键。
        assert!(v.get("causationId").is_none());
        // payload 内层也 camelCase。
        assert_eq!(v["payload"]["failedBatches"], 1);
        assert_eq!(v["payload"]["affectedTsCodes"][0], "600519.SH");
    }

    #[test]
    fn progress_envelope_serializes_camel_case() {
        let env = wrap_market_quotes_refresh_progress(sample_progress(), None);
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["type"], "market-quotes-refresh-progress");
        assert_eq!(v["payload"]["completed"], 5);
        assert_eq!(v["payload"]["affectedTsCodes"][0], "000001.SZ");
        // correlationId 省略。
        assert!(v.get("correlationId").is_none());
    }
}
