use crate::models::DatabaseInfo;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager};

const SCHEMA_VERSION: i64 = 1;

#[tauri::command]
pub fn initialize_database(app: AppHandle) -> Result<DatabaseInfo, String> {
    let path = database_path(&app)?;
    let connection = open_database(&app)?;
    migrate(&connection)?;
    Ok(DatabaseInfo {
        path: path.to_string_lossy().to_string(),
        schema_version: SCHEMA_VERSION,
    })
}

#[tauri::command]
pub fn save_app_state(app: AppHandle, key: String, value: Value) -> Result<(), String> {
    save_app_state_value(&app, &key, &value)
}

pub fn save_app_state_value(app: &AppHandle, key: &str, value: &Value) -> Result<(), String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let now = now();
    connection
        .execute(
            "insert into app_state (key, value_json, updated_at)
             values (?1, ?2, ?3)
             on conflict(key) do update set value_json = excluded.value_json, updated_at = excluded.updated_at",
            params![key, value.to_string(), now],
        )
        .map_err(|err| format!("保存本地状态失败：{err}"))?;
    Ok(())
}

#[tauri::command]
pub fn load_app_state(app: AppHandle, key: String) -> Result<Option<Value>, String> {
    load_app_state_value(&app, &key)
}

pub fn load_app_state_value(app: &AppHandle, key: &str) -> Result<Option<Value>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let raw = connection
        .query_row(
            "select value_json from app_state where key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|err| format!("读取本地状态失败：{err}"))?;

    raw.map(|text| {
        serde_json::from_str(&text).map_err(|err| format!("本地状态 JSON 解析失败：{err}"))
    })
    .transpose()
}

/// 启动恢复——把卡在 `processing` 的资讯归还为 `pending`，避免一次崩溃永远漏分析。
///
/// briefing 流水线 claim 一批资讯后，如果中途崩溃（provider 长流卡死、应用被杀、网络断开
/// 后 panic 等），那些资讯会一直停在 `processing` 状态，再也不会被新一轮 claim 看到。
/// 这个函数在 Tauri setup 阶段调一次即可。
pub fn recover_stale_processing_news(app: &AppHandle) -> Result<usize, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let now = now();
    let count = connection
        .execute(
            "update news_items set analysis_status = 'pending', updated_at = ?1 where analysis_status = 'processing'",
            params![now],
        )
        .map_err(|err| format!("恢复资讯状态失败：{err}"))?;
    Ok(count)
}

#[tauri::command]
pub fn list_news_items(app: AppHandle, limit: Option<i64>) -> Result<Vec<Value>, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let mut statement = connection
        .prepare(
            "select payload_json, analysis_status
             from news_items
             order by coalesce(published, updated_at) desc
             limit ?1",
        )
        .map_err(|err| format!("读取资讯缓存失败：{err}"))?;
    let items = statement
        .query_map(params![limit.unwrap_or(300).clamp(1, 1000)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| format!("读取资讯缓存失败：{err}"))?
        .map(|raw| {
            raw.map_err(|err| format!("读取资讯缓存失败：{err}"))
                .and_then(|(payload, status)| {
                    let mut value: Value = serde_json::from_str(&payload)
                        .map_err(|err| format!("资讯 JSON 解析失败：{err}"))?;
                    if let Value::Object(map) = &mut value {
                        map.insert("analysisStatus".to_string(), Value::String(status));
                    }
                    Ok(value)
                })
        })
        .collect();
    items
}

pub fn save_news_items(app: AppHandle, items: Vec<Value>) -> Result<usize, String> {
    let mut connection = open_database(&app)?;
    migrate(&connection)?;
    let tx = connection
        .transaction()
        .map_err(|err| format!("保存资讯缓存失败：{err}"))?;
    let now = now();
    let mut saved = 0usize;

    for item in items {
        let id = required_json_string(&item, "/id", "资讯缺少 id")?;
        let source = required_json_string(&item, "/source", "资讯缺少 source")?;
        let published = json_string(&item, "/published");
        // 冲突时不覆盖 analysis_status，保留旧的分析状态
        tx.execute(
            "insert into news_items (id, source, published, payload_json, created_at, updated_at)
             values (?1, ?2, ?3, ?4, ?5, ?5)
             on conflict(id) do update set
                source = excluded.source,
                published = excluded.published,
                payload_json = excluded.payload_json,
                updated_at = excluded.updated_at",
            params![id, source, published, item.to_string(), now],
        )
        .map_err(|err| format!("写入资讯缓存失败：{err}"))?;
        saved += 1;
    }

    tx.commit()
        .map_err(|err| format!("提交资讯缓存失败：{err}"))?;
    Ok(saved)
}

pub fn claim_pending_news_batch(app: AppHandle, limit: i64) -> Result<Vec<Value>, String> {
    let mut connection = open_database(&app)?;
    migrate(&connection)?;
    let cap = limit.clamp(1, 200);
    let now = now();
    let tx = connection
        .transaction()
        .map_err(|err| format!("领取资讯失败：{err}"))?;
    let rows: Vec<(String, String)> = {
        let mut stmt = tx
            .prepare(
                "select id, payload_json from news_items
                 where analysis_status = 'pending'
                 order by coalesce(published, updated_at) desc
                 limit ?1",
            )
            .map_err(|err| format!("领取资讯失败：{err}"))?;
        let collected = stmt
            .query_map(params![cap], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|err| format!("领取资讯失败：{err}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("领取资讯失败：{err}"))?;
        collected
    };
    let mut items = Vec::with_capacity(rows.len());
    for (id, payload) in rows {
        tx.execute(
            "update news_items set analysis_status = 'processing', updated_at = ?2 where id = ?1",
            params![id, now],
        )
        .map_err(|err| format!("领取资讯失败：{err}"))?;
        let mut value: Value =
            serde_json::from_str(&payload).map_err(|err| format!("资讯 JSON 解析失败：{err}"))?;
        if let Value::Object(map) = &mut value {
            map.insert(
                "analysisStatus".to_string(),
                Value::String("processing".to_string()),
            );
        }
        items.push(value);
    }
    tx.commit()
        .map_err(|err| format!("提交资讯领取失败：{err}"))?;
    Ok(items)
}

// `mark_news_consumed` 之前是 briefing 5e 用的——单事务化后由 commit_briefing
// 内部直接执行，不再需要独立公共 fn。如果未来其他流水线需要批量标记 consumed，
// 重新引入即可（实现也就 5 行）。

pub fn revert_news_to_pending(app: AppHandle, ids: Vec<String>) -> Result<usize, String> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut connection = open_database(&app)?;
    migrate(&connection)?;
    let now = now();
    let tx = connection
        .transaction()
        .map_err(|err| format!("回滚资讯状态失败：{err}"))?;
    let mut updated = 0usize;
    for id in &ids {
        let n = tx
            .execute(
                "update news_items set analysis_status = 'pending', updated_at = ?2
                 where id = ?1 and analysis_status = 'processing'",
                params![id, now],
            )
            .map_err(|err| format!("回滚资讯状态失败：{err}"))?;
        updated += n;
    }
    tx.commit()
        .map_err(|err| format!("提交资讯回滚失败：{err}"))?;
    Ok(updated)
}

#[tauri::command]
pub fn get_news_items_by_ids(app: AppHandle, ids: Vec<String>) -> Result<Vec<Value>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let placeholders = (0..ids.len()).map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "select payload_json, analysis_status from news_items where id in ({}) order by coalesce(published, updated_at) desc",
        placeholders
    );
    let mut stmt = connection
        .prepare(&sql)
        .map_err(|err| format!("查询资讯失败：{err}"))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| format!("查询资讯失败：{err}"))?
        .map(|raw| {
            raw.map_err(|err| format!("查询资讯失败：{err}"))
                .and_then(|(payload, status)| {
                    let mut value: Value = serde_json::from_str(&payload)
                        .map_err(|err| format!("资讯 JSON 解析失败：{err}"))?;
                    if let Value::Object(map) = &mut value {
                        map.insert("analysisStatus".to_string(), Value::String(status));
                    }
                    Ok(value)
                })
        })
        .collect::<Result<Vec<Value>, String>>()?;
    Ok(rows)
}

#[tauri::command]
pub fn count_pending_news(app: AppHandle) -> Result<i64, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let count: i64 = connection
        .query_row(
            "select count(*) from news_items where analysis_status = 'pending'",
            [],
            |row| row.get(0),
        )
        .map_err(|err| format!("统计待消化资讯失败：{err}"))?;
    Ok(count)
}

pub fn load_article_content(app: AppHandle, url: String) -> Result<Option<Value>, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let raw = connection
        .query_row(
            "select payload_json from article_contents where url = ?1",
            params![url],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|err| format!("读取正文缓存失败：{err}"))?;

    raw.map(|text| {
        serde_json::from_str(&text).map_err(|err| format!("正文缓存 JSON 解析失败：{err}"))
    })
    .transpose()
}

pub fn save_article_content(
    app: AppHandle,
    item_id: Option<String>,
    article: Value,
) -> Result<(), String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let url = required_json_string(&article, "/url", "正文缓存缺少 url")?;
    if url.trim().is_empty() {
        return Ok(());
    }
    let fetched_at = json_string(&article, "/fetchedAt").unwrap_or_else(|| now());
    connection
        .execute(
            "insert into article_contents (url, item_id, payload_json, fetched_at)
             values (?1, ?2, ?3, ?4)
             on conflict(url) do update set
                item_id = excluded.item_id,
                payload_json = excluded.payload_json,
                fetched_at = excluded.fetched_at",
            params![url, item_id, article.to_string(), fetched_at],
        )
        .map_err(|err| format!("写入正文缓存失败：{err}"))?;
    Ok(())
}

#[tauri::command]
pub fn list_analysis_records(app: AppHandle, limit: Option<i64>) -> Result<Vec<Value>, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    list_json_payloads(
        &connection,
        "select payload_json from analysis_records order by created_at desc limit ?1",
        limit.unwrap_or(300).clamp(1, 1000),
        "读取分析记录失败",
    )
}

// `replace_analysis_records` 之前是 briefing/review 各自调用——单事务化后由
// commit_briefing / commit_review 内部直接执行，不再需要独立公共 fn。

#[tauri::command]
pub fn list_simulated_positions(app: AppHandle) -> Result<Vec<Value>, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    list_json_payloads(
        &connection,
        "select payload_json from simulated_positions order by created_at desc limit ?1",
        1000,
        "读取模拟持仓失败",
    )
}

/// Account 写事务：可选 append 一条 position_event，并整列替换 positions。
///
/// `AccountService` 的写路径必须保持 event/state 原子性：不能出现 event 已写但
/// positions 没更新，或 positions 更新但 event 缺失。reset 场景传 `clear_events=true`
/// 清空事件链并替换为空 positions，让账户从干净初始状态重练。
pub fn commit_account_positions(
    app: AppHandle,
    event: Option<Value>,
    positions: Vec<Value>,
    clear_events: bool,
) -> Result<(), String> {
    let mut connection = open_database(&app)?;
    migrate(&connection)?;
    let tx = connection
        .transaction()
        .map_err(|err| format!("提交账户事务失败：{err}"))?;

    if clear_events {
        tx.execute("delete from position_events", [])
            .map_err(|err| format!("清空持仓事件失败：{err}"))?;
    }

    if let Some(event) = event {
        insert_position_event_tx(&tx, &event)?;
    }

    replace_simulated_positions_tx(&tx, positions)?;

    tx.commit()
        .map_err(|err| format!("提交账户事务失败：{err}"))?;
    Ok(())
}

fn replace_simulated_positions_tx(
    tx: &Transaction<'_>,
    positions: Vec<Value>,
) -> Result<(), String> {
    tx.execute("delete from simulated_positions", [])
        .map_err(|err| format!("清理模拟持仓失败：{err}"))?;

    let now = now();
    for position in positions {
        let id = required_json_string(&position, "/id", "模拟持仓缺少 id")?;
        let code = required_json_string(&position, "/code", "模拟持仓缺少 code")?;
        let source_analysis_id = required_json_string(
            &position,
            "/sourceAnalysisId",
            "模拟持仓缺少 sourceAnalysisId",
        )?;
        let status = required_json_string(&position, "/status", "模拟持仓缺少 status")?;
        let created_at = json_string(&position, "/entryAt").unwrap_or_else(|| now.clone());
        let updated_at = json_string(&position, "/exitAt").unwrap_or_else(|| now.clone());
        tx.execute(
            "insert into simulated_positions
                (id, code, source_analysis_id, status, payload_json, created_at, updated_at)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                code,
                source_analysis_id,
                status,
                position.to_string(),
                created_at,
                updated_at
            ],
        )
        .map_err(|err| format!("写入模拟持仓失败：{err}"))?;
    }
    Ok(())
}

fn insert_position_event_tx(tx: &Transaction<'_>, event: &Value) -> Result<(), String> {
    let id = required_json_string(event, "/id", "持仓事件缺少 id")?;
    let position_id = required_json_string(event, "/positionId", "持仓事件缺少 positionId")?;
    let event_kind = required_json_string(event, "/eventKind", "持仓事件缺少 eventKind")?;
    let occurred_at = json_string(event, "/occurredAt").unwrap_or_else(now);
    let source_kind = json_string(event, "/sourceKind");
    let source_ref = json_string(event, "/sourceRef");
    let agent_note = json_string(event, "/agentNoteMd");

    tx.execute(
        "insert into position_events
            (id, position_id, event_kind, occurred_at, source_kind, source_ref,
             payload_json, agent_note_md, created_at)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id,
            position_id,
            event_kind,
            occurred_at,
            source_kind,
            source_ref,
            event.to_string(),
            agent_note,
            now()
        ],
    )
    .map_err(|err| format!("写入持仓事件失败：{err}"))?;
    Ok(())
}

/// 一次 briefing 写盘的全部 payload——所有字段在单事务内 commit。
/// 投资学习的核心约束：用户看到的简报、新写的 trade hypothesis、新开的模拟仓
/// 必须**同时**存在或**同时**不存在，避免"agent 推荐了 5 个标的但只开了 3 个仓"
/// 这类显著学习数据污染。
#[derive(Debug, Default)]
pub struct BriefingCommit {
    /// 完整 records 列表（覆盖式 replace）。空 Vec 表示无变更，跳过。
    pub records: Option<Vec<Value>>,
    /// 完整 positions 列表（覆盖式 replace）。空 Vec 表示无变更，跳过。
    pub positions: Option<Vec<Value>>,
    /// position_events append-only。每条独立 insert。
    pub position_events: Vec<Value>,
    /// 一次 briefing 通常 1 条主消息 + 0/1 条 highlight。
    pub chat_messages: Vec<Value>,
    /// 标记 consumed 的 news id。
    pub news_consumed_ids: Vec<String>,
    /// 回滚 pending 的 news id（agent 没真正覆盖到的 claim）。
    pub news_revert_ids: Vec<String>,
    /// (key, value)——一般是 investor_memory 和 last_briefing_at_ms。
    pub app_state_writes: Vec<(String, Value)>,
}

/// 单事务提交一次 briefing 的全部产物。返回每条 chat_message 的最终落盘形态
/// （带 server-side 校验后的字段），供调用方在 commit 之后做 `chat-message-appended`
/// emit。事件不在事务内 emit——前端订阅必须看到的是已提交的状态。
///
/// 对应 D1 走查的 torn-state 修复：5a-5e 任何一步失败时整个事务回滚，
/// 不会留下 records 已写入但 chat_message 缺失的"幽灵记录"。
pub fn commit_briefing(app: AppHandle, payload: BriefingCommit) -> Result<Vec<Value>, String> {
    let mut connection = open_database(&app)?;
    migrate(&connection)?;
    let tx = connection
        .transaction()
        .map_err(|err| format!("开启 briefing 事务失败：{err}"))?;

    // 1. app_state writes（memory + last_briefing_at）
    for (key, value) in &payload.app_state_writes {
        let now = now();
        tx.execute(
            "insert into app_state (key, value_json, updated_at)
             values (?1, ?2, ?3)
             on conflict(key) do update set value_json = excluded.value_json, updated_at = excluded.updated_at",
            params![key, value.to_string(), now],
        )
        .map_err(|err| format!("写 app_state[{key}] 失败：{err}"))?;
    }

    // 2. analysis_records: 完整覆盖
    if let Some(records) = &payload.records {
        tx.execute("delete from analysis_records", [])
            .map_err(|err| format!("清理分析记录失败：{err}"))?;
        for record in records {
            let id = required_json_string(record, "/id", "分析记录缺少 id")?;
            let item_id = required_json_string(record, "/item/id", "分析记录缺少 item.id")?;
            let created_at = json_string(record, "/createdAt")
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
            tx.execute(
                "insert into analysis_records (id, item_id, payload_json, created_at)
                 values (?1, ?2, ?3, ?4)",
                params![id, item_id, record.to_string(), created_at],
            )
            .map_err(|err| format!("写入分析记录失败：{err}"))?;
        }
    }

    // 3. simulated_positions: 完整覆盖
    if let Some(positions) = &payload.positions {
        let now = now();
        tx.execute("delete from simulated_positions", [])
            .map_err(|err| format!("清理模拟持仓失败：{err}"))?;
        for position in positions {
            let id = required_json_string(position, "/id", "模拟持仓缺少 id")?;
            let code = required_json_string(position, "/code", "模拟持仓缺少 code")?;
            let source_analysis_id = required_json_string(
                position,
                "/sourceAnalysisId",
                "模拟持仓缺少 sourceAnalysisId",
            )?;
            let status = required_json_string(position, "/status", "模拟持仓缺少 status")?;
            let created_at = json_string(position, "/entryAt").unwrap_or_else(|| now.clone());
            let updated_at = json_string(position, "/exitAt").unwrap_or_else(|| now.clone());
            tx.execute(
                "insert into simulated_positions
                    (id, code, source_analysis_id, status, payload_json, created_at, updated_at)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id,
                    code,
                    source_analysis_id,
                    status,
                    position.to_string(),
                    created_at,
                    updated_at
                ],
            )
            .map_err(|err| format!("写入模拟持仓失败：{err}"))?;
        }
    }

    // 4. position_events: append-only
    for event in &payload.position_events {
        let id = required_json_string(event, "/id", "持仓事件缺少 id")?;
        let position_id = required_json_string(event, "/positionId", "持仓事件缺少 positionId")?;
        let event_kind = required_json_string(event, "/eventKind", "持仓事件缺少 eventKind")?;
        let occurred_at = json_string(event, "/occurredAt").unwrap_or_else(now);
        let source_kind = json_string(event, "/sourceKind");
        let source_ref = json_string(event, "/sourceRef");
        let agent_note = json_string(event, "/agentNoteMd");
        tx.execute(
            "insert into position_events
                (id, position_id, event_kind, occurred_at, source_kind, source_ref,
                 payload_json, agent_note_md, created_at)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                position_id,
                event_kind,
                occurred_at,
                source_kind,
                source_ref,
                event.to_string(),
                agent_note,
                now()
            ],
        )
        .map_err(|err| format!("写入持仓事件失败：{err}"))?;
    }

    // 5. chat_messages
    let mut stored_messages: Vec<Value> = Vec::with_capacity(payload.chat_messages.len());
    for message in &payload.chat_messages {
        let id = required_json_string(message, "/id", "对话消息缺少 id")?;
        let created_at =
            json_string(message, "/createdAt").unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        let role = required_json_string(message, "/role", "对话消息缺少 role")?;
        let kind = required_json_string(message, "/kind", "对话消息缺少 kind")?;
        let content_md = required_json_string(message, "/contentMd", "对话消息缺少 contentMd")?;
        let content_json = message
            .pointer("/contentJson")
            .filter(|v| !v.is_null())
            .map(|v| v.to_string());
        let source_task_id = json_string(message, "/sourceTaskId");
        let source_record_id = json_string(message, "/sourceRecordId");
        let source_news_ids = message
            .pointer("/sourceNewsIds")
            .filter(|v| !v.is_null())
            .map(|v| v.to_string());
        tx.execute(
            "insert into chat_messages
                (id, created_at, role, kind, content_md, content_json,
                 source_task_id, source_news_ids, source_record_id)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                created_at,
                role,
                kind,
                content_md,
                content_json,
                source_task_id,
                source_news_ids,
                source_record_id,
            ],
        )
        .map_err(|err| format!("写入对话消息失败：{err}"))?;
        // 收集事务内一致的形态（后续 commit 后再 emit 事件）
        stored_messages.push(read_chat_message_in_tx(&tx, &id)?);
    }

    // 6. news_items 状态翻转：consumed + revert
    let now_news = now();
    for id in &payload.news_consumed_ids {
        tx.execute(
            "update news_items set analysis_status = 'consumed', updated_at = ?2 where id = ?1",
            params![id, now_news],
        )
        .map_err(|err| format!("更新资讯 consumed 失败：{err}"))?;
    }
    for id in &payload.news_revert_ids {
        tx.execute(
            "update news_items set analysis_status = 'pending', updated_at = ?2
             where id = ?1 and analysis_status = 'processing'",
            params![id, now_news],
        )
        .map_err(|err| format!("回滚资讯 pending 失败：{err}"))?;
    }

    tx.commit()
        .map_err(|err| format!("提交 briefing 事务失败：{err}"))?;

    // commit 之后再 emit `chat-message-appended`——前端订阅看到的必须是已提交状态
    for msg in &stored_messages {
        let _ = app.emit("chat-message-appended", msg.clone());
    }
    Ok(stored_messages)
}

fn read_chat_message_in_tx(tx: &Transaction, id: &str) -> Result<Value, String> {
    tx.query_row(
        "select id, created_at, role, kind, content_md, content_json,
                source_task_id, source_news_ids, source_record_id
         from chat_messages where id = ?1",
        params![id],
        row_to_chat_message,
    )
    .map_err(|err| format!("读取对话消息失败：{err}"))
}

/// 一次 review 写盘的 payload——和 [`BriefingCommit`] 同型，只是 review 不动 memory
/// 也不切 news 状态。
///
/// 修复 review_pipeline 之前分步写的 torn-state 风险：
/// - replace_analysis_records 成功 → append_chat_message 失败 → record 已标 reviewed
///   但用户看不到复盘消息
/// - chat_message 成功 → reviewed event 失败 → 审计链缺 reviewed
/// - reviewed event 成功 → positions 覆盖失败 → 标 invalidated 但仓位还 open
#[derive(Debug, Default)]
pub struct ReviewCommit {
    /// 复盘后的全量 records（覆盖式 replace）。空 Vec 跳过。
    pub records: Vec<Value>,
    /// review 主消息（一般 1 条）。
    pub chat_messages: Vec<Value>,
    /// reviewed / invalidated / closed 等事件，按 occurred_at 顺序写入。
    pub position_events: Vec<Value>,
    /// 仅当有平仓时给——传入则覆盖 simulated_positions 全表。
    pub positions: Option<Vec<Value>>,
}

/// 单事务提交一次 review 的全部产物，行为模式镜像 [`commit_briefing`]：
/// 任何一步失败 → 整事务回滚 → DB 退回 review 之前的快照。返回 chat_messages
/// 落盘形态（commit 之后再 emit 给前端）。
pub fn commit_review(app: AppHandle, payload: ReviewCommit) -> Result<Vec<Value>, String> {
    let mut connection = open_database(&app)?;
    migrate(&connection)?;
    let tx = connection
        .transaction()
        .map_err(|err| format!("开启 review 事务失败：{err}"))?;

    // 1. records replace
    if !payload.records.is_empty() {
        tx.execute("delete from analysis_records", [])
            .map_err(|err| format!("清理分析记录失败：{err}"))?;
        for record in &payload.records {
            let id = required_json_string(record, "/id", "分析记录缺少 id")?;
            let item_id = required_json_string(record, "/item/id", "分析记录缺少 item.id")?;
            let created_at = json_string(record, "/createdAt")
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
            tx.execute(
                "insert into analysis_records (id, item_id, payload_json, created_at)
                 values (?1, ?2, ?3, ?4)",
                params![id, item_id, record.to_string(), created_at],
            )
            .map_err(|err| format!("写入分析记录失败：{err}"))?;
        }
    }

    // 2. chat_messages
    let mut stored_messages: Vec<Value> = Vec::with_capacity(payload.chat_messages.len());
    for message in &payload.chat_messages {
        let id = required_json_string(message, "/id", "对话消息缺少 id")?;
        let created_at =
            json_string(message, "/createdAt").unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        let role = required_json_string(message, "/role", "对话消息缺少 role")?;
        let kind = required_json_string(message, "/kind", "对话消息缺少 kind")?;
        let content_md = required_json_string(message, "/contentMd", "对话消息缺少 contentMd")?;
        let content_json = message
            .pointer("/contentJson")
            .filter(|v| !v.is_null())
            .map(|v| v.to_string());
        let source_task_id = json_string(message, "/sourceTaskId");
        let source_record_id = json_string(message, "/sourceRecordId");
        let source_news_ids = message
            .pointer("/sourceNewsIds")
            .filter(|v| !v.is_null())
            .map(|v| v.to_string());
        tx.execute(
            "insert into chat_messages
                (id, created_at, role, kind, content_md, content_json,
                 source_task_id, source_news_ids, source_record_id)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                created_at,
                role,
                kind,
                content_md,
                content_json,
                source_task_id,
                source_news_ids,
                source_record_id,
            ],
        )
        .map_err(|err| format!("写入对话消息失败：{err}"))?;
        stored_messages.push(read_chat_message_in_tx(&tx, &id)?);
    }

    // 3. position_events
    for event in &payload.position_events {
        let id = required_json_string(event, "/id", "持仓事件缺少 id")?;
        let position_id = required_json_string(event, "/positionId", "持仓事件缺少 positionId")?;
        let event_kind = required_json_string(event, "/eventKind", "持仓事件缺少 eventKind")?;
        let occurred_at = json_string(event, "/occurredAt").unwrap_or_else(now);
        let source_kind = json_string(event, "/sourceKind");
        let source_ref = json_string(event, "/sourceRef");
        let agent_note = json_string(event, "/agentNoteMd");
        tx.execute(
            "insert into position_events
                (id, position_id, event_kind, occurred_at, source_kind, source_ref,
                 payload_json, agent_note_md, created_at)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                position_id,
                event_kind,
                occurred_at,
                source_kind,
                source_ref,
                event.to_string(),
                agent_note,
                now()
            ],
        )
        .map_err(|err| format!("写入持仓事件失败：{err}"))?;
    }

    // 4. positions（仅当有平仓）
    if let Some(positions) = &payload.positions {
        let now = now();
        tx.execute("delete from simulated_positions", [])
            .map_err(|err| format!("清理模拟持仓失败：{err}"))?;
        for position in positions {
            let id = required_json_string(position, "/id", "模拟持仓缺少 id")?;
            let code = required_json_string(position, "/code", "模拟持仓缺少 code")?;
            let source_analysis_id = required_json_string(
                position,
                "/sourceAnalysisId",
                "模拟持仓缺少 sourceAnalysisId",
            )?;
            let status = required_json_string(position, "/status", "模拟持仓缺少 status")?;
            let created_at = json_string(position, "/entryAt").unwrap_or_else(|| now.clone());
            let updated_at = json_string(position, "/exitAt").unwrap_or_else(|| now.clone());
            tx.execute(
                "insert into simulated_positions
                    (id, code, source_analysis_id, status, payload_json, created_at, updated_at)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id,
                    code,
                    source_analysis_id,
                    status,
                    position.to_string(),
                    created_at,
                    updated_at
                ],
            )
            .map_err(|err| format!("写入模拟持仓失败：{err}"))?;
        }
    }

    tx.commit()
        .map_err(|err| format!("提交 review 事务失败：{err}"))?;

    for msg in &stored_messages {
        let _ = app.emit("chat-message-appended", msg.clone());
    }
    Ok(stored_messages)
}

/// 持仓事件（append-only 审计流）：opened / reviewed / adjusted / trimmed / added /
/// stop_triggered / invalidated / closed。事件不能被修改或删除，是 Agent 复盘时
/// 看到"这个仓位是怎么走过来的"的唯一可信来源。
pub fn append_position_event(app: AppHandle, event: Value) -> Result<Value, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let id = required_json_string(&event, "/id", "持仓事件缺少 id")?;
    let position_id = required_json_string(&event, "/positionId", "持仓事件缺少 positionId")?;
    let event_kind = required_json_string(&event, "/eventKind", "持仓事件缺少 eventKind")?;
    let occurred_at = json_string(&event, "/occurredAt").unwrap_or_else(now);
    let source_kind = json_string(&event, "/sourceKind");
    let source_ref = json_string(&event, "/sourceRef");
    let agent_note = json_string(&event, "/agentNoteMd");

    connection
        .execute(
            "insert into position_events
                (id, position_id, event_kind, occurred_at, source_kind, source_ref,
                 payload_json, agent_note_md, created_at)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                position_id,
                event_kind,
                occurred_at,
                source_kind,
                source_ref,
                event.to_string(),
                agent_note,
                now()
            ],
        )
        .map_err(|err| format!("写入持仓事件失败：{err}"))?;
    Ok(event)
}

/// 一次拉多个持仓的事件，按 occurred_at 升序。前端在内存里按 positionId 分组。
#[tauri::command]
pub fn list_position_events_batch(
    app: AppHandle,
    position_ids: Vec<String>,
) -> Result<Vec<Value>, String> {
    if position_ids.is_empty() {
        return Ok(vec![]);
    }
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let placeholders = (0..position_ids.len())
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "select payload_json from position_events
         where position_id in ({placeholders})
         order by occurred_at asc
         limit 2000"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|err| format!("读取批量持仓事件失败：{err}"))?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(position_ids.iter()), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|err| format!("读取批量持仓事件失败：{err}"))?
        .filter_map(|raw| raw.ok())
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        .collect();
    Ok(rows)
}

pub fn append_chat_message(app: AppHandle, message: Value) -> Result<Value, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let id = required_json_string(&message, "/id", "对话消息缺少 id")?;
    let created_at =
        json_string(&message, "/createdAt").unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let role = required_json_string(&message, "/role", "对话消息缺少 role")?;
    let kind = required_json_string(&message, "/kind", "对话消息缺少 kind")?;
    let content_md = required_json_string(&message, "/contentMd", "对话消息缺少 contentMd")?;
    let content_json = message
        .pointer("/contentJson")
        .filter(|v| !v.is_null())
        .map(|v| v.to_string());
    let source_task_id = json_string(&message, "/sourceTaskId");
    let source_record_id = json_string(&message, "/sourceRecordId");
    let source_news_ids = message
        .pointer("/sourceNewsIds")
        .filter(|v| !v.is_null())
        .map(|v| v.to_string());

    connection
        .execute(
            "insert into chat_messages
                (id, created_at, role, kind, content_md, content_json,
                 source_task_id, source_news_ids, source_record_id)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                created_at,
                role,
                kind,
                content_md,
                content_json,
                source_task_id,
                source_news_ids,
                source_record_id,
            ],
        )
        .map_err(|err| format!("写入对话消息失败：{err}"))?;
    let stored = read_chat_message(&connection, &id)?;
    let _ = app.emit("chat-message-appended", stored.clone());
    Ok(stored)
}

/// 前端分页用——UI 滚动加载 chat 历史。clamp 到 1..=200 保护渲染层不被一次性数千行打爆。
///
/// **不要给 agent pipeline 用**——agent 读历史应该走 `read_all_chat_messages` 拿全量，
/// 由 compact tier（MicroClear → Summarize → Drop）决定语义截断。
#[tauri::command]
pub fn list_chat_messages(
    app: AppHandle,
    before: Option<String>,
    limit: Option<i64>,
) -> Result<Vec<Value>, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let cap = limit.unwrap_or(50).clamp(1, 200);
    let mut statement = connection
        .prepare(
            "select id, created_at, role, kind, content_md, content_json,
                    source_task_id, source_news_ids, source_record_id
             from chat_messages
             where (?1 is null or created_at < ?1)
             order by created_at desc
             limit ?2",
        )
        .map_err(|err| format!("读取对话消息失败：{err}"))?;
    let rows = statement
        .query_map(params![before, cap], row_to_chat_message)
        .map_err(|err| format!("读取对话消息失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("读取对话消息失败：{err}"))?;
    Ok(rows)
}

/// Agent pipeline 专用——读取 chat_messages 全量历史，**不做条数截断**。
///
/// 与 `list_chat_messages` 区别：
/// - 不暴露为 Tauri command（仅 Rust 内部调用）；
/// - 没有 UI 友好的 200 行钳位——agent 拿到的就该是 DB 里 `before` 之前的所有行；
/// - 语义截断由上层 `read_recent_chat_thread` + compact tier 处理。
///
/// 主流共识（Cursor / Cline / Roo / Aider / OpenAI Agents SDK / LangGraph /
/// Anthropic Cookbook）：DB 读取层不截语义，先读全，让 compaction 层按 token 决定。
///
/// `before` 用于游标分页（一般不传，pipeline 拿最新到现在）。
pub fn read_all_chat_messages(app: &AppHandle, before: Option<&str>) -> Result<Vec<Value>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let mut statement = connection
        .prepare(
            "select id, created_at, role, kind, content_md, content_json,
                    source_task_id, source_news_ids, source_record_id
             from chat_messages
             where (?1 is null or created_at < ?1)
             order by created_at desc",
        )
        .map_err(|err| format!("读取对话消息失败：{err}"))?;
    let rows = statement
        .query_map(params![before], row_to_chat_message)
        .map_err(|err| format!("读取对话消息失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("读取对话消息失败：{err}"))?;
    Ok(rows)
}

#[tauri::command]
pub fn search_chat_messages(
    app: AppHandle,
    query: String,
    limit: Option<i64>,
) -> Result<Vec<Value>, String> {
    let connection = open_database(&app)?;
    migrate(&connection)?;
    let cap = limit.unwrap_or(50).clamp(1, 200);
    let pattern = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
    let mut statement = connection
        .prepare(
            "select id, created_at, role, kind, content_md, content_json,
                    source_task_id, source_news_ids, source_record_id
             from chat_messages
             where content_md like ?1 escape '\\'
             order by created_at desc
             limit ?2",
        )
        .map_err(|err| format!("搜索对话消息失败：{err}"))?;
    let rows = statement
        .query_map(params![pattern, cap], row_to_chat_message)
        .map_err(|err| format!("搜索对话消息失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("搜索对话消息失败：{err}"))?;
    Ok(rows)
}

/// agent_runs 表的写入入口——在 run 启动时插一条 started_at；run 结束时
/// update token / turns / stop_reason / ended_at。observer.rs 调这两个。
pub fn insert_agent_run_start(
    app: &AppHandle,
    run_id: &str,
    pipeline: &str,
    provider: &str,
    model: &str,
    started_at: &str,
    trigger_message_id: Option<&str>,
) -> Result<(), String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .execute(
            "insert into agent_runs (run_id, pipeline, provider, model, started_at, trigger_message_id)
             values (?1, ?2, ?3, ?4, ?5, ?6)",
            params![run_id, pipeline, provider, model, started_at, trigger_message_id],
        )
        .map_err(|err| format!("写 agent_runs 失败：{err}"))?;
    Ok(())
}

/// 每个 turn 收尾时落一行 agent_run_turns。
/// 投资学习的关键审计点——briefing 给错答案时能 grep 出第几 turn 出岔子。
#[allow(clippy::too_many_arguments)]
pub fn insert_agent_run_turn(
    app: &AppHandle,
    run_id: &str,
    turn: u32,
    started_at: &str,
    ended_at: &str,
    stop_reason: Option<&str>,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    local_tool_calls: u32,
    server_tool_calls: u32,
    error: Option<&str>,
) -> Result<(), String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .execute(
            "insert or replace into agent_run_turns
                (run_id, turn, started_at, ended_at, stop_reason,
                 input_tokens, output_tokens, cache_read_tokens,
                 local_tool_calls, server_tool_calls, error)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                run_id,
                turn,
                started_at,
                ended_at,
                stop_reason,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                local_tool_calls,
                server_tool_calls,
                error,
            ],
        )
        .map_err(|err| format!("写 agent_run_turns 失败：{err}"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn finalize_agent_run(
    app: &AppHandle,
    run_id: &str,
    ended_at: &str,
    turns: u32,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
    local_tool_calls: u32,
    server_tool_calls: u32,
    stop_reason: Option<&str>,
    error: Option<&str>,
) -> Result<(), String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .execute(
            "update agent_runs set
                ended_at = ?2,
                turns = ?3,
                input_tokens = ?4,
                output_tokens = ?5,
                cache_read_tokens = ?6,
                cache_write_tokens = ?7,
                local_tool_calls = ?8,
                server_tool_calls = ?9,
                stop_reason = ?10,
                error = ?11
             where run_id = ?1",
            params![
                run_id,
                ended_at,
                turns,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                local_tool_calls,
                server_tool_calls,
                stop_reason,
                error,
            ],
        )
        .map_err(|err| format!("更新 agent_runs 失败：{err}"))?;
    Ok(())
}

fn read_chat_message(connection: &Connection, id: &str) -> Result<Value, String> {
    connection
        .query_row(
            "select id, created_at, role, kind, content_md, content_json,
                    source_task_id, source_news_ids, source_record_id
             from chat_messages where id = ?1",
            params![id],
            row_to_chat_message,
        )
        .map_err(|err| format!("读取对话消息失败：{err}"))
}

fn row_to_chat_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let content_json_raw: Option<String> = row.get(5)?;
    let source_news_raw: Option<String> = row.get(7)?;
    Ok(serde_json::json!({
        "id": row.get::<_, String>(0)?,
        "createdAt": row.get::<_, String>(1)?,
        "role": row.get::<_, String>(2)?,
        "kind": row.get::<_, String>(3)?,
        "contentMd": row.get::<_, String>(4)?,
        "contentJson": content_json_raw
            .as_deref()
            .map(|text| serde_json::from_str::<Value>(text).unwrap_or(Value::Null))
            .unwrap_or(Value::Null),
        "sourceTaskId": row.get::<_, Option<String>>(6)?,
        "sourceNewsIds": source_news_raw
            .as_deref()
            .map(|text| serde_json::from_str::<Value>(text).unwrap_or(Value::Null))
            .unwrap_or(Value::Null),
        "sourceRecordId": row.get::<_, Option<String>>(8)?,
    }))
}

fn database_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|err| format!("获取应用数据目录失败：{err}"))?;
    fs::create_dir_all(&dir).map_err(|err| format!("创建应用数据目录失败：{err}"))?;
    Ok(dir.join("gangzi-terminal.sqlite3"))
}

fn open_database(app: &AppHandle) -> Result<Connection, String> {
    let path = database_path(app)?;
    static LOGGED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let _ = LOGGED.get_or_init(|| {
        tracing::info!(path = %path.display(), "SQLite 数据库路径");
    });
    let connection = Connection::open(path).map_err(|err| format!("打开 SQLite 失败：{err}"))?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(|err| format!("启用 WAL 失败：{err}"))?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|err| format!("启用外键失败：{err}"))?;
    Ok(connection)
}

fn migrate(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            "
            create table if not exists schema_meta (
                id integer primary key check (id = 1),
                version integer not null,
                updated_at text not null
            );

            create table if not exists app_state (
                key text primary key,
                value_json text not null,
                updated_at text not null
            );

            create table if not exists news_items (
                id text primary key,
                source text not null,
                published text,
                payload_json text not null,
                analysis_status text not null default 'pending',
                created_at text not null,
                updated_at text not null
            );

            create table if not exists article_contents (
                url text primary key,
                item_id text,
                payload_json text not null,
                fetched_at text not null
            );

            create table if not exists analysis_records (
                id text primary key,
                item_id text not null unique,
                payload_json text not null,
                created_at text not null
            );

            create table if not exists simulated_positions (
                id text primary key,
                code text not null,
                source_analysis_id text not null,
                status text not null,
                payload_json text not null,
                created_at text not null,
                updated_at text not null
            );

            create table if not exists agent_tasks (
                id text primary key,
                item_id text not null,
                title text not null,
                task_type text,
                agent_role text,
                status text not null check (status in ('queued', 'running', 'completed', 'failed', 'skipped')),
                priority integer not null default 100,
                attempts integer not null default 0,
                input_json text not null,
                result_json text,
                error text,
                session_id text,
                parent_task_id text,
                locked_at text,
                created_at text not null,
                updated_at text not null,
                started_at text,
                completed_at text
            );

            create table if not exists position_events (
                id text primary key,
                position_id text not null,
                event_kind text not null,
                occurred_at text not null,
                source_kind text,
                source_ref text,
                payload_json text not null,
                agent_note_md text,
                created_at text not null
            );

            create table if not exists chat_messages (
                id text primary key,
                created_at text not null,
                role text not null check (role in ('user', 'assistant', 'system')),
                kind text not null check (kind in ('chat', 'briefing', 'review', 'system', 'highlight', 'compact_boundary')),
                content_md text not null,
                content_json text,
                source_task_id text,
                source_news_ids text,
                source_record_id text
            );

            create table if not exists agent_runs (
                run_id text primary key,
                pipeline text not null check (pipeline in ('chat', 'briefing', 'review')),
                provider text not null,
                model text not null,
                started_at text not null,
                ended_at text,
                turns integer not null default 0,
                input_tokens integer not null default 0,
                output_tokens integer not null default 0,
                cache_read_tokens integer not null default 0,
                cache_write_tokens integer not null default 0,
                local_tool_calls integer not null default 0,
                server_tool_calls integer not null default 0,
                stop_reason text,
                error text,
                trigger_message_id text
            );
            create index if not exists idx_agent_runs_pipeline_started on agent_runs(pipeline, started_at desc);

            create table if not exists agent_run_turns (
                run_id text not null,
                turn integer not null,
                started_at text not null,
                ended_at text,
                stop_reason text,
                input_tokens integer not null default 0,
                output_tokens integer not null default 0,
                cache_read_tokens integer not null default 0,
                local_tool_calls integer not null default 0,
                server_tool_calls integer not null default 0,
                error text,
                primary key (run_id, turn)
            );
            create index if not exists idx_agent_run_turns_run on agent_run_turns(run_id, turn);

            create table if not exists stocks (
                code text primary key,
                name text not null,
                sector text,
                market text not null,
                updated_at text not null
            );

            -- 大盘指数档案：ts_code (000001.SH 形式) 是 PK，和 stocks 不冲突
            create table if not exists indexes (
                ts_code text primary key,
                code text not null,
                name text not null,
                market text not null,        -- SSE / SZSE / CSI / SW（申万）
                publisher text,              -- 发布机构
                category text,               -- 大盘 / 行业 / 主题 / 风格
                updated_at text not null
            );

            -- 基金档案：ETF / LOF / 封基 等 ts_code 是 PK
            create table if not exists funds (
                ts_code text primary key,
                code text not null,
                name text not null,
                market text not null,        -- E (场内) / O (场外)
                fund_type text,              -- 股票型 / 混合型 / 债券型 / 货币型 / ETF / LOF
                management text,             -- 管理人
                list_date text,              -- 上市日 YYYYMMDD
                status text,                 -- L (上市) / D (退市)
                updated_at text not null
            );

            -- K 线缓存：个股 / 指数 / 基金 统一存这里
            -- ts_code 作为 PK 一部分，避免 000001 SH（上证）vs 000001 SZ（平安）冲突
            create table if not exists klines (
                ts_code text not null,       -- 形如 000001.SZ / 510300.SH / 399006.SZ
                period text not null,        -- day / week / month
                adjust text not null,        -- qfq / hfq / none
                date text not null,          -- YYYYMMDD
                open real not null,
                close real not null,
                high real not null,
                low real not null,
                volume real,
                amount real,
                source text not null,        -- tushare / em / stale
                primary key (ts_code, period, adjust, date)
            );

            create table if not exists kline_meta (
                ts_code text not null,
                period text not null,
                adjust text not null,
                last_known_date text not null,
                fetched_at text not null,
                primary key (ts_code, period, adjust)
            );

            -- 分钟 K 缓存：1m / 5m / 15m / 30m / 60m
            -- 走 EM push2his klt 端点拉取，盘中持续累加，TTL 30s
            create table if not exists minute_klines (
                ts_code text not null,
                period text not null,           -- 1m / 5m / 15m / 30m / 60m
                timestamp_ms integer not null,  -- unix ms（北京 9:30 = UTC 01:30）
                open real not null,
                close real not null,
                high real not null,
                low real not null,
                volume integer not null,
                amount real not null,
                source text not null,
                primary key (ts_code, period, timestamp_ms)
            );

            create table if not exists minute_kline_meta (
                ts_code text not null,
                period text not null,
                last_known_ts integer not null,
                fetched_at text not null,
                primary key (ts_code, period)
            );

            create index if not exists idx_agent_tasks_status_priority on agent_tasks(status, priority, created_at);
            create index if not exists idx_analysis_records_item_id on analysis_records(item_id);
            create index if not exists idx_simulated_positions_code_status on simulated_positions(code, status);
            create index if not exists idx_chat_messages_created on chat_messages(created_at desc);
            create index if not exists idx_chat_messages_kind on chat_messages(kind, created_at desc);
            create index if not exists idx_position_events_pos_time on position_events(position_id, occurred_at);
            create index if not exists idx_stocks_name on stocks(name);
            create index if not exists idx_indexes_name on indexes(name);
            create index if not exists idx_funds_name on funds(name);
            create index if not exists idx_funds_market on funds(market);
            -- 注意：klines 表的索引 idx_klines_ts_period_date 不在这里建——
            -- 必须等 upgrade_klines_to_ts_code 把旧 schema (列 `code`) 升级为新 schema (列 `ts_code`)
            -- 之后才能建。见 migrate() 末尾。
            ",
        )
        .map_err(|err| format!("初始化 SQLite schema 失败：{err}"))?;

    // K 线表 schema 升级：旧版列叫 `code`（6 位），新版叫 `ts_code`（带后缀）。
    // 老表存在且没有 ts_code 列 → DROP 重建（缓存数据丢就丢，反正是缓存）
    upgrade_klines_to_ts_code(connection)?;

    // upgrade 完成后，安全建 klines 的新索引（针对 ts_code 列）
    connection
        .execute_batch(
            "create index if not exists idx_minute_klines_ts_period_ts
             on minute_klines(ts_code, period, timestamp_ms desc);
             create index if not exists idx_klines_ts_period_date
             on klines(ts_code, period, adjust, date desc);",
        )
        .map_err(|err| format!("建 klines 索引失败：{err}"))?;

    add_column_if_missing(
        connection,
        "news_items",
        "analysis_status",
        "text not null default 'pending'",
    )?;
    // 索引依赖 analysis_status 列，必须在 add_column_if_missing 之后建
    connection
        .execute(
            "create index if not exists idx_news_items_status_published on news_items(analysis_status, published desc)",
            [],
        )
        .map_err(|err| format!("创建资讯状态索引失败：{err}"))?;
    // 旧版 analysis_status 值（completed/reviewed/failed）统一收敛为 consumed；
    // processing 留着，由 recover_stale_processing_news 回 pending
    connection
        .execute(
            "update news_items set analysis_status = 'consumed'
             where analysis_status in ('completed', 'reviewed', 'failed')",
            [],
        )
        .map_err(|err| format!("迁移 analysis_status 失败：{err}"))?;
    // 旧版多 session 对话表已弃用，直接清掉避免存量数据干扰
    connection
        .execute("drop table if exists chat_sessions", [])
        .map_err(|err| format!("清理旧 chat_sessions 表失败：{err}"))?;
    // 撤销/审计已删除，连同表一并清理
    connection
        .execute("drop table if exists investor_memory_log", [])
        .map_err(|err| format!("清理旧 investor_memory_log 表失败：{err}"))?;
    // 旧行情 DB 快照已被 `MARKET_SNAPSHOT` in-memory 真源替代；不保留兼容。
    connection
        .execute("drop table if exists quote_snapshots", [])
        .map_err(|err| format!("清理旧 quote_snapshots 表失败：{err}"))?;
    connection
        .execute("drop table if exists market_quotes_snapshot", [])
        .map_err(|err| format!("清理旧 market_quotes_snapshot 表失败：{err}"))?;
    // codex 时代的 MCP 注册诊断状态——agent 直连后已不写入；老用户本地若残留
    // 一条失败记录会让前端 status bar 持续显示"MCP 工具未启用"，一次性清掉。
    connection
        .execute(
            "delete from app_state where key = 'gangzi-terminal.mcp-status'",
            [],
        )
        .map_err(|err| format!("清理旧 mcp-status key 失败：{err}"))?;
    // codex CLI 的长会话 id——agent 自管 messages，不再需要 resume；清掉避免误读。
    connection
        .execute(
            "delete from app_state where key = 'gangzi-terminal.main-agent-session-id'",
            [],
        )
        .map_err(|err| format!("清理旧 codex session-id key 失败：{err}"))?;
    add_column_if_missing(connection, "agent_tasks", "task_type", "text")?;
    add_column_if_missing(connection, "agent_tasks", "agent_role", "text")?;
    add_column_if_missing(
        connection,
        "agent_tasks",
        "attempts",
        "integer not null default 0",
    )?;
    add_column_if_missing(connection, "agent_tasks", "parent_task_id", "text")?;
    add_column_if_missing(connection, "agent_tasks", "locked_at", "text")?;

    // 老库的 chat_messages 表 CHECK 约束没有 'compact_boundary'——让 compact 边界
    // 行写入会失败。SQLite 不支持 ALTER CHECK，必须重建表。检查现有 sql 文本里
    // 是否已包含新值，否则做一次性重建。
    upgrade_chat_messages_kind_check(connection)?;

    connection
        .execute(
            "insert into schema_meta (id, version, updated_at)
             values (1, ?1, ?2)
             on conflict(id) do update set version = excluded.version, updated_at = excluded.updated_at",
            params![SCHEMA_VERSION, now()],
        )
        .map_err(|err| format!("写入 schema 版本失败：{err}"))?;
    Ok(())
}

/// 一次性升级 chat_messages 表的 kind CHECK 约束，让它接受 'compact_boundary'。
///
/// SQLite 不支持 ALTER CHECK——只能重建：
/// 1. 读现有 schema 文本，确认是否需要重建
/// 2. 重命名旧表到 _legacy
/// 3. 按当前定义建新表
/// 4. 数据 copy
/// 5. 删 _legacy + 重建索引
///
/// 整套流程包在事务里——失败则回滚到旧表。
fn upgrade_chat_messages_kind_check(connection: &Connection) -> Result<(), String> {
    let existing_sql: Option<String> = connection
        .query_row(
            "select sql from sqlite_master where type='table' and name='chat_messages'",
            [],
            |row| row.get(0),
        )
        .ok();
    let needs_upgrade = match existing_sql {
        Some(sql) => !sql.contains("compact_boundary"),
        None => false, // 表不存在——上一步 create table if not exists 已用新约束建好
    };
    if !needs_upgrade {
        return Ok(());
    }
    tracing::info!("升级 chat_messages.kind CHECK 约束以接受 compact_boundary");
    connection
        .execute_batch(
            "begin transaction;
             alter table chat_messages rename to chat_messages_legacy;
             create table chat_messages (
                id text primary key,
                created_at text not null,
                role text not null check (role in ('user', 'assistant', 'system')),
                kind text not null check (kind in ('chat', 'briefing', 'review', 'system', 'highlight', 'compact_boundary')),
                content_md text not null,
                content_json text,
                source_task_id text,
                source_news_ids text,
                source_record_id text
             );
             insert into chat_messages
                (id, created_at, role, kind, content_md, content_json,
                 source_task_id, source_news_ids, source_record_id)
             select id, created_at, role, kind, content_md, content_json,
                    source_task_id, source_news_ids, source_record_id
             from chat_messages_legacy;
             drop table chat_messages_legacy;
             create index if not exists idx_chat_messages_created on chat_messages(created_at desc);
             create index if not exists idx_chat_messages_kind on chat_messages(kind, created_at desc);
             commit;",
        )
        .map_err(|err| format!("升级 chat_messages CHECK 约束失败：{err}"))?;
    Ok(())
}

/// K 线表 schema 升级——把旧 `klines`/`kline_meta`（列 `code` 6 位）替换为新版（列 `ts_code` 带后缀）。
/// 旧 schema 不能容纳指数/基金（000001.SH 和 000001.SZ 冲突），直接 DROP 重建。缓存丢就丢。
fn upgrade_klines_to_ts_code(connection: &Connection) -> Result<(), String> {
    let has_ts_code = |table: &str| -> Result<bool, String> {
        let mut stmt = connection
            .prepare(&format!("pragma table_info({table})"))
            .map_err(|err| format!("读取 {table} schema 失败：{err}"))?;
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|err| format!("读取 {table} schema 失败：{err}"))?
            .filter_map(|r| r.ok())
            .collect();
        if names.is_empty() {
            return Ok(true); // 表不存在 → 由 create table if not exists 建好
        }
        Ok(names.iter().any(|n| n == "ts_code"))
    };

    if !has_ts_code("klines")? {
        tracing::info!("升级 klines schema：DROP + 重建（旧缓存丢失，下次访问会重拉）");
        connection
            .execute_batch(
                "drop table if exists klines;
                 create table klines (
                     ts_code text not null,
                     period text not null,
                     adjust text not null,
                     date text not null,
                     open real not null,
                     close real not null,
                     high real not null,
                     low real not null,
                     volume real,
                     amount real,
                     source text not null,
                     primary key (ts_code, period, adjust, date)
                 );
                 create index idx_klines_ts_period_date on klines(ts_code, period, adjust, date desc);",
            )
            .map_err(|err| format!("升级 klines 失败：{err}"))?;
    }
    if !has_ts_code("kline_meta")? {
        tracing::info!("升级 kline_meta schema：DROP + 重建");
        connection
            .execute_batch(
                "drop table if exists kline_meta;
                 create table kline_meta (
                     ts_code text not null,
                     period text not null,
                     adjust text not null,
                     last_known_date text not null,
                     fetched_at text not null,
                     primary key (ts_code, period, adjust)
                 );",
            )
            .map_err(|err| format!("升级 kline_meta 失败：{err}"))?;
    }
    Ok(())
}

fn add_column_if_missing(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(&format!("pragma table_info({table})"))
        .map_err(|err| format!("读取 {table} schema 失败：{err}"))?;
    let exists = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|err| format!("读取 {table} schema 失败：{err}"))?
        .any(|name| name.map(|value| value == column).unwrap_or(false));
    if !exists {
        connection
            .execute(
                &format!("alter table {table} add column {column} {definition}"),
                [],
            )
            .map_err(|err| format!("升级 {table}.{column} 失败：{err}"))?;
    }
    Ok(())
}

fn list_json_payloads(
    connection: &Connection,
    sql: &str,
    limit: i64,
    context: &str,
) -> Result<Vec<Value>, String> {
    let mut statement = connection
        .prepare(sql)
        .map_err(|err| format!("{context}：{err}"))?;
    let payloads = statement
        .query_map(params![limit], |row| row.get::<_, String>(0))
        .map_err(|err| format!("{context}：{err}"))?
        .map(|raw| {
            raw.map_err(|err| format!("{context}：{err}"))
                .and_then(|text| {
                    serde_json::from_str::<Value>(&text)
                        .map_err(|err| format!("{context} JSON解析失败：{err}"))
                })
        })
        .collect();
    payloads
}

fn required_json_string(value: &Value, pointer: &str, message: &str) -> Result<String, String> {
    json_string(value, pointer).ok_or_else(|| message.to_string())
}

fn json_string(value: &Value, pointer: &str) -> Option<String> {
    value.pointer(pointer).and_then(|field| {
        field
            .as_str()
            .map(str::to_string)
            .or_else(|| field.as_i64().map(|number| number.to_string()))
            .or_else(|| field.as_u64().map(|number| number.to_string()))
    })
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ===== Stocks reference table ============================================

pub struct StockRow {
    pub code: String,
    pub name: String,
    pub sector: Option<String>,
    pub market: String,
}

/// 批量 upsert stocks（开/补全市场档案）。在事务里一次性写完，避免逐条 commit 的开销。
pub fn upsert_stocks(app: AppHandle, rows: Vec<StockRow>) -> Result<usize, String> {
    let mut connection = open_database(&app)?;
    migrate(&connection)?;
    let now = now();
    let count = rows.len();
    let tx = connection
        .transaction()
        .map_err(|err| format!("开启事务失败：{err}"))?;
    {
        let mut stmt = tx
            .prepare(
                "insert into stocks (code, name, sector, market, updated_at)
                 values (?1, ?2, ?3, ?4, ?5)
                 on conflict(code) do update set
                     name = excluded.name,
                     sector = excluded.sector,
                     market = excluded.market,
                     updated_at = excluded.updated_at",
            )
            .map_err(|err| format!("准备 stocks upsert 失败：{err}"))?;
        for row in rows {
            stmt.execute(params![row.code, row.name, row.sector, row.market, now])
                .map_err(|err| format!("写入 stock {} 失败：{err}", row.code))?;
        }
    }
    tx.commit().map_err(|err| format!("提交事务失败：{err}"))?;
    Ok(count)
}

pub fn count_stocks(app: &AppHandle) -> Result<i64, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .query_row("select count(*) from stocks", [], |row| row.get(0))
        .map_err(|err| format!("count stocks 失败：{err}"))
}

/// 6 位 code → 带后缀的 ts_code（"600519" → "600519.SH"）。
/// **唯一可靠路径**：通过 stocks 表里 TuShare 返回的 market 字段拼。
/// stocks 表未命中（新股 / 表空）时返 None——caller 应该等档案刷新后再查，**不要前缀猜测**。
pub fn resolve_stock_ts_code(app: &AppHandle, code: &str) -> Option<String> {
    let row = find_stock_by_code(app, code).ok().flatten()?;
    let suffix = match row.market.as_str() {
        "sh" => "SH",
        "sz" => "SZ",
        "bj" => "BJ",
        _ => return None,
    };
    Some(format!("{code}.{suffix}"))
}

pub fn find_stock_by_code(app: &AppHandle, code: &str) -> Result<Option<StockRow>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .query_row(
            "select code, name, sector, market from stocks where code = ?1",
            params![code],
            |row| {
                Ok(StockRow {
                    code: row.get(0)?,
                    name: row.get(1)?,
                    sector: row.get::<_, Option<String>>(2)?,
                    market: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(|err| format!("查询 stock by code 失败：{err}"))
}

/// 按名字找股票——精确匹配优先，没有再走 LIKE %name% 模糊。
/// 返回最多 `limit` 条，按 code 升序。
pub fn find_stocks_by_name(
    app: &AppHandle,
    name: &str,
    limit: usize,
) -> Result<Vec<StockRow>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    // 先精确
    let exact: Vec<StockRow> = {
        let mut stmt = connection
            .prepare(
                "select code, name, sector, market from stocks where name = ?1 order by code limit ?2",
            )
            .map_err(|err| format!("准备精确匹配失败：{err}"))?;
        let rows: Vec<StockRow> = stmt
            .query_map(params![name, limit as i64], |row| {
                Ok(StockRow {
                    code: row.get(0)?,
                    name: row.get(1)?,
                    sector: row.get::<_, Option<String>>(2)?,
                    market: row.get(3)?,
                })
            })
            .map_err(|err| format!("精确匹配查询失败：{err}"))?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };
    if !exact.is_empty() {
        return Ok(exact);
    }
    // 再 LIKE %name%
    let pattern = format!("%{name}%");
    let mut stmt = connection
        .prepare(
            "select code, name, sector, market from stocks where name like ?1 order by code limit ?2",
        )
        .map_err(|err| format!("准备模糊匹配失败：{err}"))?;
    let rows: Vec<StockRow> = stmt
        .query_map(params![pattern, limit as i64], |row| {
            Ok(StockRow {
                code: row.get(0)?,
                name: row.get(1)?,
                sector: row.get::<_, Option<String>>(2)?,
                market: row.get(3)?,
            })
        })
        .map_err(|err| format!("模糊匹配查询失败：{err}"))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

// ===== indexes 表（大盘 / 行业 / 主题指数档案） =========================

pub struct IndexRow {
    pub ts_code: String,
    pub code: String,
    pub name: String,
    pub market: String,
    pub publisher: Option<String>,
    pub category: Option<String>,
}

pub fn upsert_indexes(app: AppHandle, rows: Vec<IndexRow>) -> Result<usize, String> {
    let mut connection = open_database(&app)?;
    migrate(&connection)?;
    let now = now();
    let count = rows.len();
    let tx = connection
        .transaction()
        .map_err(|err| format!("开启事务失败：{err}"))?;
    {
        let mut stmt = tx
            .prepare(
                "insert into indexes (ts_code, code, name, market, publisher, category, updated_at)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 on conflict(ts_code) do update set
                     code = excluded.code,
                     name = excluded.name,
                     market = excluded.market,
                     publisher = excluded.publisher,
                     category = excluded.category,
                     updated_at = excluded.updated_at",
            )
            .map_err(|err| format!("准备 indexes upsert 失败：{err}"))?;
        for r in rows {
            stmt.execute(params![
                r.ts_code,
                r.code,
                r.name,
                r.market,
                r.publisher,
                r.category,
                now
            ])
            .map_err(|err| format!("写入 index {} 失败：{err}", r.ts_code))?;
        }
    }
    tx.commit().map_err(|err| format!("提交事务失败：{err}"))?;
    Ok(count)
}

pub fn list_indexes(app: &AppHandle) -> Result<Vec<IndexRow>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let mut stmt = connection
        .prepare(
            "select ts_code, code, name, market, publisher, category from indexes order by ts_code",
        )
        .map_err(|err| format!("准备 list_indexes 失败：{err}"))?;
    let rows: Vec<IndexRow> = stmt
        .query_map([], |row| {
            Ok(IndexRow {
                ts_code: row.get(0)?,
                code: row.get(1)?,
                name: row.get(2)?,
                market: row.get(3)?,
                publisher: row.get::<_, Option<String>>(4)?,
                category: row.get::<_, Option<String>>(5)?,
            })
        })
        .map_err(|err| format!("list_indexes 查询失败：{err}"))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn count_indexes(app: &AppHandle) -> Result<i64, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .query_row("select count(*) from indexes", [], |row| row.get(0))
        .map_err(|err| format!("count indexes 失败：{err}"))
}

// ===== funds 表（ETF / LOF / 封基 等基金档案） ==========================

pub struct FundRow {
    pub ts_code: String,
    pub code: String,
    pub name: String,
    pub market: String, // E / O
    pub fund_type: Option<String>,
    pub management: Option<String>,
    pub list_date: Option<String>,
    pub status: Option<String>,
}

pub fn upsert_funds(app: AppHandle, rows: Vec<FundRow>) -> Result<usize, String> {
    let mut connection = open_database(&app)?;
    migrate(&connection)?;
    let now = now();
    let count = rows.len();
    let tx = connection
        .transaction()
        .map_err(|err| format!("开启事务失败：{err}"))?;
    {
        let mut stmt = tx
            .prepare(
                "insert into funds (ts_code, code, name, market, fund_type, management, list_date, status, updated_at)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 on conflict(ts_code) do update set
                     code = excluded.code,
                     name = excluded.name,
                     market = excluded.market,
                     fund_type = excluded.fund_type,
                     management = excluded.management,
                     list_date = excluded.list_date,
                     status = excluded.status,
                     updated_at = excluded.updated_at",
            )
            .map_err(|err| format!("准备 funds upsert 失败：{err}"))?;
        for r in rows {
            stmt.execute(params![
                r.ts_code,
                r.code,
                r.name,
                r.market,
                r.fund_type,
                r.management,
                r.list_date,
                r.status,
                now,
            ])
            .map_err(|err| format!("写入 fund {} 失败：{err}", r.ts_code))?;
        }
    }
    tx.commit().map_err(|err| format!("提交事务失败：{err}"))?;
    Ok(count)
}

/// 列出场内基金（ETF/LOF）——给"今日市场"列表用。场外基金（O）数量太多且不实时刷，先不暴露。
pub fn list_listed_funds(app: &AppHandle) -> Result<Vec<FundRow>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let mut stmt = connection
        .prepare(
            "select ts_code, code, name, market, fund_type, management, list_date, status
             from funds where market = 'E' and (status is null or status = 'L')
             order by ts_code",
        )
        .map_err(|err| format!("准备 list_listed_funds 失败：{err}"))?;
    let rows: Vec<FundRow> = stmt
        .query_map([], |row| {
            Ok(FundRow {
                ts_code: row.get(0)?,
                code: row.get(1)?,
                name: row.get(2)?,
                market: row.get(3)?,
                fund_type: row.get::<_, Option<String>>(4)?,
                management: row.get::<_, Option<String>>(5)?,
                list_date: row.get::<_, Option<String>>(6)?,
                status: row.get::<_, Option<String>>(7)?,
            })
        })
        .map_err(|err| format!("list_listed_funds 查询失败：{err}"))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn count_funds(app: &AppHandle) -> Result<i64, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .query_row("select count(*) from funds", [], |row| row.get(0))
        .map_err(|err| format!("count funds 失败：{err}"))
}

/// list all stocks for the market list IPC
pub fn list_stocks(app: &AppHandle) -> Result<Vec<StockRow>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let mut stmt = connection
        .prepare("select code, name, sector, market from stocks order by code")
        .map_err(|err| format!("准备 list_stocks 失败：{err}"))?;
    let rows: Vec<StockRow> = stmt
        .query_map([], |row| {
            Ok(StockRow {
                code: row.get(0)?,
                name: row.get(1)?,
                sector: row.get::<_, Option<String>>(2)?,
                market: row.get(3)?,
            })
        })
        .map_err(|err| format!("list_stocks 查询失败：{err}"))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

// ===== K 线行级缓存（个股 / 指数 / 基金 统一用 ts_code）=================

pub struct KlineRow {
    pub ts_code: String, // "000001.SZ" / "510300.SH" / "399006.SZ"
    pub period: String,  // day / week / month
    pub adjust: String,  // qfq / hfq / none
    pub date: String,    // YYYYMMDD
    pub open: f64,
    pub close: f64,
    pub high: f64,
    pub low: f64,
    pub volume: Option<f64>,
    pub amount: Option<f64>,
    pub source: String, // tushare / em / stale
}

pub struct KlineMetaRow {
    pub last_known_date: String,
    pub fetched_at: String,
}

/// 批量 upsert K 线行——一事务提交避免逐条 commit。
pub fn upsert_klines(app: &AppHandle, rows: &[KlineRow]) -> Result<usize, String> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut connection = open_database(app)?;
    migrate(&connection)?;
    let count = rows.len();
    let tx = connection
        .transaction()
        .map_err(|err| format!("开启 klines 事务失败：{err}"))?;
    {
        let mut stmt = tx
            .prepare(
                "insert into klines (ts_code, period, adjust, date, open, close, high, low, volume, amount, source)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 on conflict(ts_code, period, adjust, date) do update set
                     open = excluded.open,
                     close = excluded.close,
                     high = excluded.high,
                     low = excluded.low,
                     volume = excluded.volume,
                     amount = excluded.amount,
                     source = excluded.source",
            )
            .map_err(|err| format!("准备 klines upsert 失败：{err}"))?;
        for row in rows {
            stmt.execute(params![
                row.ts_code,
                row.period,
                row.adjust,
                row.date,
                row.open,
                row.close,
                row.high,
                row.low,
                row.volume,
                row.amount,
                row.source,
            ])
            .map_err(|err| {
                format!(
                    "写 kline {}:{}:{} 失败：{err}",
                    row.ts_code, row.period, row.date
                )
            })?;
        }
    }
    tx.commit()
        .map_err(|err| format!("提交 klines 事务失败：{err}"))?;
    Ok(count)
}

/// 拿最近 N 根 K 线，按 date 升序返回（前端图表期望旧→新）。
pub fn find_klines(
    app: &AppHandle,
    ts_code: &str,
    period: &str,
    adjust: &str,
    limit: usize,
) -> Result<Vec<KlineRow>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let mut stmt = connection
        .prepare(
            "select ts_code, period, adjust, date, open, close, high, low, volume, amount, source
             from (
                 select * from klines
                 where ts_code = ?1 and period = ?2 and adjust = ?3
                 order by date desc
                 limit ?4
             )
             order by date asc",
        )
        .map_err(|err| format!("准备 klines 查询失败：{err}"))?;
    let rows: Vec<KlineRow> = stmt
        .query_map(params![ts_code, period, adjust, limit as i64], |row| {
            Ok(KlineRow {
                ts_code: row.get(0)?,
                period: row.get(1)?,
                adjust: row.get(2)?,
                date: row.get(3)?,
                open: row.get(4)?,
                close: row.get(5)?,
                high: row.get(6)?,
                low: row.get(7)?,
                volume: row.get::<_, Option<f64>>(8)?,
                amount: row.get::<_, Option<f64>>(9)?,
                source: row.get(10)?,
            })
        })
        .map_err(|err| format!("klines 查询失败：{err}"))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn get_kline_meta(
    app: &AppHandle,
    ts_code: &str,
    period: &str,
    adjust: &str,
) -> Result<Option<KlineMetaRow>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .query_row(
            "select last_known_date, fetched_at
             from kline_meta where ts_code = ?1 and period = ?2 and adjust = ?3",
            params![ts_code, period, adjust],
            |row| {
                Ok(KlineMetaRow {
                    last_known_date: row.get(0)?,
                    fetched_at: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(|err| format!("kline_meta 查询失败：{err}"))
}

pub fn upsert_kline_meta(
    app: &AppHandle,
    ts_code: &str,
    period: &str,
    adjust: &str,
    last_known_date: &str,
    fetched_at: &str,
) -> Result<(), String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .execute(
            "insert into kline_meta (ts_code, period, adjust, last_known_date, fetched_at)
             values (?1, ?2, ?3, ?4, ?5)
             on conflict(ts_code, period, adjust) do update set
                 last_known_date = excluded.last_known_date,
                 fetched_at = excluded.fetched_at",
            params![ts_code, period, adjust, last_known_date, fetched_at],
        )
        .map_err(|err| format!("kline_meta upsert 失败：{err}"))?;
    Ok(())
}

// ===== 分钟 K 缓存 ==========================================================

pub struct MinuteKlineRow {
    pub ts_code: String,
    pub period: String, // 1m / 5m / 15m / 30m / 60m
    pub timestamp_ms: i64,
    pub open: f64,
    pub close: f64,
    pub high: f64,
    pub low: f64,
    pub volume: i64,
    pub amount: f64,
    pub source: String,
}

pub struct MinuteKlineMetaRow {
    pub last_known_ts: i64,
    pub fetched_at: String,
}

pub fn upsert_minute_klines(app: &AppHandle, rows: &[MinuteKlineRow]) -> Result<usize, String> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut connection = open_database(app)?;
    migrate(&connection)?;
    let count = rows.len();
    let tx = connection
        .transaction()
        .map_err(|err| format!("开启 minute_klines 事务失败：{err}"))?;
    {
        let mut stmt = tx
            .prepare(
                "insert into minute_klines
                   (ts_code, period, timestamp_ms, open, close, high, low, volume, amount, source)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 on conflict(ts_code, period, timestamp_ms) do update set
                     open = excluded.open,
                     close = excluded.close,
                     high = excluded.high,
                     low = excluded.low,
                     volume = excluded.volume,
                     amount = excluded.amount,
                     source = excluded.source",
            )
            .map_err(|err| format!("准备 minute_klines upsert 失败：{err}"))?;
        for row in rows {
            stmt.execute(params![
                row.ts_code,
                row.period,
                row.timestamp_ms,
                row.open,
                row.close,
                row.high,
                row.low,
                row.volume,
                row.amount,
                row.source,
            ])
            .map_err(|err| {
                format!(
                    "写 minute_kline {}:{}:{} 失败：{err}",
                    row.ts_code, row.period, row.timestamp_ms
                )
            })?;
        }
    }
    tx.commit()
        .map_err(|err| format!("提交 minute_klines 事务失败：{err}"))?;
    Ok(count)
}

/// 拿最近 N 根分钟 K（按 timestamp 升序返回）。
pub fn find_minute_klines(
    app: &AppHandle,
    ts_code: &str,
    period: &str,
    limit: usize,
) -> Result<Vec<MinuteKlineRow>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let mut stmt = connection
        .prepare(
            "select ts_code, period, timestamp_ms, open, close, high, low, volume, amount, source
             from (
                 select * from minute_klines
                 where ts_code = ?1 and period = ?2
                 order by timestamp_ms desc
                 limit ?3
             )
             order by timestamp_ms asc",
        )
        .map_err(|err| format!("准备 minute_klines 查询失败：{err}"))?;
    let rows: Vec<MinuteKlineRow> = stmt
        .query_map(params![ts_code, period, limit as i64], |row| {
            Ok(MinuteKlineRow {
                ts_code: row.get(0)?,
                period: row.get(1)?,
                timestamp_ms: row.get(2)?,
                open: row.get(3)?,
                close: row.get(4)?,
                high: row.get(5)?,
                low: row.get(6)?,
                volume: row.get(7)?,
                amount: row.get(8)?,
                source: row.get(9)?,
            })
        })
        .map_err(|err| format!("minute_klines 查询失败：{err}"))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn get_minute_kline_meta(
    app: &AppHandle,
    ts_code: &str,
    period: &str,
) -> Result<Option<MinuteKlineMetaRow>, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .query_row(
            "select last_known_ts, fetched_at from minute_kline_meta
             where ts_code = ?1 and period = ?2",
            params![ts_code, period],
            |row| {
                Ok(MinuteKlineMetaRow {
                    last_known_ts: row.get(0)?,
                    fetched_at: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(|err| format!("minute_kline_meta 查询失败：{err}"))
}

pub fn upsert_minute_kline_meta(
    app: &AppHandle,
    ts_code: &str,
    period: &str,
    last_known_ts: i64,
    fetched_at: &str,
) -> Result<(), String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    connection
        .execute(
            "insert into minute_kline_meta (ts_code, period, last_known_ts, fetched_at)
             values (?1, ?2, ?3, ?4)
             on conflict(ts_code, period) do update set
                 last_known_ts = excluded.last_known_ts,
                 fetched_at = excluded.fetched_at",
            params![ts_code, period, last_known_ts, fetched_at],
        )
        .map_err(|err| format!("minute_kline_meta upsert 失败：{err}"))?;
    Ok(())
}
