//! Quotes 模块**实网集成测试**（live integration tests）。
//!
//! Spec: docs/design/quotes-module.md §4（读取接口）+ §5（provider 策略 / universe /
//! K 线 / 复权 / 交易日历 / daily_basic / 公司事件 / TuShare 健康熔断 / pipeline 执行契约）。
//!
//! ## 这是什么
//!
//! 这些测试**打真实远端 provider**（TDX raw TCP / 腾讯 HTTP / Eastmoney HTTP / TuShare HTTP），
//! 用来端到端验证 Quotes 对外提供的主要功能。它们全部标 `#[ignore]`，**不在普通
//! `cargo test` 里运行**，只能由持凭证 / 处于可达网络的主 agent 显式 `-- --ignored` 触发。
//!
//! ## 安全纪律
//!
//! - **绝不硬编码任何 token / 密钥**。TuShare token 只通过 `std::env::var("TUSHARE_TOKEN")`
//!   读取（与 `QuotesConfig::from_env` 同源，见 `infrastructure/quotes/config.rs`）。
//! - 需要 TuShare 的测试在 `TUSHARE_TOKEN` 未设时打印 skip 并 `return`，不 panic。
//! - 任意 provider 不可达时，测试打印**清晰原因**（哪个 provider / 什么错误）后跳过，
//!   而不是脏 panic —— 让主 agent 能区分「环境不可达」与「真 bug」。
//!
//! ## 怎么跑（主 agent 用）
//!
//! 只需 TuShare（设 token；TuShare 实测可达 200）：
//! ```bash
//! TUSHARE_TOKEN=xxxx cargo test --manifest-path src-tauri/Cargo.toml --lib \
//!   quotes_live_tushare -- --ignored --nocapture --test-threads=1
//! ```
//! 需 TDX（raw TCP，可达性待定）：
//! ```bash
//! cargo test --manifest-path src-tauri/Cargo.toml --lib \
//!   quotes_live_tdx -- --ignored --nocapture --test-threads=1
//! ```
//! 需腾讯（HTTP，实测可达）：
//! ```bash
//! cargo test --manifest-path src-tauri/Cargo.toml --lib \
//!   quotes_live_tencent -- --ignored --nocapture --test-threads=1
//! ```
//! 需 Eastmoney（HTTP，本环境实测不可达 000；BJ universe 用）：
//! ```bash
//! cargo test --manifest-path src-tauri/Cargo.toml --lib \
//!   quotes_live_em -- --ignored --nocapture --test-threads=1
//! ```
//! 端到端 pipeline（混合 provider）：
//! ```bash
//! TUSHARE_TOKEN=xxxx cargo test --manifest-path src-tauri/Cargo.toml --lib \
//!   quotes_live_pipeline -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(test)]

use crate::domain::quotes::{
    Adjust, InstrumentSource, KlinePeriod, MarketInstrument, MinuteKlinePeriod, QuoteSource,
    RefreshDataScope, RefreshMarketQuotesScope, RefreshPurpose, TushareHealthConfig,
};
use crate::domain::shared::{InstrumentCategory, InstrumentStatus, TradeDate, TsCode};
use crate::infrastructure::db::{run_migrations, AppDb};
use crate::infrastructure::quotes::tdx::TdxMarket;
use crate::infrastructure::quotes::{
    migrations as quotes_migrations, EastmoneyProvider, QuotesConfig, TdxConnectionManager,
    TencentProvider, TushareClient, TushareHealthCheck,
};
use crate::pipeline::quotes::service::{
    FetchDataRequest, FetchInclude, ListMarketRequest, RefreshMarketQuotesRequest,
    ScanMarketRequest,
};
use crate::pipeline::quotes::QuotesService;
use chrono::Utc;
use std::sync::Arc;

// ============================================================================= helpers

/// 读 TuShare token（仅 env）。未设时返回 None — 调用方应 skip。
fn tushare_token() -> Option<String> {
    std::env::var("TUSHARE_TOKEN").ok().filter(|s| !s.is_empty())
}

/// 构造一个走 env token 的 TuShare client。无 token 时仍可构造（用于熔断 / token_missing 场景）。
fn tushare_client() -> TushareClient {
    TushareClient::new(tushare_token()).expect("build tushare client")
}

/// 内存 DB + quotes migrations。
fn make_db() -> AppDb {
    let db = AppDb::open_in_memory().unwrap();
    db.with(|conn| run_migrations(conn, quotes_migrations()).unwrap());
    db
}

/// 构造一个 QuotesService（token 走 env；无 token 时 TuShare 路径自动降级）。
fn make_service() -> Arc<QuotesService> {
    let db = make_db();
    Arc::new(QuotesService::new(db, QuotesConfig::from_env()).unwrap())
}

fn ts(code: &str) -> TsCode {
    TsCode::parse(code).expect("valid ts_code")
}

/// 近交易日（仅用于组装 quote DTO；provider 报价不依赖该值做有效性判断）。
fn recent_trade_date() -> TradeDate {
    // 用「今天」即可；provider 层不校验。
    TradeDate::from_naive(Utc::now().with_timezone(&chrono_tz::Asia::Shanghai).date_naive())
}

/// 把一只标的塞进本地 universe（pipeline 读路径前置）。
fn seed_instrument(svc: &QuotesService, code: &str, name: &str, cat: InstrumentCategory) {
    let c = ts(code);
    let inst = MarketInstrument {
        ts_code: c.clone(),
        name: name.to_string(),
        category: cat,
        market: c.market(),
        board: None,
        sector: Some("测试行业".into()),
        status: Some(InstrumentStatus::Listed),
        is_st: Some(false),
        publisher: None,
        index_category: None,
        fund_type: None,
        management: None,
        list_date: None,
        source: InstrumentSource::Tdx,
        updated_at: Utc::now(),
    };
    crate::infrastructure::quotes::QuotesRepository::new(svc.db())
        .upsert_instruments(&[inst])
        .unwrap();
}

/// 已知长历史老股（K 线 > 800 根）：贵州茅台。
const OLD_STOCK_SH: &str = "600519.SH";
/// 已知深市老股：平安银行（有除权事件，适合复权对比）。
const OLD_STOCK_SZ: &str = "000001.SZ";
/// 核心指数：上证指数。
const INDEX_SH: &str = "000001.SH";
/// 场内 ETF：沪深 300 ETF。
const ETF_SH: &str = "510300.SH";

// ============================================================================= A. Universe / 标的档案
// 依赖：TDX（SH/SZ universe）/ Eastmoney（BJ universe）/ TuShare（enrich）。

/// A1 · TDX SH universe count + list 解析。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_live_tdx_universe_sh() {
    let mgr = TdxConnectionManager::new();
    match mgr.fetch_universe(TdxMarket::SH).await {
        Ok(list) => {
            eprintln!("[A1] TDX SH universe count = {}", list.len());
            assert!(list.len() > 1000, "SH universe 应有上千只标的");
            // 解析不变量：code 非空、name 非空（GBK 解码成功）。
            let sample = &list[..list.len().min(5)];
            for e in sample {
                eprintln!("[A1]   {} {} pre_close={}", e.code, e.name, e.pre_close);
                assert!(!e.code.is_empty(), "code 不应为空");
            }
            // 含已知票（茅台 6 位 600519）。
            assert!(
                list.iter().any(|e| e.code == "600519"),
                "SH universe 应含贵州茅台 600519"
            );
        }
        Err(e) => eprintln!("[A1] SKIP — TDX 不可达: {e}"),
    }
}

/// A2 · TDX SZ universe。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_live_tdx_universe_sz() {
    let mgr = TdxConnectionManager::new();
    match mgr.fetch_universe(TdxMarket::SZ).await {
        Ok(list) => {
            eprintln!("[A2] TDX SZ universe count = {}", list.len());
            assert!(list.len() > 1000, "SZ universe 应有上千只标的");
            assert!(
                list.iter().any(|e| e.code == "000001"),
                "SZ universe 应含平安银行 000001"
            );
        }
        Err(e) => eprintln!("[A2] SKIP — TDX 不可达: {e}"),
    }
}

/// A2b · 把 TDX 原始全量经 `classify` 过滤后，按 类别 统计真实 universe 大小，并断言**债券不漏进**。
/// 回答「股票/指数/基金各有多少」+ 锁定「universe 只含这三类」。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_live_universe_classified_counts() {
    use crate::domain::shared::Market;
    use crate::infrastructure::quotes::universe::classify;
    let mgr = TdxConnectionManager::new();
    let mut stock = 0u32;
    let mut index = 0u32;
    let mut fund = 0u32;
    let mut dropped = 0u32; // 债券/回购等非 universe 证券
    let mut raw_total = 0u32;
    let mut index_bond_leak: Vec<String> = Vec::new();
    for (mkt, dmkt) in [(TdxMarket::SH, Market::SH), (TdxMarket::SZ, Market::SZ)] {
        match mgr.fetch_universe(mkt).await {
            Ok(list) => {
                for e in &list {
                    raw_total += 1;
                    if e.code.len() < 6 {
                        dropped += 1;
                        continue;
                    }
                    match classify(dmkt, &e.code[..6]) {
                        Some(c) => match c.category {
                            InstrumentCategory::Stock => stock += 1,
                            InstrumentCategory::Index => {
                                index += 1;
                                // 债券前缀不应出现在 Index 里。
                                let p3 = &e.code[..3];
                                if matches!(p3, "100" | "110" | "120" | "130" | "180") {
                                    index_bond_leak.push(e.code.clone());
                                }
                            }
                            InstrumentCategory::Fund => fund += 1,
                        },
                        None => dropped += 1,
                    }
                }
            }
            Err(e) => {
                eprintln!("[A2b] SKIP — TDX 不可达: {e}");
                return;
            }
        }
    }
    eprintln!(
        "[A2b] raw={raw_total} → universe: 股票={stock} 指数={index} 基金={fund}（合计 {}）；丢弃(债券/回购等)={dropped}",
        stock + index + fund
    );
    assert!(stock > 4000, "A 股股票应数千只，实测 {stock}");
    assert!(
        index_bond_leak.is_empty(),
        "债券前缀漏进指数 universe（classify bug）: {:?}",
        &index_bond_leak[..index_bond_leak.len().min(10)]
    );
}

/// A3 · Eastmoney 补 BJ universe（`fetch_bj_universe`）。Provider: EM。
#[tokio::test]
#[ignore]
async fn quotes_live_em_bj_universe() {
    let em = EastmoneyProvider::new().expect("build em");
    match em.fetch_bj_universe().await {
        Ok(list) => {
            eprintln!("[A3] EM BJ universe count = {}", list.len());
            assert!(!list.is_empty(), "BJ universe 不应为空");
            for (code, name) in list.iter().take(5) {
                eprintln!("[A3]   {} {}", code, name);
                assert_eq!(code.len(), 6, "BJ code 应为 6 位");
            }
        }
        Err(e) => eprintln!("[A3] SKIP — EM 不可达（本环境实测 000）: {e}"),
    }
}

/// A4 · TuShare stock_basic enrich（行业 / 上市状态）。Provider: TuShare。Env: TUSHARE_TOKEN。
#[tokio::test]
#[ignore]
async fn quotes_live_tushare_stock_basic_enrich() {
    let Some(_t) = tushare_token() else {
        eprintln!("[A4] SKIP — TUSHARE_TOKEN 未设");
        return;
    };
    let cli = tushare_client();
    match cli.fetch_stock_basic().await {
        Ok(list) => {
            eprintln!("[A4] TuShare stock_basic count = {}", list.len());
            assert!(list.len() > 3000, "A 股 stock_basic 应有数千只");
            let maotai = list.iter().find(|i| i.ts_code.as_str() == OLD_STOCK_SH);
            assert!(maotai.is_some(), "应含贵州茅台");
            let m = maotai.unwrap();
            eprintln!("[A4]   茅台 sector={:?} status={:?} list_date={:?}", m.sector, m.status, m.list_date);
            assert_eq!(m.category, InstrumentCategory::Stock);
            assert!(m.sector.is_some(), "enrich 应带行业");
        }
        Err(e) => eprintln!("[A4] SKIP — TuShare stock_basic 失败: {e}"),
    }
}

/// A5 · TuShare index_basic + fund_basic enrich（指数 / 场内基金分类）。Provider: TuShare。
#[tokio::test]
#[ignore]
async fn quotes_live_tushare_index_and_fund_basic() {
    let Some(_t) = tushare_token() else {
        eprintln!("[A5] SKIP — TUSHARE_TOKEN 未设");
        return;
    };
    let cli = tushare_client();
    match cli.fetch_index_basic("SSE").await {
        Ok(list) => {
            eprintln!("[A5] TuShare index_basic SSE count = {}", list.len());
            assert!(!list.is_empty(), "SSE 指数不应为空");
            for i in list.iter().take(3) {
                assert_eq!(i.category, InstrumentCategory::Index);
            }
        }
        Err(e) => eprintln!("[A5] SKIP — index_basic 失败: {e}"),
    }
    match cli.fetch_fund_basic().await {
        Ok(list) => {
            eprintln!("[A5] TuShare fund_basic(E) count = {}", list.len());
            assert!(!list.is_empty(), "场内基金不应为空");
            for f in list.iter().take(3) {
                assert_eq!(f.category, InstrumentCategory::Fund);
                eprintln!("[A5]   {} {} fund_type={:?}", f.ts_code.as_str(), f.name, f.fund_type);
            }
        }
        Err(e) => eprintln!("[A5] SKIP — fund_basic 失败: {e}"),
    }
}

// ============================================================================= B. 实时行情
// 依赖：TDX（SH/SZ 主路径）/ 腾讯（fallback + BJ 主路径）。

/// B1 · TDX 单只报价（SH 股票茅台），价格 > 0 + freshness。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_live_tdx_quote_single_sh_stock() {
    let mgr = TdxConnectionManager::new();
    let code = ts(OLD_STOCK_SH);
    match mgr
        .fetch_quote(&code, InstrumentCategory::Stock, recent_trade_date(), Utc::now(), None)
        .await
    {
        Ok(q) => {
            eprintln!(
                "[B1] 茅台 price={:?} prevClose={:?} change%={:?} source={:?}",
                q.price, q.previous_close, q.change_percent, q.source
            );
            eprintln!("[B1]   display_complete={} quote_complete={}", q.is_display_complete(), q.is_quote_complete());
            assert!(q.price.is_some(), "茅台应有报价");
            assert!(q.price.unwrap().0 > rust_decimal::Decimal::ZERO, "价格 > 0");
            assert_eq!(q.source, QuoteSource::Tdx);
        }
        Err(e) => eprintln!("[B1] SKIP — TDX 不可达: {e}"),
    }
}

/// B2 · TDX 批量报价（SH 股票 + SZ 股票 + 指数 + ETF 混合），价格不变量。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_live_tdx_quote_batch_mixed() {
    let mgr = TdxConnectionManager::new();
    let codes = vec![
        (ts(OLD_STOCK_SH), InstrumentCategory::Stock, None),
        (ts(OLD_STOCK_SZ), InstrumentCategory::Stock, None),
        (ts(INDEX_SH), InstrumentCategory::Index, None),
        (ts(ETF_SH), InstrumentCategory::Fund, None),
    ];
    let results = mgr.fetch_quotes(codes, recent_trade_date(), Utc::now()).await;
    let ok: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    eprintln!("[B2] batch ok={}/{}", ok.len(), results.len());
    if ok.is_empty() {
        eprintln!("[B2] SKIP — TDX 全部失败（可能不可达）: {:?}", results.first().map(|r| r.as_ref().err()));
        return;
    }
    for q in &ok {
        eprintln!("[B2]   {} price={:?} cat={:?}", q.ts_code.as_str(), q.price, q.category);
        if let Some(p) = q.price {
            assert!(p.0 > rust_decimal::Decimal::ZERO, "{} 价格 > 0", q.ts_code.as_str());
        }
    }
    // 市场串号回归守卫：000001.SH(上证指数) 与 000001.SZ(平安银行) 同代码不同市场，批量结果
    // 必须按 (market, code) 区分。串号 bug 会让两者价格相同（指数错配成深市股票约 10 元价位）。
    let sh_idx = ok.iter().find(|q| q.ts_code.as_str() == INDEX_SH).and_then(|q| q.price);
    let sz_stk = ok.iter().find(|q| q.ts_code.as_str() == OLD_STOCK_SZ).and_then(|q| q.price);
    if let (Some(idx), Some(stk)) = (sh_idx, sz_stk) {
        assert_ne!(idx.0, stk.0, "000001.SH 指数价被错配成 000001.SZ 股票价（市场串号 bug）");
        assert!(idx.0 > rust_decimal::Decimal::from(500), "上证指数价应在指数区间（实测 ~4000），不是个股价位");
    }
    // ETF 小数位校正回归守卫：510300(沪深300ETF) 是 3 位小数标的，实测 ~4.9。
    // 缩放 bug（protocol /100 未按 Fund 校正）会得到 ~48.7（10×）。
    if let Some(etf) = ok.iter().find(|q| q.ts_code.as_str() == ETF_SH).and_then(|q| q.price) {
        assert!(
            etf.0 < rust_decimal::Decimal::from(20),
            "510300 ETF 价应在 ~5 元区间（3 位小数已校正），不是 ~48（10× 缩放 bug）；实测 {}",
            etf.0
        );
    }
}

/// B3 · 腾讯单只报价（SH 股票），五档盘口完整性 vs 仅展示。Provider: 腾讯。
#[tokio::test]
#[ignore]
async fn quotes_live_tencent_quote_sh_stock_depth() {
    let tx = TencentProvider::new().expect("build tencent");
    let code = ts(OLD_STOCK_SH);
    match tx
        .fetch_quote(&code, InstrumentCategory::Stock, recent_trade_date(), Utc::now())
        .await
    {
        Ok(q) => {
            eprintln!(
                "[B3] 腾讯茅台 price={:?} turnover={:?} amount={:?} bid_levels={} ask_levels={}",
                q.price, q.turnover_rate, q.amount, q.bid.len(), q.ask.len()
            );
            assert!(q.price.is_some(), "应有价格");
            assert!(q.price.unwrap().0 > rust_decimal::Decimal::ZERO, "价格 > 0");
            assert_eq!(q.source, QuoteSource::Tencent);
            // 腾讯实测含五档盘口 + 成交额 + 换手；股票应能 quote_complete 或至少 display_complete。
            assert!(q.is_display_complete(), "至少 display_complete");
            eprintln!("[B3]   display_complete={} quote_complete={}", q.is_display_complete(), q.is_quote_complete());
        }
        Err(e) => eprintln!("[B3] SKIP — 腾讯不可达: {e}"),
    }
}

/// B4 · 腾讯指数 / ETF 报价（指数常缺 previousClose，应仍 display_complete）。Provider: 腾讯。
#[tokio::test]
#[ignore]
async fn quotes_live_tencent_quote_index_and_etf() {
    let tx = TencentProvider::new().expect("build tencent");
    for (code, cat) in [(INDEX_SH, InstrumentCategory::Index), (ETF_SH, InstrumentCategory::Fund)] {
        match tx
            .fetch_quote(&ts(code), cat, recent_trade_date(), Utc::now())
            .await
        {
            Ok(q) => {
                eprintln!("[B4] {} price={:?} display_complete={}", code, q.price, q.is_display_complete());
                assert!(q.is_display_complete(), "{} 应 display_complete（price 非空）", code);
            }
            Err(e) => eprintln!("[B4] SKIP {} — 腾讯不可达: {e}", code),
        }
    }
}

/// B5 · BJ 实时报价走腾讯（BJ 不走 TDX）。Provider: 腾讯。
#[tokio::test]
#[ignore]
async fn quotes_live_tencent_quote_bj() {
    let tx = TencentProvider::new().expect("build tencent");
    // 一只北交所标的（贝特瑞 835185.BJ 为常见 BJ 票）；若退市/停牌可换。
    let code = ts("835185.BJ");
    match tx
        .fetch_quote(&code, InstrumentCategory::Stock, recent_trade_date(), Utc::now())
        .await
    {
        Ok(q) => {
            eprintln!("[B5] BJ {} price={:?} display_complete={}", code.as_str(), q.price, q.is_display_complete());
            // BJ 盘前快照字段较少；只要解析成功不 panic 即算通过。BJ 也确认走腾讯而非 TDX。
        }
        Err(e) => eprintln!("[B5] SKIP/INFO — 腾讯 BJ 报价: {e}（可能该票停牌或字段不足）"),
    }
    // 对照：TDX 对 BJ 必须 UnsupportedMarket（不走 TDX）。
    let mgr = TdxConnectionManager::new();
    let r = mgr
        .fetch_quote(&code, InstrumentCategory::Stock, recent_trade_date(), Utc::now(), None)
        .await;
    assert!(r.is_err(), "TDX 不应支持 BJ 报价（spec §5：BJ 走腾讯）");
}

// ============================================================================= C. K 线
// 依赖：TDX（主路径 + 分钟 K）/ TuShare（长历史扩展）/ EM（分钟 K fallback）。

/// C1 · TDX 日 K 主路径（单次 ~800 根上限），OHLC 有序 + 日期单调。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_live_tdx_kline_daily() {
    let mgr = TdxConnectionManager::new();
    let code = ts(OLD_STOCK_SH);
    match mgr.fetch_kline_at(&code, KlinePeriod::Day, 0, 800).await {
        Ok(bars) => {
            eprintln!("[C1] 茅台日 K 根数 = {}", bars.len());
            assert!(!bars.is_empty(), "应返回日 K");
            assert!(bars.len() <= 800, "单次拉取 ≤ 800 根");
            for b in &bars {
                assert!(b.high >= b.low, "high >= low");
                assert!(b.high >= b.open && b.high >= b.close, "high 为最高");
                assert!(b.low <= b.open && b.low <= b.close, "low 为最低");
                assert!(b.close > 0.0, "收盘 > 0");
            }
            eprintln!("[C1]   first={} last={}", bars.first().unwrap().datetime(), bars.last().unwrap().datetime());
        }
        Err(e) => eprintln!("[C1] SKIP — TDX 不可达: {e}"),
    }
}

/// C2 · TDX 周 K + 月 K。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_live_tdx_kline_week_month() {
    let mgr = TdxConnectionManager::new();
    let code = ts(OLD_STOCK_SH);
    for period in [KlinePeriod::Week, KlinePeriod::Month] {
        match mgr.fetch_kline_at(&code, period, 0, 800).await {
            Ok(bars) => {
                eprintln!("[C2] 茅台 {:?} 根数 = {}", period, bars.len());
                assert!(!bars.is_empty(), "{:?} 应返回数据", period);
                for b in &bars {
                    assert!(b.high >= b.low && b.close > 0.0);
                }
            }
            Err(e) => eprintln!("[C2] SKIP {:?} — TDX 不可达: {e}", period),
        }
    }
}

/// C3 · TDX 全量历史分页（老股 > 800 根，多次分页）。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_live_tdx_kline_paginated_old_stock() {
    let mgr = TdxConnectionManager::new();
    let code = ts(OLD_STOCK_SH);
    match mgr.fetch_kline_paginated(&code, KlinePeriod::Day).await {
        Ok(bars) => {
            eprintln!("[C3] 茅台全量日 K 根数 = {}", bars.len());
            // 茅台 2001 上市，日 K 应远超单次 800 根上限。
            assert!(bars.len() > 800, "老股全量历史应 > 800 根（触发分页），实际 {}", bars.len());
            // 日期单调递增（分页 prepend 后应升序）。
            for w in bars.windows(2) {
                assert!(w[0].datetime() <= w[1].datetime(), "K 线日期应单调递增");
            }
        }
        Err(e) => eprintln!("[C3] SKIP — TDX 不可达: {e}"),
    }
}

/// C4 · TuShare 长历史 K 线扩展（超出 TDX 800 根单次上限的早期段）。Provider: TuShare。Env: TUSHARE_TOKEN。
#[tokio::test]
#[ignore]
async fn quotes_live_tushare_kline_long_history() {
    let Some(_t) = tushare_token() else {
        eprintln!("[C4] SKIP — TUSHARE_TOKEN 未设");
        return;
    };
    let cli = tushare_client();
    let code = ts(OLD_STOCK_SH);
    // 拉一段早期历史（2010-2013，约 700+ 交易日，落在 TDX 单次上限以外的「老段」）。
    match cli.fetch_kline(&code, KlinePeriod::Day, "20100101", "20131231").await {
        Ok(bars) => {
            eprintln!("[C4] TuShare 茅台 2010-2013 日 K 根数 = {}", bars.len());
            assert!(bars.len() > 600, "约 3 年应有 600+ 交易日，实际 {}", bars.len());
            // TuShare adapter 已翻转为升序。
            for w in bars.windows(2) {
                assert!(w[0].date <= w[1].date, "TuShare K 线应升序");
            }
            let first = &bars[0];
            assert!(first.close.0 > rust_decimal::Decimal::ZERO, "收盘 > 0");
        }
        Err(e) => eprintln!("[C4] SKIP — TuShare daily 失败: {e}"),
    }
}

/// C5 · TDX 分钟 K（5m / 60m），count 上限。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_live_tdx_minute_kline() {
    let mgr = TdxConnectionManager::new();
    let code = ts(OLD_STOCK_SH);
    for period in [MinuteKlinePeriod::M5, MinuteKlinePeriod::M60] {
        match mgr.fetch_minute_kline(&code, period, 240).await {
            Ok(bars) => {
                eprintln!("[C5] 茅台 {:?} 分钟 K 根数 = {}", period, bars.len());
                assert!(!bars.is_empty(), "{:?} 应返回分钟 K", period);
                for b in &bars {
                    assert!(b.high >= b.low && b.close > 0.0);
                }
            }
            Err(e) => eprintln!("[C5] SKIP {:?} — TDX 不可达: {e}", period),
        }
    }
}

/// C6 · EM 分钟 K fallback（SH/SZ）。Provider: EM。
#[tokio::test]
#[ignore]
async fn quotes_live_em_minute_kline() {
    let em = EastmoneyProvider::new().expect("build em");
    let code = ts(OLD_STOCK_SH);
    match em.fetch_minute_kline(&code, MinuteKlinePeriod::M5, 240).await {
        Ok(pts) => {
            eprintln!("[C6] EM 茅台 5m 分钟 K 根数 = {}", pts.len());
            assert!(!pts.is_empty(), "EM 应返回分钟 K");
        }
        Err(e) => eprintln!("[C6] SKIP — EM 不可达（本环境实测 000）: {e}"),
    }
}

/// C7 · EM 日线 K fallback（TuShare 不可用时）。Provider: EM。
///
/// 盲区④：EM 日线兜底 `fetch_daily_kline`。断言：根数 > 0、OHLC 有序
/// (high≥low、close/open 落在 [low,high])、日期严格单调递增。
/// 本环境 EM 出口屏蔽（实测 HTTP 000）→ 不可达时优雅 skip。
#[tokio::test]
#[ignore]
async fn quotes_live_em_daily_kline() {
    let em = EastmoneyProvider::new().expect("build em");
    let code = ts(OLD_STOCK_SH);
    match em.fetch_daily_kline(&code, 120).await {
        Ok(pts) => {
            eprintln!("[C7] EM 茅台 日线 K 根数 = {}", pts.len());
            assert!(!pts.is_empty(), "EM 应返回日线 K");
            for p in &pts {
                assert!(p.high.0 >= p.low.0, "high >= low");
                assert!(p.close.0 >= p.low.0 && p.close.0 <= p.high.0, "close ∈ [low,high]");
                assert!(p.open.0 >= p.low.0 && p.open.0 <= p.high.0, "open ∈ [low,high]");
            }
            // 日期严格单调递增（EM kline 升序返回）。
            for w in pts.windows(2) {
                assert!(
                    w[0].date.format() < w[1].date.format(),
                    "日期应严格单调递增: {} !< {}",
                    w[0].date.format(),
                    w[1].date.format()
                );
            }
            if let (Some(first), Some(last)) = (pts.first(), pts.last()) {
                eprintln!("[C7]   日期区间 {} .. {}", first.date.format(), last.date.format());
            }
        }
        Err(e) => eprintln!("[C7] SKIP — EM 不可达（本环境实测 000）: {e}"),
    }
}

// ============================================================================= D. 复权（本地基于 TDX xdxr）
// 依赖：TDX（xdxr 事件 + unadjusted K）。复权计算为本地纯算。

/// D1 · TDX xdxr 事件拉取（有除权事件的票）。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_live_tdx_xdxr_events() {
    let mgr = TdxConnectionManager::new();
    let code = ts(OLD_STOCK_SZ); // 平安银行有多年送转分红历史
    match mgr.fetch_xdxr(&code).await {
        Ok(records) => {
            eprintln!("[D1] 平安银行 xdxr 事件数 = {}", records.len());
            assert!(!records.is_empty(), "平安银行应有除权事件");
            // 至少一条「除权除息」（category == 1）应带分红 / 送转字段。
            let div_events: Vec<_> = records.iter().filter(|r| r.category == 1).collect();
            eprintln!("[D1]   除权除息事件数 = {}", div_events.len());
            for r in records.iter().take(5) {
                eprintln!(
                    "[D1]   {}-{:02}-{:02} cat={} fenhong={:?} songzhuangu={:?}",
                    r.year, r.month, r.day, r.category, r.fenhong, r.songzhuangu
                );
            }
        }
        Err(e) => eprintln!("[D1] SKIP — TDX 不可达: {e}"),
    }
}

/// D2 · 端到端 qfq / hfq / none 对比（pipeline 本地复权计算）。Provider: TDX。
///
/// 经典关系：对有正向送转分红的票，hfq 把早期价上调、qfq 把早期价下调，
/// 故对同一早期 bar：`hfq.close >= none.close >= qfq.close`（无除权时三者相等）。
#[tokio::test]
#[ignore]
async fn quotes_live_pipeline_adjust_qfq_hfq_compare() {
    let svc = make_service();
    seed_instrument(&svc, OLD_STOCK_SZ, "平安银行", InstrumentCategory::Stock);
    let code = ts(OLD_STOCK_SZ);

    // 先落 unadjusted K 线 + xdxr 事件。
    if let Err(e) = svc.refresh_klines(RefreshDataScope::Manual { ts_codes: vec![code.clone()] }, vec![KlinePeriod::Day]).await {
        eprintln!("[D2] SKIP — refresh_klines 失败（TDX 不可达?）: {:?}", e);
        return;
    }
    if let Err(e) = svc.refresh_xdxr_events(RefreshDataScope::Manual { ts_codes: vec![code.clone()] }).await {
        eprintln!("[D2] SKIP — refresh_xdxr_events 失败: {:?}", e);
        return;
    }

    let none = svc.read_kline_series_with_adjust(&code, KlinePeriod::Day, Adjust::None, 800).unwrap();
    let qfq = svc.read_kline_series_with_adjust(&code, KlinePeriod::Day, Adjust::Qfq, 800).unwrap();
    let hfq = svc.read_kline_series_with_adjust(&code, KlinePeriod::Day, Adjust::Hfq, 800).unwrap();

    let (Some(none), Some(qfq), Some(hfq)) = (none, qfq, hfq) else {
        eprintln!("[D2] SKIP — 本地无 K 线（refresh 可能空）");
        return;
    };
    eprintln!(
        "[D2] 平安 K 线根数 none={} qfq={} hfq={}（qfq warnings={:?}）",
        none.points.len(), qfq.points.len(), hfq.points.len(), qfq.warnings
    );
    assert_eq!(none.points.len(), qfq.points.len(), "复权不改点位数量");
    assert_eq!(none.points.len(), hfq.points.len(), "复权不改点位数量");
    if let (Some(n0), Some(q0), Some(h0)) =
        (none.points.first(), qfq.points.first(), hfq.points.first())
    {
        eprintln!(
            "[D2]   首点 {} none.close={} qfq.close={} hfq.close={}",
            n0.date.format(), n0.close.0, q0.close.0, h0.close.0
        );
        // 有除权时 hfq >= none >= qfq；无除权时三者相等。两种情况都满足 hfq >= qfq。
        assert!(h0.close.0 >= q0.close.0, "早期 bar hfq.close 应 >= qfq.close");
    }
}

/// D3 · 无除权事件标的（指数）qfq 等同 none，且为合理终态（不应误判数据缺失 panic）。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_live_pipeline_adjust_index_no_events() {
    let svc = make_service();
    seed_instrument(&svc, INDEX_SH, "上证指数", InstrumentCategory::Index);
    let code = ts(INDEX_SH);
    if let Err(e) = svc.refresh_klines(RefreshDataScope::Manual { ts_codes: vec![code.clone()] }, vec![KlinePeriod::Day]).await {
        eprintln!("[D3] SKIP — refresh_klines 失败: {:?}", e);
        return;
    }
    // 指数无除权概念；refresh_xdxr 对指数返回 UnsupportedMarket（BJ）或空。
    let _ = svc.refresh_xdxr_events(RefreshDataScope::Manual { ts_codes: vec![code.clone()] }).await;
    let none = svc.read_kline_series_with_adjust(&code, KlinePeriod::Day, Adjust::None, 100).unwrap();
    let qfq = svc.read_kline_series_with_adjust(&code, KlinePeriod::Day, Adjust::Qfq, 100).unwrap();
    if let (Some(none), Some(qfq)) = (none, qfq) {
        eprintln!("[D3] 指数 none={} qfq={} qfq.warnings={:?}", none.points.len(), qfq.points.len(), qfq.warnings);
        // 无 xdxr 事件 → qfq 点位与 none 完全一致。
        if let (Some(n), Some(q)) = (none.points.last(), qfq.points.last()) {
            assert_eq!(n.close.0, q.close.0, "指数无除权 → qfq.close == none.close");
        }
    } else {
        eprintln!("[D3] SKIP — 本地无指数 K 线");
    }
}

// ============================================================================= E. 交易日历
// 依赖：TuShare（trade_cal）。本地推算为 default，TuShare 校准 optional。

/// E1 · TuShare trade_cal 拉取，含已知交易日 / 排除周末。Provider: TuShare。Env: TUSHARE_TOKEN。
#[tokio::test]
#[ignore]
async fn quotes_live_tushare_trade_cal() {
    let Some(_t) = tushare_token() else {
        eprintln!("[E1] SKIP — TUSHARE_TOKEN 未设");
        return;
    };
    let cli = tushare_client();
    // 2024 年 1 月（含元旦假期 + 正常交易周）。
    match cli.fetch_trade_cal("20240101", "20240131").await {
        Ok(entries) => {
            eprintln!("[E1] trade_cal 2024-01 条数 = {}", entries.len());
            assert!(entries.len() >= 28, "一个月应有 ~31 条日历记录");
            // 2024-01-01 元旦休市。
            let new_year = entries.iter().find(|e| e.cal_date.format() == "20240101");
            assert!(new_year.is_some());
            assert!(!new_year.unwrap().is_open, "元旦应休市");
            // 2024-01-02 周二为交易日。
            let jan2 = entries.iter().find(|e| e.cal_date.format() == "20240102");
            assert!(jan2.map(|e| e.is_open).unwrap_or(false), "2024-01-02 应为交易日");
            // 周末必为非交易日（2024-01-06 周六）。
            let sat = entries.iter().find(|e| e.cal_date.format() == "20240106");
            assert!(sat.map(|e| !e.is_open).unwrap_or(true), "周六应休市");
            let open_count = entries.iter().filter(|e| e.is_open).count();
            eprintln!("[E1]   开市日数 = {}", open_count);
        }
        Err(e) => eprintln!("[E1] SKIP — TuShare trade_cal 失败: {e}"),
    }
}

/// E2 · pipeline refresh_trade_calendar（TuShare 校准本地日历）。Provider: TuShare。Env: TUSHARE_TOKEN。
#[tokio::test]
#[ignore]
async fn quotes_live_pipeline_refresh_trade_calendar() {
    let Some(_t) = tushare_token() else {
        eprintln!("[E2] SKIP — TUSHARE_TOKEN 未设");
        return;
    };
    let svc = make_service();
    // health gate：需先 ping 才会走 TuShare 校准。
    svc.health().initial_ping().await;
    if !svc.health().is_available() {
        eprintln!("[E2] SKIP — TuShare health 不可用: {:?}", svc.health().state());
        return;
    }
    match svc.refresh_trade_calendar("20240101", "20240131").await {
        Ok(n) => {
            eprintln!("[E2] refresh_trade_calendar 写入 {} 行", n);
            assert!(n > 0, "应写入日历行");
        }
        Err(e) => eprintln!("[E2] SKIP — refresh 失败: {:?}", e),
    }
}

// ============================================================================= F. DailyBasic / 公司事件
// 依赖：TuShare（daily_basic / dividend / suspend）。需 token + health。

/// F1 · TuShare daily_basic（PE/PB/换手/市值）。Provider: TuShare。Env: TUSHARE_TOKEN。
#[tokio::test]
#[ignore]
async fn quotes_live_tushare_daily_basic() {
    let Some(_t) = tushare_token() else {
        eprintln!("[F1] SKIP — TUSHARE_TOKEN 未设");
        return;
    };
    let cli = tushare_client();
    let code = ts(OLD_STOCK_SH);
    // 用一个已知交易日（2024-01-02 周二）。
    match cli.fetch_daily_basic(Some(&code), Some("20240102")).await {
        Ok(rows) => {
            eprintln!("[F1] 茅台 daily_basic 行数 = {}", rows.len());
            if let Some(r) = rows.first() {
                eprintln!(
                    "[F1]   pe={:?} pe_ttm={:?} pb={:?} turnover={:?} total_mv={:?}",
                    r.pe, r.pe_ttm, r.pb, r.turnover_rate, r.total_mv
                );
                assert_eq!(r.ts_code.as_str(), OLD_STOCK_SH);
                assert!(r.pe_ttm.is_some() || r.pb.is_some(), "应至少有一个估值字段");
                if let Some(mv) = r.total_mv {
                    assert!(mv.0 > rust_decimal::Decimal::ZERO, "市值 > 0");
                }
            } else {
                eprintln!("[F1] INFO — 该日无 daily_basic（可能非交易日）");
            }
        }
        Err(e) => eprintln!("[F1] SKIP — daily_basic 失败: {e}"),
    }
}

/// F2 · TuShare 公司事件（分红 + 停复牌）。Provider: TuShare。Env: TUSHARE_TOKEN。
#[tokio::test]
#[ignore]
async fn quotes_live_tushare_company_events() {
    let Some(_t) = tushare_token() else {
        eprintln!("[F2] SKIP — TUSHARE_TOKEN 未设");
        return;
    };
    let cli = tushare_client();
    let code = ts(OLD_STOCK_SH);
    match cli.fetch_dividends(Some(&code), "20230101", "20241231").await {
        Ok(events) => {
            eprintln!("[F2] 茅台分红事件数 = {}", events.len());
            for e in events.iter().take(3) {
                eprintln!("[F2]   id={} type={:?} ann={:?} ex={:?}", e.id, e.event_type, e.announce_date, e.effective_date);
                assert_eq!(e.ts_code.as_str(), OLD_STOCK_SH);
                assert!(!e.id.is_empty(), "事件 id 不应为空");
            }
        }
        Err(e) => eprintln!("[F2] SKIP — dividend 失败: {e}"),
    }
    match cli.fetch_suspensions(Some(&code), "20200101", "20241231").await {
        Ok(events) => eprintln!("[F2] 茅台停复牌事件数 = {}", events.len()),
        Err(e) => eprintln!("[F2] INFO — suspend 失败（可能无事件）: {e}"),
    }
}

/// F3 · pipeline refresh_daily_basic（health gate 通过时走 TuShare 并落库）。Provider: TuShare。Env: TUSHARE_TOKEN。
#[tokio::test]
#[ignore]
async fn quotes_live_pipeline_refresh_daily_basic() {
    let Some(_t) = tushare_token() else {
        eprintln!("[F3] SKIP — TUSHARE_TOKEN 未设");
        return;
    };
    let svc = make_service();
    seed_instrument(&svc, OLD_STOCK_SH, "贵州茅台", InstrumentCategory::Stock);
    svc.health().initial_ping().await;
    if !svc.health().is_available() {
        eprintln!("[F3] SKIP — TuShare health 不可用: {:?}", svc.health().state());
        return;
    }
    let code = ts(OLD_STOCK_SH);
    match svc
        .refresh_daily_basic(
            RefreshDataScope::Manual { ts_codes: vec![code.clone()] },
            Some(TradeDate::parse("20240102").unwrap()),
        )
        .await
    {
        Ok(res) => {
            eprintln!("[F3] refresh_daily_basic total={} success={} failed={}", res.total, res.success, res.failed);
            // health 可用时 total 应 > 0（走了 TuShare），不再是降级的 0。
            assert!(res.total > 0, "health 可用时应实际请求 TuShare");
        }
        Err(e) => eprintln!("[F3] SKIP — refresh 失败: {:?}", e),
    }
}

// ============================================================================= G. TuShare 健康熔断
// 依赖：TuShare（health probe）。核心降级语义。

/// G1 · token 缺失 → token_missing，不发网络，is_available=false。Provider: 无（纯本地）。
#[tokio::test]
#[ignore]
async fn quotes_live_tushare_health_token_missing() {
    // 显式构造无 token 的 client（不读 env），验证 token_missing 语义。
    let cli = Arc::new(TushareClient::new(None).unwrap());
    let health = TushareHealthCheck::new(cli, TushareHealthConfig::default());
    assert!(!health.is_available(), "no-token client 应非可用");
    health.initial_ping().await; // 应 short-circuit，不发网络
    let state = health.state();
    eprintln!("[G1] no-token state = {:?}", state);
    assert!(!state.is_available, "token 缺失 → 不可用");
    assert_eq!(state.last_error.as_deref(), Some("token_missing"));
}

/// G2 · 有 token 且健康 → initial_ping 后 is_available=true。Provider: TuShare。Env: TUSHARE_TOKEN。
#[tokio::test]
#[ignore]
async fn quotes_live_tushare_health_ping_ok() {
    let Some(_t) = tushare_token() else {
        eprintln!("[G2] SKIP — TUSHARE_TOKEN 未设");
        return;
    };
    let cli = Arc::new(tushare_client());
    let health = TushareHealthCheck::new(cli, TushareHealthConfig::default());
    health.initial_ping().await;
    let state = health.state();
    eprintln!("[G2] ping state = {:?}", state);
    if state.is_available {
        assert!(state.last_success_at.is_some(), "成功 ping 应记录 last_success_at");
        assert!(state.last_error.is_none(), "成功时无 error");
    } else {
        eprintln!("[G2] INFO — token 设了但 ping 失败（网络/鉴权/限频）: {:?}", state.last_error);
    }
}

/// G3 · 熔断：连续失败达阈值（默认 3）翻 false；一次成功重置。Provider: 无（纯本地状态机）。
#[tokio::test]
#[ignore]
async fn quotes_live_tushare_health_circuit_breaker() {
    let cli = Arc::new(tushare_client());
    let health = TushareHealthCheck::new(cli, TushareHealthConfig::default());
    // 模拟业务调用：record_failure 累加；达 3 翻 false。
    health.record_failure("e1");
    health.record_failure("e2");
    // 两次不足阈值；此时若有 token state 仍可能 unknown（未 ping）。
    health.record_failure("e3");
    let after_three = health.state();
    eprintln!("[G3] 3 次失败后 state = {:?}", after_three);
    assert!(!after_three.is_available, "连续 3 次失败应熔断 → false");
    assert!(after_three.last_error.as_deref().unwrap_or("").contains("circuit_open"));
    // 一次成功立即重置 → available。
    health.record_success();
    let after_success = health.state();
    eprintln!("[G3] 成功后 state = {:?}", after_success);
    assert!(after_success.is_available, "一次成功应重置熔断 → true");
    assert!(after_success.last_error.is_none());
}

/// G4 · health 不可用时 pipeline daily_basic 降级而非失败（返回 Ok + DataPartial）。Provider: 无。
#[tokio::test]
#[ignore]
async fn quotes_live_pipeline_daily_basic_degraded_when_unhealthy() {
    let svc = make_service();
    // 不调 initial_ping → health 默认 unknown/unavailable（即便有 token 也未 ping）。
    // 强制熔断确保 unavailable。
    svc.health().record_failure("x");
    svc.health().record_failure("x");
    svc.health().record_failure("x");
    assert!(!svc.health().is_available(), "应处于不可用态");
    let res = svc.refresh_daily_basic(RefreshDataScope::Universe, None).await.unwrap();
    eprintln!("[G4] degraded daily_basic total={} warnings={:?}", res.total, res.warnings);
    assert_eq!(res.total, 0, "不可用 → 不发起业务调用");
    assert!(res.warnings.contains(&crate::domain::shared::WarningCode::DataPartial));
}

// ============================================================================= H. Pipeline 端到端
// 依赖：TDX（主）+ 腾讯（fallback）。混合 provider 全链路。

/// H1 · refresh_market_quotes(manual) → list_market(includeQuote) 读回。Provider: TDX/腾讯。
#[tokio::test]
#[ignore]
async fn quotes_live_pipeline_refresh_manual_then_list() {
    let svc = make_service();
    seed_instrument(&svc, OLD_STOCK_SH, "贵州茅台", InstrumentCategory::Stock);
    seed_instrument(&svc, INDEX_SH, "上证指数", InstrumentCategory::Index);
    let codes = vec![ts(OLD_STOCK_SH), ts(INDEX_SH)];
    let req = RefreshMarketQuotesRequest {
        scope: RefreshMarketQuotesScope::Manual { ts_codes: codes.clone() },
        purpose: RefreshPurpose::Intraday,
        trade_date: None,
    };
    match svc.refresh_market_quotes(req).await {
        Ok(payload) => {
            eprintln!(
                "[H1] refresh manual total={} success={} failedBatches={}",
                payload.total, payload.success, payload.failed_batches
            );
            // 读回 list_market（只读 snapshot，不触发 provider）。
            let list = svc.list_market(ListMarketRequest {
                category: None,
                query: None,
                include_quote: Some(true),
                limit: Some(50),
                offset: None,
            });
            eprintln!("[H1] list_market items = {}", list.items.len());
            let maotai = list.items.iter().find(|i| i.instrument.ts_code.as_str() == OLD_STOCK_SH);
            assert!(maotai.is_some(), "list 应含茅台");
            if let Some(m) = maotai {
                eprintln!("[H1]   茅台 quote={:?} freshness={:?} warnings={:?}", m.quote.as_ref().map(|q| q.price), m.quote_freshness.as_ref().map(|f| f.status), m.warnings);
            }
        }
        Err(e) => eprintln!("[H1] SKIP — refresh 失败（TDX/腾讯不可达?）: {:?}", e),
    }
}

/// H2 · fetch_data 完整 include（quote + klines + indicators + profile）。Provider: TDX/腾讯。
#[tokio::test]
#[ignore]
async fn quotes_live_pipeline_fetch_data_full() {
    let svc = make_service();
    seed_instrument(&svc, OLD_STOCK_SH, "贵州茅台", InstrumentCategory::Stock);
    let code = ts(OLD_STOCK_SH);

    // 先 refresh quote + K 线（fetch_data 只读本地，不触发 provider）。
    let _ = svc
        .refresh_market_quotes(RefreshMarketQuotesRequest {
            scope: RefreshMarketQuotesScope::Manual { ts_codes: vec![code.clone()] },
            purpose: RefreshPurpose::Intraday,
            trade_date: None,
        })
        .await;
    let kline_ok = svc
        .refresh_klines(RefreshDataScope::Manual { ts_codes: vec![code.clone()] }, vec![KlinePeriod::Day])
        .await
        .is_ok();
    if !kline_ok {
        eprintln!("[H2] SKIP — refresh_klines 失败（TDX 不可达?）");
        return;
    }

    let req = FetchDataRequest {
        ts_codes: Some(vec![OLD_STOCK_SH.into()]),
        include: Some(FetchInclude {
            quote: Some(true),
            klines: Some(vec![KlinePeriod::Day]),
            indicators: Some(crate::pipeline::quotes::service::FetchIndicators::All(true)),
            profile: Some(true),
            ..Default::default()
        }),
        limit: None,
    };
    let res = svc.fetch_data(req);
    assert_eq!(res.items.len(), 1, "应返回 1 个 item");
    let item = &res.items[0];
    eprintln!(
        "[H2] item quote={:?} klines={:?} indicators_present={} profile_present={} warnings={:?}",
        item.quote.as_ref().map(|q| q.price),
        item.klines.as_ref().map(|k| k.keys().collect::<Vec<_>>()),
        item.indicators.is_some(),
        item.profile.is_some(),
        item.warnings
    );
    assert!(item.profile.is_some(), "应返回 profile");
    if let Some(klines) = &item.klines {
        if let Some(day) = klines.get(KlinePeriod::Day.as_str()) {
            assert!(!day.points.is_empty(), "日 K 应有点位");
            // K 线日期单调。
            for w in day.points.windows(2) {
                assert!(w[0].date <= w[1].date, "fetch_data 日 K 应升序");
            }
        }
    }
}

/// H3 · scan_market（top_gain filter）+ market_breadth + industry_heatmap。Provider: TDX/腾讯。
#[tokio::test]
#[ignore]
async fn quotes_live_pipeline_scan_breadth_heatmap() {
    let svc = make_service();
    // 多只股票铺底，便于扫描 / breadth / heatmap 有内容。
    let stocks = ["600519.SH", "000001.SZ", "600036.SH", "601318.SH", "000002.SZ"];
    for s in stocks {
        seed_instrument(&svc, s, s, InstrumentCategory::Stock);
    }
    let codes: Vec<TsCode> = stocks.iter().map(|s| ts(s)).collect();
    let refreshed = svc
        .refresh_market_quotes(RefreshMarketQuotesRequest {
            scope: RefreshMarketQuotesScope::Manual { ts_codes: codes },
            purpose: RefreshPurpose::Intraday,
            trade_date: None,
        })
        .await;
    if refreshed.is_err() {
        eprintln!("[H3] SKIP — refresh 失败（provider 不可达?）");
        return;
    }

    let scan = svc.scan_market(ScanMarketRequest {
        category: Some(InstrumentCategory::Stock),
        filter: Some(crate::domain::quotes::ScanFilter::TopGain),
        conditions: None,
        sort_by: None,
        limit: Some(10),
    });
    eprintln!(
        "[H3] scan top_gain: universe.total={} matched={} validQuote={:?} items={}",
        scan.result.universe.total, scan.result.universe.matched, scan.result.universe.valid_quote_count, scan.result.items.len()
    );
    // scan 排名：rank 单调递增（从 1）。
    for (i, it) in scan.result.items.iter().enumerate() {
        assert_eq!(it.rank as usize, i + 1, "rank 应从 1 连续递增");
    }

    let breadth = svc.market_breadth();
    eprintln!(
        "[H3] breadth total={} up={} down={} flat={} limitUp={} limitDown={} noData={}",
        breadth.total, breadth.up, breadth.down, breadth.flat, breadth.limit_up, breadth.limit_down, breadth.no_data
    );
    // 不变量：up + down + flat == total。
    assert_eq!(breadth.up + breadth.down + breadth.flat, breadth.total, "breadth: up+down+flat==total");
    assert!(breadth.limit_up <= breadth.up, "limitUp 是 up 子集");
    assert!(breadth.limit_down <= breadth.down, "limitDown 是 down 子集");

    let heatmap = svc.industry_heatmap(5);
    eprintln!(
        "[H3] heatmap topGainers={} topLosers={} tradeDate={}",
        heatmap.top_gainers.len(), heatmap.top_losers.len(), heatmap.trade_date.format()
    );
}

/// H4 · refresh_market_quotes(universe) 主体完成 + emit refreshed。Provider: TDX/腾讯。
///
/// 全市场刷新是吞吐敏感路径；这里 seed 一小批 universe 验证执行契约（不跑真 7500）。
#[tokio::test]
#[ignore]
async fn quotes_live_pipeline_refresh_universe_smoke() {
    let svc = make_service();
    for s in ["600519.SH", "000001.SZ", "600036.SH"] {
        seed_instrument(&svc, s, s, InstrumentCategory::Stock);
    }
    seed_instrument(&svc, INDEX_SH, "上证指数", InstrumentCategory::Index);
    seed_instrument(&svc, ETF_SH, "沪深300ETF", InstrumentCategory::Fund);

    // 收集 emit 的 refreshed payloads（universe 应至少 emit 一条同步首条）。
    let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_codes = collected.clone();
    svc.set_event_sink(Arc::new(move |p| {
        sink_codes.lock().unwrap().push((p.scope, p.total, p.success, p.failed_batches));
    }));

    let req = RefreshMarketQuotesRequest {
        scope: RefreshMarketQuotesScope::Universe,
        purpose: RefreshPurpose::Intraday,
        trade_date: None,
    };
    match svc.refresh_market_quotes(req).await {
        Ok(payload) => {
            eprintln!(
                "[H4] universe 主体完成 total={} success={} failedBatches={}",
                payload.total, payload.success, payload.failed_batches
            );
            assert!(payload.total > 0, "universe 应有目标标的");
            let events = collected.lock().unwrap();
            eprintln!("[H4] emit refreshed 条数 = {} → {:?}", events.len(), *events);
            assert!(!events.is_empty(), "universe scope 应 emit 至少一条 refreshed");
        }
        Err(e) => eprintln!("[H4] SKIP — universe refresh 失败: {:?}", e),
    }
}

/// H5 · 非交易时段 close snapshot fallback：refresh(close) 后非交易时段 list 能读回。
/// Provider: TDX/腾讯。
#[tokio::test]
#[ignore]
async fn quotes_live_pipeline_close_snapshot_fallback() {
    let svc = make_service();
    seed_instrument(&svc, OLD_STOCK_SH, "贵州茅台", InstrumentCategory::Stock);
    let code = ts(OLD_STOCK_SH);
    let ctx = svc.market_time_now();
    eprintln!("[H5] market time: isTradingTime={} now={}", ctx.is_trading_time, ctx.now);

    let req = RefreshMarketQuotesRequest {
        scope: RefreshMarketQuotesScope::Manual { ts_codes: vec![code.clone()] },
        purpose: RefreshPurpose::Close,
        trade_date: None,
    };
    match svc.refresh_market_quotes(req).await {
        Ok(payload) => {
            eprintln!("[H5] close refresh total={} success={}", payload.total, payload.success);
            // close_snapshot_complete 用 refresh_state 判定（manual scope universe_size 可能为 0）。
            let elig = crate::domain::quotes::eligible_trade_date(&svc.market_time_now());
            let complete = svc.close_snapshot_complete(elig.trade_date).await;
            eprintln!("[H5] close_snapshot_complete({}) = {}", elig.trade_date.format(), complete);
        }
        Err(e) => eprintln!("[H5] SKIP — close refresh 失败: {:?}", e),
    }
}
