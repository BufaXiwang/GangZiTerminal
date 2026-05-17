//! 今日市场页用的 Tauri commands——全市场列表 + 行情快照。
//!
//! - `list_market_instruments`：拉 stocks + indexes + funds 三表合并，给前端列表
//! - `run_market_quote_refresh`：手动触发全市场旁路刷新
//!
//! 全市场实时行情通过 event `market-quotes-refreshed` 推送（在 pipeline 里 emit），
//! 这里 IPC 只做"静态列表"和"手动触发"。

use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketInstrumentDto {
    pub ts_code: String,
    pub code: String,
    pub name: String,
    pub category: &'static str, // "stock" | "index" | "fund"
    pub sector: Option<String>,
}

/// 拉全市场标的列表（一次性返回所有）。
/// 数据来自本地 stocks / indexes / funds 表——首次启动若表空，scheduler 会拉，
/// 返回空数组也是合法的（前端展示"档案刷新中"提示）。
#[tauri::command]
pub async fn list_market_instruments(
    app: tauri::AppHandle,
) -> Result<Vec<MarketInstrumentDto>, String> {
    let mut all: Vec<MarketInstrumentDto> = Vec::with_capacity(7000);

    // stocks
    if let Ok(rows) = crate::infrastructure::quotes::repository::list_stocks(&app) {
        for r in rows {
            let suffix = match r.market.as_str() {
                "sh" => "SH",
                "sz" => "SZ",
                "bj" => "BJ",
                _ => "SZ",
            };
            all.push(MarketInstrumentDto {
                ts_code: format!("{}.{}", r.code, suffix),
                code: r.code,
                name: r.name,
                category: "stock",
                sector: r.sector,
            });
        }
    }

    // indexes
    if let Ok(rows) = crate::infrastructure::quotes::repository::list_indexes(&app) {
        for r in rows {
            all.push(MarketInstrumentDto {
                ts_code: r.ts_code,
                code: r.code,
                name: r.name,
                category: "index",
                // 指数 sector 用 publisher（"中证" / "上交所"）+ category 拼，给筛选用
                sector: match (r.publisher.as_deref(), r.category.as_deref()) {
                    (Some(p), Some(c)) => Some(format!("{p} · {c}")),
                    (Some(p), None) => Some(p.to_string()),
                    (None, Some(c)) => Some(c.to_string()),
                    _ => None,
                },
            });
        }
    }

    // funds (仅场内 ETF/LOF)
    if let Ok(rows) = crate::infrastructure::quotes::repository::list_listed_funds(&app) {
        for r in rows {
            all.push(MarketInstrumentDto {
                ts_code: r.ts_code,
                code: r.code,
                name: r.name,
                category: "fund",
                sector: r.fund_type,
            });
        }
    }

    Ok(all)
}
