//! News 批量状态管理——原子 claim / mark / watchdog。
//!
//! 并发安全的核心在 `claim_batch`：用 `UPDATE ... RETURNING` 单语句
//! 把"SELECT pending + 标 processing + 返回 ids"一步做完——SQLite 内部
//! 整体持写锁，两个 tick 即使同时触发也绝不会取到同一条 news。
//!
//! 状态机：
//!   pending     ─claim_batch──►  processing
//!   processing  ─mark_consumed──►  consumed
//!   processing  ─mark_failed───►  failed
//!   processing  ─reclaim_stale──►  pending   (watchdog 回收孤儿)
//!
//! `failed` 不自动重试——留作错误证据；想重试由用户/agent 主动调 `mark_failed_as_pending`。

use crate::domain::news::NewsId;
use crate::infrastructure::db::{migrate, open_database};
use rusqlite::params;
use tauri::AppHandle;

/// 原子取 batch——把 m 条 pending news 标为 processing 并返回 ids。
///
/// 排序：按 `coalesce(published, created_at) asc`——先取最早的（FIFO），
/// 避免新 news 把老的挤掉永远不消费。
///
/// 并发保证：UPDATE 持 reserved → exclusive 写锁，子查询 SELECT 和 UPDATE
/// 在同语句执行。两个并发 claim_batch：第二个阻塞等第一个释放，看到的
/// pending 集合已经少了 m 条。
pub fn claim_batch(app: &AppHandle, m: usize) -> Result<Vec<NewsId>, String> {
    if m == 0 {
        return Ok(Vec::new());
    }
    let connection = open_database(app)?;
    migrate(&connection)?;
    let now_iso = chrono::Utc::now().to_rfc3339();
    let mut stmt = connection
        .prepare(
            "update news_items
                set analysis_status = 'processing',
                    processing_started_at = ?1,
                    updated_at = ?1
              where id in (
                select id from news_items
                 where analysis_status = 'pending'
                 order by coalesce(published, created_at) asc
                 limit ?2
              )
              returning id",
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

/// 标 failed——同上严格条件。当前仅留作"未来 agent 显式拒绝"接口；
/// listener 路径的瞬时失败走 `revert_processing_to_pending` 而非 mark_failed，
/// 防止 provider 超时 / 网络故障导致永久漏分析。
pub fn mark_failed(app: &AppHandle, ids: &[NewsId]) -> Result<usize, String> {
    update_status_strict(app, ids, "failed", "processing")
}

/// agent run 系统级失败时把 processing 退回 pending，让下次 batch 重试。
pub fn revert_processing_to_pending(app: &AppHandle, ids: &[NewsId]) -> Result<usize, String> {
    update_status_strict(app, ids, "pending", "processing")
}

/// 用户/agent 主动把 failed 重新挂回 pending。
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
    // next=pending 时清空 processing_started_at；其他 next 保留它作历史审计
    let sql = if next == "pending" {
        format!(
            "update news_items
                set analysis_status = ?1,
                    processing_started_at = null,
                    updated_at = ?2
              where analysis_status = ?3
                and id in ({placeholders})"
        )
    } else {
        format!(
            "update news_items
                set analysis_status = ?1,
                    updated_at = ?2
              where analysis_status = ?3
                and id in ({placeholders})"
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
        .map_err(|err| format!("更新资讯状态失败：{err}"))?;
    Ok(n)
}

/// Watchdog——processing 状态超过 cutoff 分钟未确认 → 回收为 pending。
///
/// 触发原因：agent run 挂（OOM / 网络）/ 进程崩溃 / 调用方忘了 mark。
/// 默认 cutoff=30min。
pub fn reclaim_stale_processing(app: &AppHandle, cutoff_minutes: i64) -> Result<u64, String> {
    let cutoff_at = (chrono::Utc::now() - chrono::Duration::minutes(cutoff_minutes)).to_rfc3339();
    let now_iso = chrono::Utc::now().to_rfc3339();
    let connection = open_database(app)?;
    migrate(&connection)?;
    let n = connection
        .execute(
            "update news_items
                set analysis_status = 'pending',
                    processing_started_at = null,
                    updated_at = ?1
              where analysis_status = 'processing'
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

/// 当前 pending 计数——给 batch_loop 做 buffer overflow 判定用。
pub fn count_pending(app: &AppHandle) -> Result<u64, String> {
    let connection = open_database(app)?;
    migrate(&connection)?;
    let n: i64 = connection
        .query_row(
            "select count(*) from news_items where analysis_status = 'pending'",
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

    fn insert_news(conn: &Connection, id: &str, status: &str, published: &str) {
        conn.execute(
            "insert into news_items
                (id, source, published, analysis_status, payload_json, created_at, updated_at)
             values (?1, 'test', ?2, ?3, '{}', ?2, ?2)",
            params![id, published, status],
        )
        .unwrap();
    }

    #[test]
    fn claim_batch_atomic_picks_pending_in_order() {
        let conn = open_test_db();
        insert_news(&conn, "n3", "pending", "2025-01-03T00:00:00Z");
        insert_news(&conn, "n1", "pending", "2025-01-01T00:00:00Z");
        insert_news(&conn, "n2", "pending", "2025-01-02T00:00:00Z");
        insert_news(&conn, "n4", "consumed", "2025-01-04T00:00:00Z"); // 已消费不动

        // 模拟 claim_batch SQL 直接执行
        let now_iso = chrono::Utc::now().to_rfc3339();
        let mut stmt = conn
            .prepare(
                "update news_items
                    set analysis_status = 'processing',
                        processing_started_at = ?1,
                        updated_at = ?1
                  where id in (
                    select id from news_items
                     where analysis_status = 'pending'
                     order by coalesce(published, created_at) asc
                     limit ?2
                  )
                  returning id",
            )
            .unwrap();
        let claimed: Vec<String> = stmt
            .query_map(params![now_iso, 2i64], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        // 应该取出 n1 + n2（最早两条）
        assert_eq!(claimed.len(), 2);
        assert!(claimed.contains(&"n1".to_string()));
        assert!(claimed.contains(&"n2".to_string()));

        // n3 仍 pending；n4 仍 consumed
        let n3_status: String = conn
            .query_row(
                "select analysis_status from news_items where id='n3'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n3_status, "pending");
        let n4_status: String = conn
            .query_row(
                "select analysis_status from news_items where id='n4'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n4_status, "consumed");
    }

    #[test]
    fn mark_consumed_only_affects_processing() {
        let conn = open_test_db();
        insert_news(&conn, "p1", "processing", "2025-01-01T00:00:00Z");
        insert_news(&conn, "p2", "pending", "2025-01-02T00:00:00Z");

        let now_iso = chrono::Utc::now().to_rfc3339();
        let n = conn
            .execute(
                "update news_items
                    set analysis_status = 'consumed',
                        updated_at = ?1
                  where analysis_status = 'processing'
                    and id in (?2, ?3)",
                params![now_iso, "p1", "p2"],
            )
            .unwrap();
        assert_eq!(n, 1); // 只有 p1 命中
    }

    #[test]
    fn reclaim_stale_only_old_processing() {
        let conn = open_test_db();
        // 35 分钟前进 processing，应该被回收
        let stale = (chrono::Utc::now() - chrono::Duration::minutes(35)).to_rfc3339();
        // 5 分钟前进 processing，不应该被回收
        let fresh = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        conn.execute(
            "insert into news_items
                (id, source, published, analysis_status, processing_started_at, payload_json, created_at, updated_at)
             values ('stale', 'test', '2025', 'processing', ?1, '{}', ?1, ?1),
                    ('fresh', 'test', '2025', 'processing', ?2, '{}', ?2, ?2)",
            params![stale, fresh],
        )
        .unwrap();
        let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(30)).to_rfc3339();
        let now_iso = chrono::Utc::now().to_rfc3339();
        let n = conn
            .execute(
                "update news_items
                    set analysis_status = 'pending',
                        processing_started_at = null,
                        updated_at = ?1
                  where analysis_status = 'processing'
                    and processing_started_at is not null
                    and processing_started_at < ?2",
                params![now_iso, cutoff],
            )
            .unwrap();
        assert_eq!(n, 1);
        let stale_status: String = conn
            .query_row(
                "select analysis_status from news_items where id='stale'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale_status, "pending");
        let fresh_status: String = conn
            .query_row(
                "select analysis_status from news_items where id='fresh'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fresh_status, "processing");
    }
}
