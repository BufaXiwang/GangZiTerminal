//! TuShare 能力探测——遍历常用接口测试 token 实际能拿到什么。
//!
//! 用途：
//! - 一次性盘点用户 token 在 2000 / 5000 / 8000 档对各接口的支持情况
//! - 生成 docs/tushare-capabilities.md 的真实数据
//!
//! 触发：
//! - IPC `probe_tushare_capabilities`（前端 / chrome devtools invoke）
//! - 结果写入 app data dir 下 `tushare-probe-result.json`
//! - 同时 tracing::info! 输出便于日志抓取
//!
//! 设计：
//! - 每个接口只发一次轻量请求（默认带最小限定参数）
//! - 串行执行 + 每次 sleep 80ms，规避 TuShare 单分钟频率限制
//! - 任一接口失败不影响后续（独立 try）

use super::client::call;
use crate::domain::quotes::QuotesError;
use serde::Serialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tauri::AppHandle;

/// 一个接口的探测结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    /// 接口名（TuShare api_name）
    pub api: &'static str,
    /// 分组：stocks / indexes / funds / financials / flow / events / sectors / global / derivatives / macro / realtime / calendar
    pub category: &'static str,
    /// 接口能力描述
    pub label: &'static str,
    /// 探测结果
    pub status: ProbeStatus,
    /// 返回行数（仅 status=Ok 时有意义）
    pub rows: usize,
    /// 错误明细
    pub error: Option<String>,
    /// 耗时（毫秒）
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    /// 接口返成功并有数据
    Ok,
    /// 接口返成功但 0 行（接口本身可调用，但当前参数没数据）
    Empty,
    /// 40203 / 40211 频率超限
    RateLimited,
    /// 40202 / 40219 积分不足
    QuotaExceeded,
    /// 接口不存在 / 字段不对
    BadRequest,
    /// 网络层错误
    Network,
    /// 其它响应解析错
    DecodeError,
    /// token 未配
    MissingToken,
}

/// 接口清单——每条 (api, category, label, params, fields)。
///
/// 参数选择策略：
/// - 全市场档案类（stock_basic / fund_basic / hk_basic 等）：用最小过滤参数
/// - 单标的类（daily / income / fina_indicator 等）：用 000001.SZ 平安银行
/// - 单日类（top_list / suspend_d / margin 等）：用近期一个交易日
/// - 区间类（moneyflow_hsgt 等）：start/end 各填一个
type Probe = (
    &'static str,
    &'static str,
    &'static str,
    fn() -> Value,
    &'static str,
);

fn recent_trade_date() -> String {
    // 取 7 天前的工作日（避免今天数据还没落库）
    let beijing = chrono::Utc::now() + chrono::Duration::hours(8) - chrono::Duration::days(7);
    beijing.format("%Y%m%d").to_string()
}

fn probes() -> Vec<Probe> {
    vec![
        // ========== 股票 ==========
        (
            "stock_basic",
            "stocks",
            "全 A 股档案",
            || json!({"list_status": "L"}),
            "ts_code,name,industry",
        ),
        (
            "daily",
            "stocks",
            "个股日 K",
            || json!({"ts_code": "000001.SZ"}),
            "trade_date,open,close",
        ),
        (
            "weekly",
            "stocks",
            "个股周 K",
            || json!({"ts_code": "000001.SZ"}),
            "trade_date,open,close",
        ),
        (
            "monthly",
            "stocks",
            "个股月 K",
            || json!({"ts_code": "000001.SZ"}),
            "trade_date,open,close",
        ),
        (
            "adj_factor",
            "stocks",
            "复权因子",
            || json!({"ts_code": "000001.SZ"}),
            "trade_date,adj_factor",
        ),
        (
            "daily_basic",
            "stocks",
            "PE/PB/换手/市值",
            || json!({"trade_date": recent_trade_date()}),
            "ts_code,pe,pb,total_mv",
        ),
        (
            "stk_factor",
            "stocks",
            "技术因子（MA/MACD/KDJ/RSI 已算好）",
            || json!({"ts_code": "000001.SZ"}),
            "trade_date,macd,kdj_k,rsi_6",
        ),
        (
            "stk_mins",
            "stocks",
            "历史分钟 K（5000+）",
            || json!({"ts_code": "000001.SZ", "freq": "5min", "start_date": "2026-05-10 09:00:00", "end_date": "2026-05-10 15:00:00"}),
            "trade_time,open,close",
        ),
        (
            "realtime_quote",
            "stocks",
            "实时报价（5000+）",
            || json!({"ts_code": "000001.SZ"}),
            "ts_code,price",
        ),
        // ========== 指数 ==========
        (
            "index_basic",
            "indexes",
            "指数档案",
            || json!({"market": "SSE"}),
            "ts_code,name",
        ),
        (
            "index_daily",
            "indexes",
            "指数日 K",
            || json!({"ts_code": "000001.SH"}),
            "trade_date,close",
        ),
        (
            "index_weekly",
            "indexes",
            "指数周 K",
            || json!({"ts_code": "000001.SH"}),
            "trade_date,close",
        ),
        (
            "index_monthly",
            "indexes",
            "指数月 K",
            || json!({"ts_code": "000001.SH"}),
            "trade_date,close",
        ),
        (
            "index_dailybasic",
            "indexes",
            "指数 PE/PB/股息率",
            || json!({"trade_date": recent_trade_date()}),
            "ts_code,pe,pb",
        ),
        (
            "index_weight",
            "indexes",
            "成份股权重",
            || json!({"index_code": "000300.SH", "trade_date": recent_trade_date()}),
            "con_code,weight",
        ),
        // ========== 基金 ==========
        (
            "fund_basic",
            "funds",
            "基金档案",
            || json!({"market": "E"}),
            "ts_code,name,fund_type",
        ),
        (
            "fund_daily",
            "funds",
            "场内基金日 K",
            || json!({"ts_code": "510300.SH"}),
            "trade_date,close",
        ),
        (
            "fund_nav",
            "funds",
            "基金净值",
            || json!({"ts_code": "510300.SH"}),
            "ann_date,unit_nav",
        ),
        (
            "fund_portfolio",
            "funds",
            "持仓明细",
            || json!({"ts_code": "510300.SH"}),
            "symbol,mkv,amount",
        ),
        // ========== 财务报表 ==========
        (
            "income",
            "financials",
            "利润表",
            || json!({"ts_code": "000001.SZ"}),
            "end_date,revenue,n_income",
        ),
        (
            "balancesheet",
            "financials",
            "资产负债表",
            || json!({"ts_code": "000001.SZ"}),
            "end_date,total_assets",
        ),
        (
            "cashflow",
            "financials",
            "现金流量表",
            || json!({"ts_code": "000001.SZ"}),
            "end_date,n_cashflow_act",
        ),
        (
            "forecast",
            "financials",
            "业绩预告",
            || json!({"ts_code": "000001.SZ"}),
            "end_date,p_change_min,p_change_max",
        ),
        (
            "express",
            "financials",
            "业绩快报",
            || json!({"ts_code": "000001.SZ"}),
            "end_date,revenue,n_income",
        ),
        (
            "fina_indicator",
            "financials",
            "财务指标（ROE/毛利率/EPS）",
            || json!({"ts_code": "000001.SZ"}),
            "end_date,roe,eps",
        ),
        (
            "fina_mainbz",
            "financials",
            "主营业务构成",
            || json!({"ts_code": "000001.SZ"}),
            "bz_item,bz_sales",
        ),
        (
            "disclosure_date",
            "financials",
            "财报披露日历",
            || json!({"ts_code": "000001.SZ"}),
            "ts_code,ann_date,end_date",
        ),
        // ========== 资金面 / 龙虎榜 ==========
        (
            "top_list",
            "flow",
            "龙虎榜每日",
            || json!({"trade_date": recent_trade_date()}),
            "ts_code,name,close",
        ),
        (
            "top_inst",
            "flow",
            "龙虎榜机构席位",
            || json!({"trade_date": recent_trade_date()}),
            "ts_code,exalter,buy",
        ),
        (
            "moneyflow",
            "flow",
            "个股资金流",
            || json!({"ts_code": "000001.SZ"}),
            "trade_date,buy_sm_vol",
        ),
        (
            "moneyflow_hsgt",
            "flow",
            "沪深港通资金",
            || json!({"start_date": recent_trade_date(), "end_date": recent_trade_date()}),
            "trade_date,ggt_ss,ggt_sz",
        ),
        (
            "hsgt_top10",
            "flow",
            "北向 TOP10",
            || json!({"trade_date": recent_trade_date()}),
            "ts_code,name,amount",
        ),
        (
            "ggt_top10",
            "flow",
            "港股通 TOP10",
            || json!({"trade_date": recent_trade_date()}),
            "ts_code,name,amount",
        ),
        (
            "margin",
            "flow",
            "融资融券汇总",
            || json!({"trade_date": recent_trade_date()}),
            "exchange_id,rzye",
        ),
        (
            "margin_detail",
            "flow",
            "融资融券明细",
            || json!({"ts_code": "000001.SZ"}),
            "trade_date,rzye",
        ),
        (
            "repurchase",
            "flow",
            "股票回购",
            || json!({}),
            "ts_code,ann_date",
        ),
        (
            "block_trade",
            "flow",
            "大宗交易",
            || json!({"trade_date": recent_trade_date()}),
            "ts_code,price,vol",
        ),
        (
            "share_float",
            "flow",
            "限售解禁",
            || json!({"ts_code": "000001.SZ"}),
            "ts_code,ann_date,float_date",
        ),
        (
            "stk_holdernumber",
            "flow",
            "股东户数",
            || json!({"ts_code": "000001.SZ"}),
            "ts_code,ann_date,holder_num",
        ),
        (
            "top10_holders",
            "flow",
            "前十大股东",
            || json!({"ts_code": "000001.SZ"}),
            "holder_name,hold_ratio",
        ),
        (
            "top10_floatholders",
            "flow",
            "前十大流通股东",
            || json!({"ts_code": "000001.SZ"}),
            "holder_name,hold_ratio",
        ),
        // ========== 公司动作 ==========
        (
            "dividend",
            "events",
            "分红送股",
            || json!({"ts_code": "000001.SZ"}),
            "end_date,div_proc",
        ),
        (
            "suspend_d",
            "events",
            "停复牌",
            || json!({"trade_date": recent_trade_date()}),
            "ts_code,suspend_type",
        ),
        (
            "namechange",
            "events",
            "股票曾用名",
            || json!({"ts_code": "000001.SZ"}),
            "name,start_date",
        ),
        (
            "stk_managers",
            "events",
            "公司高管",
            || json!({"ts_code": "000001.SZ"}),
            "name,title",
        ),
        (
            "stk_rewards",
            "events",
            "高管薪酬",
            || json!({"ts_code": "000001.SZ"}),
            "name,reward",
        ),
        (
            "new_share",
            "events",
            "IPO 新股",
            || json!({}),
            "ts_code,sub_code,name",
        ),
        (
            "hs_const",
            "events",
            "沪深股通成份",
            || json!({"hs_type": "SH"}),
            "ts_code,in_date",
        ),
        // ========== 板块 ==========
        (
            "concept",
            "sectors",
            "同花顺概念分类",
            || json!({"src": "ts"}),
            "code,name",
        ),
        (
            "concept_detail",
            "sectors",
            "概念成份股",
            || json!({"id": "TS0"}),
            "ts_code,name",
        ),
        (
            "index_classify",
            "sectors",
            "申万行业分类",
            || json!({"level": "L1"}),
            "index_code,industry_name",
        ),
        // ========== 港股 / 美股 ==========
        (
            "hk_basic",
            "global",
            "港股档案",
            || json!({}),
            "ts_code,name",
        ),
        (
            "hk_daily",
            "global",
            "港股日 K",
            || json!({"trade_date": recent_trade_date()}),
            "ts_code,close",
        ),
        (
            "us_basic",
            "global",
            "美股档案",
            || json!({}),
            "ts_code,name",
        ),
        (
            "us_daily",
            "global",
            "美股日 K",
            || json!({"trade_date": recent_trade_date()}),
            "ts_code,close",
        ),
        // ========== 可转债 / 期货 / 期权 ==========
        (
            "cb_basic",
            "derivatives",
            "可转债档案",
            || json!({}),
            "ts_code,name",
        ),
        (
            "cb_daily",
            "derivatives",
            "可转债日 K",
            || json!({"trade_date": recent_trade_date()}),
            "ts_code,close",
        ),
        (
            "fut_basic",
            "derivatives",
            "期货合约",
            || json!({"exchange": "DCE"}),
            "ts_code,name",
        ),
        (
            "fut_daily",
            "derivatives",
            "期货日 K",
            || json!({"trade_date": recent_trade_date()}),
            "ts_code,close",
        ),
        (
            "opt_basic",
            "derivatives",
            "期权合约",
            || json!({}),
            "ts_code,name",
        ),
        (
            "opt_daily",
            "derivatives",
            "期权日 K",
            || json!({"trade_date": recent_trade_date()}),
            "ts_code,close",
        ),
        // ========== 宏观 ==========
        (
            "shibor",
            "macro",
            "Shibor 利率",
            || json!({"start_date": "20250101", "end_date": recent_trade_date()}),
            "date,on,1w",
        ),
        (
            "cn_gdp",
            "macro",
            "GDP",
            || json!({"start_q": "2024Q1", "end_q": "2024Q4"}),
            "quarter,gdp",
        ),
        (
            "cn_cpi",
            "macro",
            "CPI",
            || json!({"start_m": "202401", "end_m": "202412"}),
            "month,nt_val",
        ),
        (
            "cn_ppi",
            "macro",
            "PPI",
            || json!({"start_m": "202401", "end_m": "202412"}),
            "month,ppi_yoy",
        ),
        (
            "cn_m",
            "macro",
            "货币供应",
            || json!({"start_m": "202401", "end_m": "202412"}),
            "month,m2",
        ),
        (
            "sf_month",
            "macro",
            "社融",
            || json!({"start_m": "202401", "end_m": "202412"}),
            "month,inc_month",
        ),
        // ========== 交易日历 ==========
        (
            "trade_cal",
            "calendar",
            "交易日历",
            || json!({"exchange": "SSE", "start_date": "20250101", "end_date": recent_trade_date()}),
            "exchange,cal_date,is_open",
        ),
    ]
}

/// 执行全部探测——串行（避免触发频率限制）。
pub async fn run_probe(app: &AppHandle) -> Vec<ProbeResult> {
    let probes = probes();
    let total = probes.len();
    tracing::info!(total, "TuShare 能力探测开始");

    let mut results = Vec::with_capacity(total);
    for (idx, (api, category, label, params_fn, fields)) in probes.into_iter().enumerate() {
        let started = Instant::now();
        let params = params_fn();
        let result = call(app, api, params, fields).await;
        let duration_ms = started.elapsed().as_millis() as i64;

        let (status, rows, error) = match result {
            Ok(items) if items.is_empty() => (ProbeStatus::Empty, 0, None),
            Ok(items) => (ProbeStatus::Ok, items.len(), None),
            Err(QuotesError::MissingToken) => {
                (ProbeStatus::MissingToken, 0, Some("token 未配".into()))
            }
            Err(QuotesError::RateLimited) => (ProbeStatus::RateLimited, 0, Some("接口限流".into())),
            Err(QuotesError::QuotaExceeded) => {
                (ProbeStatus::QuotaExceeded, 0, Some("积分不足".into()))
            }
            Err(QuotesError::Network(msg)) => (ProbeStatus::Network, 0, Some(msg)),
            Err(QuotesError::Decode(msg)) => (ProbeStatus::DecodeError, 0, Some(msg)),
            Err(QuotesError::Provider { msg, code, .. }) => (
                ProbeStatus::BadRequest,
                0,
                Some(format!("[{code:?}] {msg}")),
            ),
            Err(e) => (ProbeStatus::BadRequest, 0, Some(e.to_string())),
        };

        tracing::info!(
            idx = idx + 1,
            total,
            api,
            category,
            ?status,
            rows,
            duration_ms,
            error = error.as_deref().unwrap_or(""),
            "probe"
        );

        results.push(ProbeResult {
            api,
            category,
            label,
            status,
            rows,
            error,
            duration_ms,
        });

        // 频率保护——TuShare 单 token 每秒最多 10 次左右，我们 80ms 间隔很安全
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    // 汇总
    let ok = results
        .iter()
        .filter(|r| matches!(r.status, ProbeStatus::Ok))
        .count();
    let empty = results
        .iter()
        .filter(|r| matches!(r.status, ProbeStatus::Empty))
        .count();
    let quota = results
        .iter()
        .filter(|r| matches!(r.status, ProbeStatus::QuotaExceeded))
        .count();
    let other_fail = results.len() - ok - empty - quota;
    tracing::info!(
        ok,
        empty,
        quota_exceeded = quota,
        other_failures = other_fail,
        total = results.len(),
        "TuShare 能力探测完成"
    );

    results
}

/// IPC 命令——前端 invoke 触发探测。返回结果数组。
/// 同时把结果写到 app data dir 下 `tushare-probe-result.json`。
#[tauri::command]
pub async fn probe_tushare_capabilities(app: AppHandle) -> Result<Vec<ProbeResult>, String> {
    let results = run_probe(&app).await;

    // 写文件——方便事后整理 spec
    if let Ok(json) = serde_json::to_string_pretty(&results) {
        if let Ok(dir) = tauri::Manager::path(&app).app_data_dir() {
            let path = dir.join("tushare-probe-result.json");
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!(path = %path.display(), err = %e, "写 probe 结果失败");
            } else {
                tracing::info!(path = %path.display(), "probe 结果已落盘");
            }
        }
    }

    Ok(results)
}
