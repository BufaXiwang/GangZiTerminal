//! Quotes 子域参考表的 SQLite 读写——stocks / indexes / funds 三张静态档案表。
//!
//! 这些表由 TuShare scheduler 每天 08:30 北京时间盘前刷新一遍（见
//! `pipeline::stocks::refresh_universe`），其他时段只读。共 stocks 5500+ /
//! indexes 5900+ / funds 2000+，提供 agent 工具按代码 / 按名字解析的能力面。
//!
//! K 线缓存（按 ts_code 索引）在 `super::cache`，不在这里。

use crate::infrastructure::db::{migrate, now, open_database};
use rusqlite::{params, OptionalExtension};
use tauri::AppHandle;

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
