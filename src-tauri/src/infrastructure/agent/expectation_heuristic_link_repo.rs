//! Expectation ↔ Heuristic 精确归因 link 表。
//!
//! 当 agent 创建 expectation 时显式声明"用了哪些 heuristic"——review 终态时
//! 按这张表精确给对应 heuristic 计数，取代之前"所有 agent_inferred + 有
//! supporting lessons 的 heuristic 都给计数"的粗暴聚合（会误伤无关 heuristic）。
//!
//! 表只有 (expectation_id, heuristic_id) 复合主键，无生命周期——expectation
//! 自身被删时由 SQL 级联清理（目前没删 expectation 的路径，留空即可）。

use crate::domain::account::expectation::ExpectationId;
use crate::domain::agent::heuristic::HeuristicId;
use crate::infrastructure::db::{migrate, open_database};
use rusqlite::params;
use tauri::AppHandle;

/// 一次性记录某 expectation 引用的全部 heuristic ids（幂等：重复 ignore）。
pub fn record(
    app: &AppHandle,
    expectation_id: &ExpectationId,
    heuristic_ids: &[HeuristicId],
) -> Result<(), String> {
    if heuristic_ids.is_empty() {
        return Ok(());
    }
    let mut conn = open_database(app)?;
    migrate(&conn)?;
    let tx = conn
        .transaction()
        .map_err(|err| format!("开启事务失败：{err}"))?;
    for hid in heuristic_ids {
        tx.execute(
            "insert or ignore into expectation_heuristic_links (expectation_id, heuristic_id)
             values (?1, ?2)",
            params![expectation_id.as_str(), hid.as_str()],
        )
        .map_err(|err| format!("写 expectation_heuristic_link 失败：{err}"))?;
    }
    tx.commit().map_err(|err| format!("提交事务失败：{err}"))?;
    Ok(())
}

/// 查某 expectation 关联的 heuristic ids——review 终态时调来精确给 hit/miss 计数。
pub fn list_for_expectation(
    app: &AppHandle,
    expectation_id: &ExpectationId,
) -> Result<Vec<HeuristicId>, String> {
    let conn = open_database(app)?;
    migrate(&conn)?;
    let mut stmt = conn
        .prepare(
            "select heuristic_id from expectation_heuristic_links
             where expectation_id = ?1 order by heuristic_id",
        )
        .map_err(|err| format!("准备 list_for_expectation 失败：{err}"))?;
    let rows = stmt
        .query_map(params![expectation_id.as_str()], |row| {
            let s: String = row.get(0)?;
            Ok(HeuristicId::from_string(s))
        })
        .map_err(|err| format!("query 失败：{err}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| format!("collect 失败：{err}"))?;
    Ok(rows)
}
