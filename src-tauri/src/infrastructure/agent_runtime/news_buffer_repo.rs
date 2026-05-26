//! agent_news_buffer —— news_analysis 待分析 buffer。

use crate::infrastructure::db::helpers::now;
use crate::infrastructure::db::{migrate, open_database};
use rusqlite::{params, Connection};
use tauri::AppHandle;

fn conn(app: &AppHandle) -> Result<Connection, String> {
    let c = open_database(app)?;
    migrate(&c)?;
    Ok(c)
}

/// 把若干 news_id 入队（pending）；已存在的不重复写。
/// 返回本次新增的条数。
pub fn enqueue_pending(
    app: &AppHandle,
    news_ids: &[String],
    source_batch_id: &str,
) -> Result<u64, String> {
    let c = conn(app)?;
    let ts = now();
    let mut added: u64 = 0;
    for id in news_ids {
        let affected = c
            .execute(
                "insert into agent_news_buffer(
                    news_id, source_batch_id, status, run_id,
                    entered_at, updated_at, retry_count, next_retry_at, last_error
                 ) values (?1, ?2, 'pending', NULL, ?3, ?3, 0, NULL, NULL)
                 on conflict(news_id) do nothing",
                params![id, source_batch_id, ts],
            )
            .map_err(|e| format!("enqueue news buffer 失败：{e}"))?;
        added += affected as u64;
    }
    Ok(added)
}

pub fn count_pending(app: &AppHandle) -> Result<i64, String> {
    let c = conn(app)?;
    let n: i64 = c
        .query_row(
            "select count(*) from agent_news_buffer where status = 'pending'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| format!("count 失败：{e}"))?;
    Ok(n)
}

pub fn oldest_pending_age_secs(app: &AppHandle) -> Result<Option<i64>, String> {
    let c = conn(app)?;
    let row: Option<String> = c
        .query_row(
            "select entered_at from agent_news_buffer
             where status = 'pending'
             order by entered_at asc limit 1",
            [],
            |r| r.get(0),
        )
        .ok();
    let Some(entered_at) = row else {
        return Ok(None);
    };
    let parsed = chrono::DateTime::parse_from_rfc3339(&entered_at)
        .map(|t| t.with_timezone(&chrono::Utc))
        .map_err(|e| format!("解析 entered_at 失败：{e}"))?;
    Ok(Some((chrono::Utc::now() - parsed).num_seconds().max(0)))
}

/// 取出最早的若干 pending news_id 并整体改为 in_batch（绑定 run_id）。
pub fn checkout_batch(
    app: &AppHandle,
    run_id: &str,
    limit: i64,
) -> Result<Vec<String>, String> {
    let c = conn(app)?;
    let mut stmt = c
        .prepare(
            "select news_id from agent_news_buffer
             where status = 'pending'
             order by entered_at asc limit ?1",
        )
        .map_err(|e| format!("prepare 失败：{e}"))?;
    let mut rows = stmt.query(params![limit]).map_err(|e| format!("query 失败：{e}"))?;
    let mut ids = Vec::new();
    while let Some(r) = rows.next().map_err(|e| format!("next 失败：{e}"))? {
        ids.push(r.get::<_, String>(0).map_err(|e| e.to_string())?);
    }
    let ts = now();
    for id in &ids {
        c.execute(
            "update agent_news_buffer
             set status = 'in_batch', run_id = ?2, updated_at = ?3
             where news_id = ?1",
            params![id, run_id, ts],
        )
        .map_err(|e| format!("update in_batch 失败：{e}"))?;
    }
    Ok(ids)
}

pub fn mark_consumed(app: &AppHandle, news_ids: &[String]) -> Result<(), String> {
    let c = conn(app)?;
    let ts = now();
    for id in news_ids {
        c.execute(
            "update agent_news_buffer
             set status = 'consumed', updated_at = ?2
             where news_id = ?1",
            params![id, ts],
        )
        .map_err(|e| format!("mark_consumed 失败：{e}"))?;
    }
    Ok(())
}

/// spec §8 buffer item 失败处理：
/// - retryable 且 retry_count < max_retries → 回 pending、retry_count++、set next_retry_at
/// - 否则 → 终态 failed
///
/// 调用方在 run 完成时若 batch 对应 ids 处理失败，调本函数推进状态机。
#[allow(dead_code)] // spec §8 失败处理 API；现 hook 在 run 失败路径中接入
pub fn mark_failed(
    app: &AppHandle,
    news_ids: &[String],
    error: &str,
    retryable: bool,
    max_retries: i64,
    retry_delay_secs: i64,
) -> Result<(u64, u64), String> {
    let c = conn(app)?;
    let ts = now();
    let mut requeued = 0u64;
    let mut terminal = 0u64;
    for id in news_ids {
        // 读 retry_count
        let rc: i64 = c
            .query_row(
                "select retry_count from agent_news_buffer where news_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if retryable && rc < max_retries {
            let next = (chrono::Utc::now()
                + chrono::Duration::seconds(retry_delay_secs))
            .to_rfc3339();
            c.execute(
                "update agent_news_buffer
                 set status='pending', retry_count=retry_count+1,
                     next_retry_at=?2, last_error=?3, updated_at=?4
                 where news_id=?1",
                params![id, next, error, ts],
            )
            .map_err(|e| format!("mark_failed requeue 失败：{e}"))?;
            requeued += 1;
        } else {
            c.execute(
                "update agent_news_buffer
                 set status='failed', last_error=?2, updated_at=?3
                 where news_id=?1",
                params![id, error, ts],
            )
            .map_err(|e| format!("mark_failed terminal 失败：{e}"))?;
            terminal += 1;
        }
    }
    Ok((requeued, terminal))
}

/// spec §8 显式忽略：终态 ignored，不再 retry。例如 dedup 后或政策决定不分析。
#[allow(dead_code)]
pub fn mark_ignored(app: &AppHandle, news_ids: &[String], reason: &str) -> Result<u64, String> {
    let c = conn(app)?;
    let ts = now();
    let mut count = 0u64;
    for id in news_ids {
        let n = c
            .execute(
                "update agent_news_buffer
                 set status='ignored', last_error=?2, updated_at=?3
                 where news_id=?1 and status not in ('consumed','ignored')",
                params![id, reason, ts],
            )
            .map_err(|e| format!("mark_ignored 失败：{e}"))?;
        count += n as u64;
    }
    Ok(count)
}

/// 启动恢复：把 status=in_batch 但 run 不再存在 / 失败的 item 回到 pending。
pub fn recover_orphans(app: &AppHandle) -> Result<u64, String> {
    let c = conn(app)?;
    let n = c
        .execute(
            "update agent_news_buffer
             set status = 'pending',
                 retry_count = retry_count + 1,
                 last_error = 'interrupted_by_restart',
                 updated_at = ?1
             where status = 'in_batch'
               and (run_id is null
                    or run_id not in (select run_id from agent_runs where status in ('queued','running')))",
            params![now()],
        )
        .map_err(|e| format!("recover_orphans 失败：{e}"))?;
    Ok(n as u64)
}
