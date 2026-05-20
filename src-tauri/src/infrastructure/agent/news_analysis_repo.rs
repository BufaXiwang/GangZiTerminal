//! Agent 对 news 的分析状态机持久化——News BC 不感知本表。
//!
//! 表：`agent_news_analysis_state(news_id PK, status, processing_started_at, updated_at)`
//!
//! 并发安全的核心在 `claim_batch`：用 `INSERT ... SELECT LEFT JOIN news_items
//! ... ON CONFLICT DO UPDATE RETURNING` 单语句把"找出还没分析的 news + 注册
//! 为 processing + 返回 ids"一步做完。SQLite 内部整体持写锁，并发安全。
//!
//! `failed` 不自动重试——留作错误证据；想重试由用户/agent 主动调
//! `revert_failed_to_pending`。

use crate::domain::news::NewsId;
use crate::infrastructure::db::{migrate, open_database};
use rusqlite::params;
use tauri::AppHandle;

/// 原子取 batch——把 m 条 pending（或还没注册过状态）的 news 标为 processing 并返回 ids。
///
/// 排序：按 news_items.published asc（FIFO），避免老 news 被挤掉永远不消费。
///
/// 并发保证：单语句 INSERT...ON CONFLICT 持 reserved → exclusive 写锁，
/// 子查询 LEFT JOIN 和 INSERT 在同语句执行。两个并发 claim_batch：第二个
/// 阻塞等第一个释放，看到的可 claim 集合已经少了 m 条。
pub fn claim_batch(app: &AppHandle, m: usize) -> Result<Vec<NewsId>, String> {
    if m == 0 {
        return Ok(Vec::new());
    }
    let connection = open_database(app)?;
    migrate(&connection)?;
    let now_iso = chrono::Utc::now().to_rfc3339();
    let mut stmt = connection
        .prepare(
            "insert into agent_news_analysis_state
                (news_id, status, processing_started_at, updated_at)
             select ni.id, 'processing', ?1, ?1
               from news_items ni
               left join agent_news_analysis_state nas on nas.news_id = ni.id
              where nas.news_id is null or nas.status = 'pending'
              order by coalesce(ni.published, ni.created_at) asc
              limit ?2
              on conflict(news_id) do update set
                status = 'processing',
                processing_started_at = excluded.processing_started_at,
                updated_at = excluded.updated_at
             returning news_id",
        )
        .map_err(|err| format!("准备 claim_batch 失败：{err}"))?;
    let ids: Vec<NewsId> = stmt
        .query_map(params![now_iso, m as i64], |row| {
            let id: String = row.get(0)?;
            Ok(NewsId::new(id))
        })
        .map_err(|err| format!("执行 claim_batch 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("读取 claim_batch 行失败：{err}"))?;
    Ok(ids)
}

/// 标 consumed——只对当前 status='processing' 的生效（防止 stale agent run
/// 已被 watchdog 回收后再来 mark）。
pub fn mark_consumed(app: &AppHandle, ids: &[NewsId]) -> Result<usize, String> {
    update_status_strict(app, ids, "consumed", "processing")
}

/// 标 failed——当前仅留作"未来 agent 显式拒绝"接口；listener 路径的瞬时
/// 失败走 `revert_processing_to_pending` 而非 mark_failed，防止 provider
/// 超时 / 网络故障导致永久漏分析。
#[allow(dead_code)]
pub fn mark_failed(app: &AppHandle, ids: &[NewsId]) -> Result<usize, String> {
    update_status_strict(app, ids, "failed", "processing")
}

/// agent run 系统级失败时把 processing 退回 pending，让下次 batch 重试。
pub fn revert_processing_to_pending(app: &AppHandle, ids: &[NewsId]) -> Result<usize, String> {
    update_status_strict(app, ids, "pending", "processing")
}

/// 用户/agent 主动把 failed 重新挂回 pending。
#[allow(dead_code)]
pub fn revert_failed_to_pending(app: &AppHandle, ids: &[NewsId]) -> Result<usize, String> {
    update_status_strict(app, ids, "pending", "failed")
}

fn update_status_strict(
    app: &AppHandle,
    ids: &[NewsId],
    next: &str,
    expect_current: &str,
) -> Result<usize, String> {
    if ids.is_empty() {
        return Ok(0);
    }
    let connection = open_database(app)?;
    migrate(&connection)?;
    let now_iso = chrono::Utc::now().to_rfc3339();
    let placeholders = (0..ids.len()).map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = if next == "pending" {
        format!(
            "update agent_news_analysis_state
                set status = ?1,
                    processing_started_at = null,
                    updated_at = ?2
              where status = ?3
                and news_id in ({placeholders})"
        )
    } else {
        format!(
            "update agent_news_analysis_state
                set status = ?1,
                    updated_at = ?2
              where status = ?3
                and news_id in ({placeholders})"
        )
    };
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(ids.len() + 3);
    params_vec.push(Box::new(next.to_string()) as Box<dyn rusqlite::ToSql>);
    params_vec.push(Box::new(now_iso) as Box<dyn rusqlite::ToSql>);
    params_vec.push(Box::new(expect_current.to_string()) as Box<dyn rusqlite::ToSql>);
    for id in ids {
        params_vec.push(Box::new(id.as_str().to_string()) as Box<dyn rusqlite::ToSql>);
    }
    let n = connection
        .execute(
            &sql,
            rusqlite::params_from_iter(params_vec.iter().map(|b| b.as_ref())),
        )
        .map_err(|err| format!("更新 news analysis 状态失败：{err}"))?;
    Ok(n)
}

/// Watchdog——processing 状态超过 cutoff 分钟未确认 → 回收为 pending。
///
/// 触发原因：agent run 挂（OOM / 网络）/ 进程崩溃 / 调用方忘了 mark。
pub fn reclaim_stale_processing(app: &AppHandle, cutoff_minutes: i64) -> Result<u64, String> {
    let cutoff_at = (chrono::Utc::now() - chrono::Duration::minutes(cutoff_minutes)).to_rfc3339();
    let now_iso = chrono::Utc::now().to_rfc3339();
    let connection = open_database(app)?;
    migrate(&connection)?;
    let n = connection
        .execute(
            "update agent_news_analysis_state
                set status = 'pending',
                    processing_started_at = null,
                    updated_at = ?1
              where status = 'processing'
                and processing_started_at is not null
                and processing_started_at < ?2",
            params![now_iso, cutoff_at],
        )
        .map_err(|err| format!("reclaim_stale_processing 失败：{err}"))?;
    if n > 0 {
        tracing::info!(
            reclaimed = n,
            cutoff_min = cutoff_minutes,
            "watchdog 回收 stale processing news"
        );
    }
    Ok(n as u64)
}

/// 当前 pending 计数（包含还没注册过 state 的新 news_items）——给 batch_loop
/// 做 buffer overflow 判定用。
pub fn count_pending(app: &AppHandle) -> Result<u64, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let n: i64 = connection
        .query_row(
            "select count(*)
               from news_items ni
               left join agent_news_analysis_state nas on nas.news_id = ni.id
              where nas.news_id is null or nas.status = 'pending'",
            [],
            |row| row.get(0),
        )
        .map_err(|err| format!("count_pending 失败：{err}"))?;
    Ok(n.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::infrastructure::db::migrations::migrate(&conn).unwrap();
        conn
    }

    fn insert_news(conn: &Connection, id: &str, published: &str) {
        conn.execute(
            "insert into news_items
                (id, source, published, payload_json, created_at, updated_at)
             values (?1, 'test', ?2, '{}', ?2, ?2)",
            params![id, published],
        )
        .unwrap();
    }

    #[test]
    fn claim_batch_picks_earliest_uncovered_news() {
        let conn = open_test_db();
        insert_news(&conn, "n3", "2025-01-03T00:00:00Z");
        insert_news(&conn, "n1", "2025-01-01T00:00:00Z");
        insert_news(&conn, "n2", "2025-01-02T00:00:00Z");
        // n4 已经被 consumed（不该再 claim）
        insert_news(&conn, "n4", "2025-01-04T00:00:00Z");
        conn.execute(
            "insert into agent_news_analysis_state (news_id, status, updated_at)
             values ('n4', 'consumed', '2025-01-04')",
            [],
        )
        .unwrap();

        let now_iso = chrono::Utc::now().to_rfc3339();
        let mut stmt = conn
            .prepare(
                "insert into agent_news_analysis_state
                    (news_id, status, processing_started_at, updated_at)
                 select ni.id, 'processing', ?1, ?1
                   from news_items ni
                   left join agent_news_analysis_state nas on nas.news_id = ni.id
                  where nas.news_id is null or nas.status = 'pending'
                  order by coalesce(ni.published, ni.created_at) asc
                  limit ?2
                  on conflict(news_id) do update set
                    status = 'processing',
                    processing_started_at = excluded.processing_started_at,
                    updated_at = excluded.updated_at
                 returning news_id",
            )
            .unwrap();
        let claimed: Vec<String> = stmt
            .query_map(params![now_iso, 2i64], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        // 应该取出 n1 + n2（最早两条 + 不动 n4 consumed）
        assert_eq!(claimed.len(), 2);
        assert!(claimed.contains(&"n1".to_string()));
        assert!(claimed.contains(&"n2".to_string()));

        let n4_status: String = conn
            .query_row(
                "select status from agent_news_analysis_state where news_id='n4'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n4_status, "consumed");
    }

    #[test]
    fn reclaim_stale_only_old_processing() {
        let conn = open_test_db();
        insert_news(&conn, "stale", "2025-01-01");
        insert_news(&conn, "fresh", "2025-01-02");
        let stale = (chrono::Utc::now() - chrono::Duration::minutes(35)).to_rfc3339();
        let fresh = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        conn.execute(
            "insert into agent_news_analysis_state
                (news_id, status, processing_started_at, updated_at)
             values ('stale', 'processing', ?1, ?1),
                    ('fresh', 'processing', ?2, ?2)",
            params![stale, fresh],
        )
        .unwrap();
        let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(30)).to_rfc3339();
        let now_iso = chrono::Utc::now().to_rfc3339();
        let n = conn
            .execute(
                "update agent_news_analysis_state
                    set status = 'pending',
                        processing_started_at = null,
                        updated_at = ?1
                  where status = 'processing'
                    and processing_started_at is not null
                    and processing_started_at < ?2",
                params![now_iso, cutoff],
            )
            .unwrap();
        assert_eq!(n, 1);
        let stale_status: String = conn
            .query_row(
                "select status from agent_news_analysis_state where news_id='stale'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale_status, "pending");
    }
}
