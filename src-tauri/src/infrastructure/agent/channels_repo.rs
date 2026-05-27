//! ProviderChannelsRepo — `agent_provider_channels` CRUD。
//!
//! Spec: docs/design/agent-infra-module.md §2 `ProviderChannel`，§5 ProviderChannels Repo API

use crate::domain::agent::{ProviderChannel, WireFormat};
use crate::infrastructure::db::AppDb;
use chrono::Utc;
use rusqlite::{params, OptionalExtension, Row};

#[derive(Clone)]
pub struct ProviderChannelsRepo {
    db: AppDb,
}

#[derive(Debug, thiserror::Error)]
pub enum ChannelsRepoError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid wire format '{0}'")]
    InvalidWireFormat(String),
}

impl ProviderChannelsRepo {
    pub fn new(db: AppDb) -> Self {
        Self { db }
    }

    /// 插入新 channel 配置。
    pub fn add(&self, channel: &ProviderChannel) -> Result<(), ChannelsRepoError> {
        let wf = wire_format_str(channel.wire_format);
        let now = Utc::now().to_rfc3339();
        self.db.with(|c| {
            c.execute(
                "INSERT INTO agent_provider_channels (
                    channel_id, provider, wire_format, base_url, model,
                    stream, supports_vision, supports_thinking,
                    max_output_tokens, context_window_tokens,
                    created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    channel.channel_id,
                    channel.provider,
                    wf,
                    channel.base_url,
                    channel.model,
                    channel.stream as i32,
                    channel.supports_vision as i32,
                    channel.supports_thinking as i32,
                    channel.max_output_tokens.map(|v| v as i64),
                    channel.context_window_tokens.map(|v| v as i64),
                    now,
                    now,
                ],
            )?;
            Ok::<_, ChannelsRepoError>(())
        })
    }

    /// 更新已有 channel。
    pub fn update(&self, channel: &ProviderChannel) -> Result<(), ChannelsRepoError> {
        let wf = wire_format_str(channel.wire_format);
        let now = Utc::now().to_rfc3339();
        self.db.with(|c| {
            c.execute(
                "UPDATE agent_provider_channels SET
                    provider = ?2,
                    wire_format = ?3,
                    base_url = ?4,
                    model = ?5,
                    stream = ?6,
                    supports_vision = ?7,
                    supports_thinking = ?8,
                    max_output_tokens = ?9,
                    context_window_tokens = ?10,
                    updated_at = ?11
                 WHERE channel_id = ?1",
                params![
                    channel.channel_id,
                    channel.provider,
                    wf,
                    channel.base_url,
                    channel.model,
                    channel.stream as i32,
                    channel.supports_vision as i32,
                    channel.supports_thinking as i32,
                    channel.max_output_tokens.map(|v| v as i64),
                    channel.context_window_tokens.map(|v| v as i64),
                    now,
                ],
            )?;
            Ok::<_, ChannelsRepoError>(())
        })
    }

    pub fn remove(&self, channel_id: &str) -> Result<(), ChannelsRepoError> {
        self.db.with(|c| {
            c.execute(
                "DELETE FROM agent_provider_channels WHERE channel_id = ?1",
                params![channel_id],
            )?;
            Ok::<_, ChannelsRepoError>(())
        })
    }

    pub fn get(&self, channel_id: &str) -> Result<Option<ProviderChannel>, ChannelsRepoError> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT channel_id, provider, wire_format, base_url, model,
                        stream, supports_vision, supports_thinking,
                        max_output_tokens, context_window_tokens
                 FROM agent_provider_channels WHERE channel_id = ?1",
            )?;
            let row = stmt
                .query_row(params![channel_id], row_to_channel)
                .optional()?;
            Ok(row)
        })
    }

    pub fn list(&self) -> Result<Vec<ProviderChannel>, ChannelsRepoError> {
        self.db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT channel_id, provider, wire_format, base_url, model,
                        stream, supports_vision, supports_thinking,
                        max_output_tokens, context_window_tokens
                 FROM agent_provider_channels ORDER BY channel_id ASC",
            )?;
            let rows: Vec<ProviderChannel> = stmt
                .query_map([], row_to_channel)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }
}

fn wire_format_str(wf: WireFormat) -> &'static str {
    match wf {
        WireFormat::Messages => "messages",
        WireFormat::Responses => "responses",
        WireFormat::ChatCompletions => "chat_completions",
    }
}

fn wire_format_parse(s: &str) -> Result<WireFormat, ChannelsRepoError> {
    match s {
        "messages" => Ok(WireFormat::Messages),
        "responses" => Ok(WireFormat::Responses),
        "chat_completions" => Ok(WireFormat::ChatCompletions),
        other => Err(ChannelsRepoError::InvalidWireFormat(other.into())),
    }
}

fn row_to_channel(row: &Row) -> rusqlite::Result<ProviderChannel> {
    let channel_id: String = row.get(0)?;
    let provider: String = row.get(1)?;
    let wire_format_s: String = row.get(2)?;
    let base_url: Option<String> = row.get(3)?;
    let model: String = row.get(4)?;
    let stream_i: i64 = row.get(5)?;
    let supports_vision_i: i64 = row.get(6)?;
    let supports_thinking_i: i64 = row.get(7)?;
    let max_output_tokens_i: Option<i64> = row.get(8)?;
    let context_window_tokens_i: Option<i64> = row.get(9)?;

    let wire_format = wire_format_parse(&wire_format_s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())),
        )
    })?;

    Ok(ProviderChannel {
        channel_id,
        provider,
        wire_format,
        base_url,
        model,
        stream: stream_i != 0,
        supports_vision: supports_vision_i != 0,
        supports_thinking: supports_thinking_i != 0,
        max_output_tokens: max_output_tokens_i.map(|v| v as u32),
        context_window_tokens: context_window_tokens_i.map(|v| v as u32),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::agent::migrations::migrations as agent_migrations;
    use crate::infrastructure::db::run_migrations;

    fn fresh_repo() -> ProviderChannelsRepo {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|c| run_migrations(c, agent_migrations()).unwrap());
        ProviderChannelsRepo::new(db)
    }

    fn sample(id: &str) -> ProviderChannel {
        ProviderChannel {
            channel_id: id.into(),
            provider: "anthropic".into(),
            wire_format: WireFormat::Messages,
            base_url: Some("https://api.anthropic.com".into()),
            model: "claude-sonnet-4-5".into(),
            stream: true,
            supports_vision: true,
            supports_thinking: true,
            max_output_tokens: Some(8192),
            context_window_tokens: Some(200_000),
        }
    }

    #[test]
    fn add_get_round_trip() {
        let r = fresh_repo();
        let c = sample("ch1");
        r.add(&c).unwrap();
        let got = r.get("ch1").unwrap().unwrap();
        assert_eq!(got, c);
    }

    #[test]
    fn update_changes_model() {
        let r = fresh_repo();
        let mut c = sample("ch1");
        r.add(&c).unwrap();
        c.model = "claude-opus-4-7".into();
        r.update(&c).unwrap();
        let got = r.get("ch1").unwrap().unwrap();
        assert_eq!(got.model, "claude-opus-4-7");
    }

    #[test]
    fn remove_deletes() {
        let r = fresh_repo();
        let c = sample("ch1");
        r.add(&c).unwrap();
        r.remove("ch1").unwrap();
        assert!(r.get("ch1").unwrap().is_none());
    }

    #[test]
    fn list_returns_sorted() {
        let r = fresh_repo();
        let mut a = sample("b");
        a.channel_id = "b".into();
        let mut b = sample("a");
        b.channel_id = "a".into();
        r.add(&a).unwrap();
        r.add(&b).unwrap();
        let all = r.list().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].channel_id, "a");
        assert_eq!(all[1].channel_id, "b");
    }
}
