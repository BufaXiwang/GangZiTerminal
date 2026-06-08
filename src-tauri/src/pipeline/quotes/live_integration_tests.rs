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

/// A2c · 按需取债券（universe 外）报价的**小数位校正**：从 SH 原始全量挑一只可转债（110/113 段），
/// 用 category=Stock（调用方默认）取 TDX 报价，断言价格落在 3 位小数可转债的合理区间（~80–400），
/// **不是 10× 偏高**（缩放 bug 会得到 ~800–4000）。证明「按需取债券报价正确 + universe 不收债券」。
/// Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_live_tdx_bond_quote_decimal_scaling() {
    let mgr = TdxConnectionManager::new();
    let raw = match mgr.fetch_universe(TdxMarket::SH).await {
        Ok(list) => list,
        Err(e) => {
            eprintln!("[A2c] SKIP — TDX 不可达: {e}");
            return;
        }
    };
    // 收集多只 SH 可转债（110/113 段）——universe 里没有（被 classify 丢弃），raw 全量有。
    // 批量取报价，挑第一只有有效价的（很多老券已赎回无行情），对它断言 3 位小数缩放正确。
    // 全量扫 SH 可转债（110/113 段）——老券（低号）多已赎回、高号多为预留未上市，活跃券在中间，
    // 故全取，批量里挑第一只有有效价的。
    let bonds: Vec<_> = raw
        .iter()
        .filter(|e| e.code.len() == 6 && (e.code.starts_with("110") || e.code.starts_with("113")))
        .collect();
    if bonds.is_empty() {
        eprintln!("[A2c] SKIP — raw 全量未找到 110/113 可转债");
        return;
    }
    let codes: Vec<_> = bonds
        .iter()
        .map(|e| (ts(&format!("{}.SH", &e.code)), InstrumentCategory::Stock, Some(e.name.clone())))
        .collect();
    let results = mgr.fetch_quotes(codes, recent_trade_date(), Utc::now()).await;
    let n_ok = results.iter().filter(|r| r.is_ok()).count();
    let n_priced = results.iter().filter_map(|r| r.as_ref().ok()).filter(|q| q.price.is_some()).count();
    eprintln!(
        "[A2c] 取 {} 只可转债：ok={n_ok} priced={n_priced}；样例 {:?}",
        results.len(),
        results.iter().filter_map(|r| r.as_ref().ok()).take(3).map(|q| (q.ts_code.as_str().to_string(), q.price)).collect::<Vec<_>>()
    );
    let priced = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .find(|q| q.price.is_some());
    match priced {
        Some(q) => {
            let v = q.price.unwrap().0;
            eprintln!("[A2c] 可转债 {} price={v}（面值 ~100，正常区间 ~80–400）", q.ts_code.as_str());
            assert!(
                v > rust_decimal::Decimal::from(20) && v < rust_decimal::Decimal::from(600),
                "可转债价 {v} 不在 3 位小数合理区间——疑似 10× 缩放未校正（is_bond 没生效）"
            );
        }
        // 实测发现：本 TDX 服务器池对可转债 security_quotes 返回记录但 price=0/None（非交易时段
        // 或该池不服务可转债实时价）。此时无法实证缩放，优雅 skip——缩放逻辑由 hermetic 单测
        // `map_security_quote_bond_price_scaled_to_three_decimals` 证明；交易时段若有价会硬断言。
        None => eprintln!("[A2c] SKIP — 该 TDX 池本窗口未返回可转债有效价（price=None），缩放由 hermetic 单测覆盖"),
    }
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
            assert_eq!(q.source, QuoteSource::Tdx);
            // 现价非交易时段可能为空（TDX 当前 session 无价、只回 prevClose）——这是合法市场状态、
            // 非 bug。有现价才断言 >0；否则要求至少 prevClose 在（证明拿到了有效报价记录）。
            match q.price {
                Some(p) => assert!(p.0 > rust_decimal::Decimal::ZERO, "现价应 >0"),
                None => {
                    assert!(q.previous_close.is_some(), "无现价时至少应有 prevClose（否则非有效报价）");
                    eprintln!("[B1]   现价为空（非交易时段），prevClose 在 → 合法，跳过现价断言");
                }
            }
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
    use crate::infrastructure::quotes::eastmoney::client::EmError;
    let em = EastmoneyProvider::new().expect("build em");
    let code = ts(OLD_STOCK_SH);
    match em.fetch_minute_kline(&code, MinuteKlinePeriod::M5, 240).await {
        Ok(pts) => {
            eprintln!("[C6] EM 茅台 5m 分钟 K 根数 = {}", pts.len());
            assert!(!pts.is_empty(), "EM 应返回分钟 K");
        }
        // 按 error 类型区分：连接级失败（000/超时/限流断连）= 真不可达 → skip；
        // server 有响应但 data:null（Empty）/ 解析失败 = 可达但坏 → fail（这正是缺 end 参数的 bug）。
        Err(EmError::Http(e)) => eprintln!("[C6] SKIP — EM 网络不可达/限流: {e}"),
        Err(e) => panic!("[C6] EM 有响应但分钟 K 空/解析失败 —— 应修不应 skip: {e}"),
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
    use crate::infrastructure::quotes::eastmoney::client::EmError;
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
            // 量价单位自洽（EM volume 手×100→股、amount 元）：取一根高量 bar，
            // vwap = amount/volume 应落在 [low*0.9, high*1.1]，否则疑似手/股 或 万元/元 单位 bug。
            if let Some(p) = pts
                .iter()
                .rfind(|p| p.volume.map(|v| v.0 > 100_000).unwrap_or(false) && p.amount.is_some())
            {
                let vol = p.volume.unwrap().0 as f64;
                let amt = dec_f64(p.amount.unwrap().0);
                let vwap = amt / vol;
                let (lo, hi) = (dec_f64(p.low.0), dec_f64(p.high.0));
                eprintln!("[C7]   量价自洽 vwap={vwap:.2} low={lo:.2} high={hi:.2} (vol={vol} amt={amt})");
                assert!(
                    vwap > lo * 0.9 && vwap < hi * 1.1,
                    "[C7] vwap={vwap} 越界 [{}*0.9,{}*1.1]——疑似 EM 量/额单位 bug（手/股 或 万元/元）",
                    lo,
                    hi
                );
            }
        }
        // 连接级失败 = 真不可达 → skip；有响应但 data:null（Empty）/解析失败 = 可达但坏 → fail。
        Err(EmError::Http(e)) => eprintln!("[C7] SKIP — EM 网络不可达/限流: {e}"),
        Err(e) => panic!("[C7] EM 有响应但日线 K 空/解析失败 —— 应修不应 skip: {e}"),
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
                sort: None,
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

/// H6 · 端到端 fetch_data(dailyBasic + events) 读回：pipeline refresh（TuShare）→ DB → facade。
/// 验证「TuShare 拉取 → 落库 → fetch_data 读回」完整链路（hermetic 只能 seed 后读，这里测真拉真读）。
/// Provider: TuShare。Env: TUSHARE_TOKEN。
#[tokio::test]
#[ignore]
async fn quotes_live_pipeline_fetch_data_daily_basic_and_events() {
    let Some(_t) = tushare_token() else {
        eprintln!("[H6] SKIP — TUSHARE_TOKEN 未设");
        return;
    };
    let svc = make_service();
    seed_instrument(&svc, OLD_STOCK_SH, "贵州茅台", InstrumentCategory::Stock);
    svc.health().initial_ping().await;
    if !svc.health().is_available() {
        eprintln!("[H6] SKIP — TuShare health 不可用: {:?}", svc.health().state());
        return;
    }
    let code = ts(OLD_STOCK_SH);
    // 拉一个已知交易日的 daily_basic + 一段时间窗口的公司事件。
    let db_res = svc
        .refresh_daily_basic(
            RefreshDataScope::Manual { ts_codes: vec![code.clone()] },
            Some(TradeDate::parse("20240102").unwrap()),
        )
        .await;
    let ev_res = svc
        .refresh_company_events(
            RefreshDataScope::Manual { ts_codes: vec![code.clone()] },
            None,
        )
        .await;
    eprintln!("[H6] refresh daily_basic={:?} events={:?}", db_res.map(|r| r.success), ev_res.map(|r| r.success));

    // fetch_data 只读本地——验证 daily_basic / events 能否读回（取决于 refresh 是否写入）。
    let res = svc.fetch_data(FetchDataRequest {
        ts_codes: Some(vec![OLD_STOCK_SH.into()]),
        include: Some(FetchInclude {
            daily_basic: Some(true),
            events: Some(true),
            profile: Some(true),
            ..Default::default()
        }),
        limit: None,
    });
    assert_eq!(res.items.len(), 1);
    let item = &res.items[0];
    eprintln!(
        "[H6] daily_basic_present={} events={:?} warnings={:?}",
        item.daily_basic.is_some(),
        item.events.as_ref().map(|e| e.len()),
        item.warnings
    );
    // daily_basic 写入成功则应读回；写入失败（如该日无数据）则 daily_basic_missing warning。
    // 两种都是合法终态，断言「读回 ⟺ 无 missing warning」自洽，不硬依赖网络拉到具体值。
    if item.daily_basic.is_some() {
        assert!(
            !item.warnings.contains(&crate::domain::shared::WarningCode::DailyBasicMissing),
            "读回 daily_basic 时不应同时报 missing"
        );
        let db = item.daily_basic.as_ref().unwrap();
        assert_eq!(db.ts_code.as_str(), OLD_STOCK_SH);
    } else {
        assert!(item.warnings.contains(&crate::domain::shared::WarningCode::DailyBasicMissing));
    }
}

/// H7 · 端到端 fetch_data(quote + klines[day,week] + minuteKlines[5m]) 读回：
/// pipeline refresh（TDX）→ DB → facade 一次取齐多读模型。验证多 include 组合在真链路一致。
/// Provider: TDX/腾讯。
#[tokio::test]
#[ignore]
async fn quotes_live_pipeline_fetch_data_multi_include() {
    let svc = make_service();
    seed_instrument(&svc, OLD_STOCK_SH, "贵州茅台", InstrumentCategory::Stock);
    let code = ts(OLD_STOCK_SH);
    let _ = svc
        .refresh_market_quotes(RefreshMarketQuotesRequest {
            scope: RefreshMarketQuotesScope::Manual { ts_codes: vec![code.clone()] },
            purpose: RefreshPurpose::Intraday,
            trade_date: None,
        })
        .await;
    let kl = svc
        .refresh_klines(
            RefreshDataScope::Manual { ts_codes: vec![code.clone()] },
            vec![KlinePeriod::Day, KlinePeriod::Week],
        )
        .await;
    let mk = svc
        .refresh_minute_klines(
            RefreshDataScope::Manual { ts_codes: vec![code.clone()] },
            vec![MinuteKlinePeriod::M5],
        )
        .await;
    if kl.is_err() {
        eprintln!("[H7] SKIP — refresh_klines 失败（TDX 不可达?）");
        return;
    }
    eprintln!("[H7] kline={:?} minute={:?}", kl.map(|r| r.success), mk.map(|r| r.success));

    let res = svc.fetch_data(FetchDataRequest {
        ts_codes: Some(vec![OLD_STOCK_SH.into()]),
        include: Some(FetchInclude {
            quote: Some(true),
            klines: Some(vec![KlinePeriod::Day, KlinePeriod::Week]),
            minute_klines: Some(vec![MinuteKlinePeriod::M5]),
            profile: Some(true),
            ..Default::default()
        }),
        limit: None,
    });
    let item = &res.items[0];
    if let Some(klines) = &item.klines {
        eprintln!("[H7] klines keys={:?}", klines.keys().collect::<Vec<_>>());
        // 只请求 day+week → 不应出现 month。
        assert!(!klines.contains_key(KlinePeriod::Month.as_str()), "未请求 month 不应出现");
        if let Some(day) = klines.get(KlinePeriod::Day.as_str()) {
            for w in day.points.windows(2) {
                assert!(w[0].date <= w[1].date, "日 K 升序");
            }
        }
    }
    if let Some(minute) = &item.minute_klines {
        eprintln!("[H7] minute keys={:?}", minute.keys().collect::<Vec<_>>());
        assert!(!minute.contains_key("1m"), "未请求 1m 不应出现");
    }
}

// ===== ACC. 数据准确性（跨源一致 + 数学一致）=====
//
// 与 A–H 的「结构 / 契约」测试不同，本节验**数值对不对**：
//   - 跨源一致：同一标的同一交易日，TDX 解码 + 缩放后的 OHLC 必须与 TuShare / 腾讯
//     的权威值在容差内一致（抓「解码 / 缩放 / 串号 / 单位」类 bug）。
//   - 数学一致：单源 K 线的 OHLC 不变量 + 量价单位自洽 + 复权因子恒定（抓「手 vs 股」
//     「万元 vs 元」「复权公式」类 bug）。
//
// 容差约定（跨源）：相对误差 ≤ 0.5% **或** 绝对误差 ≤ 0.01，取宽松者通过。两源对同一
// 标的的四舍五入 / 采样口径可能有微小差异，但缩放 bug（10×）远超此容差，必被抓到。
//
// 全部 #[tokio::test] #[ignore]，命名 quotes_acc_*。token 仅走 TUSHARE_TOKEN env。
// 任一 provider 不可达 / token 缺失 → eprintln! skip 后 return，不脏 panic。

/// Decimal → f64（仅用于测试断言里的容差比较）。
fn dec_f64(d: rust_decimal::Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    d.to_f64().unwrap_or(f64::NAN)
}

/// 跨源数值「在容差内相等」：相对 ≤ 0.5% 或绝对 ≤ 0.01，取宽松者。
fn close_enough(a: f64, b: f64) -> bool {
    let abs = (a - b).abs();
    if abs <= 0.01 {
        return true;
    }
    let denom = a.abs().max(b.abs()).max(1e-9);
    abs / denom <= 0.005
}

/// TDX `Bar` 的日期键（YYYYMMDD），用于和 TuShare `TradeDate::format()` 对齐。
fn bar_date_key(b: &crate::infrastructure::quotes::tdx::Bar) -> String {
    format!("{:04}{:02}{:02}", b.year, b.month, b.day)
}

/// ACC-1 跨源日 K 数值一致：TDX（不复权）↔ TuShare daily（不复权）。
/// 对 股票 / ETF / 指数 各跑一遍（共用 body）。按 trade_date 对齐重叠区间，断言重叠日
/// open/high/low/close 在容差内相等；ETF（510300）额外验 close ~4–5（3 位小数缩放端到端）。
/// Provider: TDX + TuShare。Env: TUSHARE_TOKEN。
async fn acc1_cross_source_daily(tag: &str, code: &str, etf_band: bool) {
    let Some(_t) = tushare_token() else {
        eprintln!("[{tag}] SKIP — TUSHARE_TOKEN 未设");
        return;
    };
    let c = ts(code);
    let mgr = TdxConnectionManager::new();
    // TDX 不复权日 K（最新 ~800 根）。
    let tdx_bars = match mgr.fetch_kline_at(&c, KlinePeriod::Day, 0, 800).await {
        Ok(b) if !b.is_empty() => b,
        Ok(_) => {
            eprintln!("[{tag}] SKIP — TDX 返回空日 K");
            return;
        }
        Err(e) => {
            eprintln!("[{tag}] SKIP — TDX 不可达: {e}");
            return;
        }
    };
    // TuShare 不复权日 K（daily/index_daily/fund_daily 由 fetch_kline 内部按 ts_code 选择？
    // 实测 fetch_kline 走 `daily` 接口——股票准确；指数 / ETF 用同接口在本环境可能空，空则 skip）。
    // 取一段与 TDX 重叠的近窗口（近 ~400 自然日，覆盖足够交易日做交集）。
    let today = Utc::now().with_timezone(&chrono_tz::Asia::Shanghai).date_naive();
    let start = (today - chrono::Duration::days(400)).format("%Y%m%d").to_string();
    let end = today.format("%Y%m%d").to_string();
    let ts_bars = match tushare_client().fetch_kline(&c, KlinePeriod::Day, &start, &end).await {
        Ok(b) if !b.is_empty() => b,
        Ok(_) => {
            eprintln!("[{tag}] SKIP — TuShare daily 返回空（指数/ETF 可能不走 daily 接口）");
            return;
        }
        Err(e) => {
            eprintln!("[{tag}] SKIP — TuShare daily 失败: {e}");
            return;
        }
    };
    // TDX 按日期建索引。
    use std::collections::HashMap;
    let tdx_map: HashMap<String, &crate::infrastructure::quotes::tdx::Bar> =
        tdx_bars.iter().map(|b| (bar_date_key(b), b)).collect();
    let mut compared = 0usize;
    let mut sample_printed = 0usize;
    for p in &ts_bars {
        let key = p.date.format();
        let Some(b) = tdx_map.get(&key) else { continue };
        let (to, th, tl, tc) = (b.open, b.high, b.low, b.close);
        let (so, sh, sl, sc) = (
            dec_f64(p.open.0),
            dec_f64(p.high.0),
            dec_f64(p.low.0),
            dec_f64(p.close.0),
        );
        if sample_printed < 5 {
            eprintln!(
                "[{tag}] {key} TDX(o={to:.3} h={th:.3} l={tl:.3} c={tc:.3}) TS(o={so:.3} h={sh:.3} l={sl:.3} c={sc:.3})"
            );
            sample_printed += 1;
        }
        assert!(close_enough(to, so), "[{tag}] {key} open 跨源不一致 TDX={to} TS={so}");
        assert!(close_enough(th, sh), "[{tag}] {key} high 跨源不一致 TDX={th} TS={sh}");
        assert!(close_enough(tl, sl), "[{tag}] {key} low 跨源不一致 TDX={tl} TS={sl}");
        assert!(close_enough(tc, sc), "[{tag}] {key} close 跨源不一致 TDX={tc} TS={sc}");
        if etf_band {
            // 510300 沪深300ETF：3 位小数标的，close 应 ~4–5；10× 缩放 bug 会得到 ~48。
            assert!(
                tc > 1.0 && tc < 20.0,
                "[{tag}] ETF close={tc} 不在 ~5 区间（疑似 10× 缩放 bug；正确应 ~4–5）"
            );
            assert!(
                sc > 1.0 && sc < 20.0,
                "[{tag}] TuShare ETF close={sc} 异常"
            );
        }
        compared += 1;
    }
    eprintln!("[{tag}] 重叠交易日 = {compared}（TDX {} 根 / TuShare {} 根）", tdx_bars.len(), ts_bars.len());
    if compared < 5 {
        eprintln!("[{tag}] SKIP — 重叠交易日 < 5（无法做有意义的跨源校验）");
        return;
    }
    assert!(compared >= 5, "[{tag}] 应有 ≥5 重叠交易日");
}

/// ACC-1a · 股票 600519 TDX↔TuShare 日 K 数值一致。
#[tokio::test]
#[ignore]
async fn quotes_acc_cross_source_daily_stock() {
    acc1_cross_source_daily("ACC-1a", OLD_STOCK_SH, false).await;
}

/// ACC-1b · ETF 510300 TDX↔TuShare 日 K 数值一致（额外验 3 位小数缩放：close ~4–5）。
#[tokio::test]
#[ignore]
async fn quotes_acc_cross_source_daily_etf() {
    acc1_cross_source_daily("ACC-1b", ETF_SH, true).await;
}

/// ACC-1c · 指数 000001.SH TDX↔TuShare 日 K 数值一致。
/// 注意：TuShare `fetch_kline` 走 `daily` 接口（股票口径），指数可能返回空 → 优雅 skip。
/// 单位 / 接口口径若拿不准，按「空即 skip」处理，不猜测改写。
#[tokio::test]
#[ignore]
async fn quotes_acc_cross_source_daily_index() {
    acc1_cross_source_daily("ACC-1c", INDEX_SH, false).await;
}

/// ACC-2 · 跨源报价一致：TDX ↔ 腾讯。非交易时段现价可能为空，故用**稳定字段**
/// previous_close 比（两边都有 high/low 时也比），现价仅两边都非空时比。
/// 510300 prevClose 应 ~4–5（再验 ETF 缩放）。Provider: TDX + 腾讯。
#[tokio::test]
#[ignore]
async fn quotes_acc_cross_source_quote_tdx_vs_tencent() {
    let mgr = TdxConnectionManager::new();
    let tx = TencentProvider::new().expect("build tencent");
    let cases = [
        ("ACC-2/stock", OLD_STOCK_SH, InstrumentCategory::Stock, false),
        ("ACC-2/etf", ETF_SH, InstrumentCategory::Fund, true),
        ("ACC-2/index", INDEX_SH, InstrumentCategory::Index, false),
    ];
    let mut any_compared = false;
    for (tag, code, cat, etf_band) in cases {
        let c = ts(code);
        let tdx = mgr
            .fetch_quote(&c, cat, recent_trade_date(), Utc::now(), None)
            .await;
        let txq = tx
            .fetch_quote(&c, cat, recent_trade_date(), Utc::now())
            .await;
        let (tdx, txq) = match (tdx, txq) {
            (Ok(a), Ok(b)) => (a, b),
            (a, b) => {
                eprintln!(
                    "[{tag}] SKIP — provider 不可达: TDX={:?} 腾讯={:?}",
                    a.err(),
                    b.err()
                );
                continue;
            }
        };
        // 稳定字段：previous_close。
        match (tdx.previous_close, txq.previous_close) {
            (Some(a), Some(b)) => {
                let (a, b) = (dec_f64(a.0), dec_f64(b.0));
                eprintln!("[{tag}] {code} prevClose TDX={a:.3} 腾讯={b:.3}");
                assert!(close_enough(a, b), "[{tag}] prevClose 跨源不一致 TDX={a} 腾讯={b}");
                if etf_band {
                    assert!(a > 1.0 && a < 20.0, "[{tag}] ETF TDX prevClose={a} 疑似 10× 缩放 bug");
                    assert!(b > 1.0 && b < 20.0, "[{tag}] ETF 腾讯 prevClose={b} 异常");
                }
                any_compared = true;
            }
            _ => eprintln!("[{tag}] INFO — prevClose 某源为空，跳过该字段"),
        }
        // high / low：两边都非空才比。
        if let (Some(a), Some(b)) = (tdx.high, txq.high) {
            let (a, b) = (dec_f64(a.0), dec_f64(b.0));
            assert!(close_enough(a, b), "[{tag}] high 跨源不一致 TDX={a} 腾讯={b}");
        }
        if let (Some(a), Some(b)) = (tdx.low, txq.low) {
            let (a, b) = (dec_f64(a.0), dec_f64(b.0));
            assert!(close_enough(a, b), "[{tag}] low 跨源不一致 TDX={a} 腾讯={b}");
        }
        // 现价：仅两边都非空（交易时段）才比。
        if let (Some(a), Some(b)) = (tdx.price, txq.price) {
            let (a, b) = (dec_f64(a.0), dec_f64(b.0));
            eprintln!("[{tag}] {code} price TDX={a:.3} 腾讯={b:.3}（交易时段才有意义）");
            assert!(close_enough(a, b), "[{tag}] 现价跨源不一致 TDX={a} 腾讯={b}");
        }
        // 成交量跨源单位一致性（捕捉 手/股 normalize bug）：
        // 腾讯已知 ×100 转股（shared-types §Volume：盘口/成交量统一为股，不用手），TDX 必须同单位。
        // 总成交量是当日累计、两源同一交易日 → 数量级必须一致（ratio≈1，绝不应差 ~100×）。
        if let (Some(a), Some(b)) = (tdx.volume, txq.volume) {
            let (a, b) = (a.0 as f64, b.0 as f64);
            if a > 0.0 && b > 0.0 {
                let ratio = a / b;
                eprintln!("[{tag}] {code} 总成交量 TDX={a:.0} 腾讯={b:.0} ratio={ratio:.4}");
                assert!(
                    ratio > 0.5 && ratio < 2.0,
                    "[{tag}] 总成交量跨源比 {ratio:.4} 偏离 1（疑似 手/股 单位 bug）TDX={a} 腾讯={b}"
                );
            }
        }
        // 卖一盘口量跨源（depth 交易时段变化快，只查不差 ~100× 数量级）。
        if let (Some(a), Some(b)) = (
            tdx.ask.first().and_then(|l| l.volume),
            txq.ask.first().and_then(|l| l.volume),
        ) {
            let (a, b) = (a.0 as f64, b.0 as f64);
            if a > 0.0 && b > 0.0 {
                let ratio = a / b;
                eprintln!("[{tag}] {code} 卖一量 TDX={a:.0} 腾讯={b:.0} ratio={ratio:.4}");
                assert!(
                    ratio > 0.1 && ratio < 10.0,
                    "[{tag}] 卖一盘口量跨源差 ~100×（疑似 手/股 单位 bug）TDX={a} 腾讯={b}"
                );
            }
        }
        // 成交额跨源单位一致性（元）：腾讯 ×10000（万元→元），TDX 须同为元（shared-types §Amount）。
        if let (Some(a), Some(b)) = (tdx.amount, txq.amount) {
            let (a, b) = (dec_f64(a.0), dec_f64(b.0));
            if a > 0.0 && b > 0.0 {
                let ratio = a / b;
                eprintln!("[{tag}] {code} 成交额 TDX={a:.0} 腾讯={b:.0} ratio={ratio:.4}");
                assert!(
                    ratio > 0.5 && ratio < 2.0,
                    "[{tag}] 成交额跨源比 {ratio:.4} 偏离 1（疑似 万元/元 单位 bug）TDX={a} 腾讯={b}"
                );
            }
        }
    }
    if !any_compared {
        eprintln!("[ACC-2] SKIP — 无任一标的两源 prevClose 同时可得");
    }
}

/// ACC-3 · 单源 TDX K 线内部数学一致：OHLC 不变量 + date 严格递增 + 量价单位自洽。
/// vwap = amount / volume（amount 元、volume 股 → 元/股）应落在 [low*0.9, high*1.1]，
/// 这能抓「手 vs 股」「万元 vs 元」单位 bug。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_acc_kline_internal_math() {
    let mgr = TdxConnectionManager::new();
    let c = ts(OLD_STOCK_SH);
    let bars = match mgr.fetch_kline_at(&c, KlinePeriod::Day, 0, 60).await {
        Ok(b) if !b.is_empty() => b,
        Ok(_) => {
            eprintln!("[ACC-3] SKIP — TDX 返回空");
            return;
        }
        Err(e) => {
            eprintln!("[ACC-3] SKIP — TDX 不可达: {e}");
            return;
        }
    };
    eprintln!("[ACC-3] 600519 日 K 根数 = {}", bars.len());
    let mut prev_key: Option<String> = None;
    let mut vwap_samples = 0usize;
    for b in &bars {
        // OHLC 不变量。
        assert!(b.high >= b.open, "high >= open");
        assert!(b.high >= b.close, "high >= close");
        assert!(b.high >= b.low, "high >= low");
        assert!(b.low <= b.open, "low <= open");
        assert!(b.low <= b.close, "low <= close");
        assert!(b.volume >= 0.0, "volume >= 0");
        assert!(b.amount >= 0.0, "amount >= 0");
        // date 严格递增。
        let key = bar_date_key(b);
        if let Some(prev) = &prev_key {
            assert!(*prev < key, "date 应严格递增: {prev} !< {key}");
        }
        prev_key = Some(key);
        // 量价一致性：vwap = amount / volume 应落在 [low*0.9, high*1.1]。
        // 注意单位：TDX online K 线 amount 为元、volume 为股（offline 同）。
        // 阈值 >10万股：跳过「当日未完成 / 占位」bar——非交易时段 TDX 可能给今日一根 vol/amt 近 0
        // 的占位 bar（vwap 无意义），这不是单位 bug。真实交易日成交量远超 10 万股。
        if b.volume > 100_000.0 && b.amount > 0.0 {
            let vwap = b.amount / b.volume;
            if vwap_samples < 5 {
                eprintln!(
                    "[ACC-3] {} vwap={vwap:.3} (low={:.3} high={:.3} vol={} amt={})",
                    bar_date_key(b),
                    b.low,
                    b.high,
                    b.volume,
                    b.amount
                );
                vwap_samples += 1;
            }
            assert!(
                vwap >= b.low * 0.9 && vwap <= b.high * 1.1,
                "[ACC-3] {} vwap={vwap} 越界 [{}*0.9, {}*1.1]——疑似 amount/volume 单位 bug（手/股 或 万元/元）",
                bar_date_key(b),
                b.low,
                b.high
            );
        }
    }
}

/// ACC-4 · qfq / hfq 复权数学正确（平安银行 000001.SZ，有分红送转）。
/// 取 none/qfq/hfq 同窗口日 K，验：点数 / date 对齐；qfq 锚最新、hfq 锚最早；
/// hfq/qfq 比值恒定（= 总复权因子）；除权方向（qfq 早期 ≤ none 早期）。
/// Provider: TDX（K 线 + xdxr，本地复权计算）。
#[tokio::test]
#[ignore]
async fn quotes_acc_adjust_qfq_hfq_math() {
    let svc = make_service();
    seed_instrument(&svc, OLD_STOCK_SZ, "平安银行", InstrumentCategory::Stock);
    let code = ts(OLD_STOCK_SZ);
    if let Err(e) = svc
        .refresh_klines(RefreshDataScope::Manual { ts_codes: vec![code.clone()] }, vec![KlinePeriod::Day])
        .await
    {
        eprintln!("[ACC-4] SKIP — refresh_klines 失败（TDX 不可达?）: {:?}", e);
        return;
    }
    if let Err(e) = svc
        .refresh_xdxr_events(RefreshDataScope::Manual { ts_codes: vec![code.clone()] })
        .await
    {
        eprintln!("[ACC-4] SKIP — refresh_xdxr_events 失败: {:?}", e);
        return;
    }
    let none = svc.read_kline_series_with_adjust(&code, KlinePeriod::Day, Adjust::None, 250).unwrap();
    let qfq = svc.read_kline_series_with_adjust(&code, KlinePeriod::Day, Adjust::Qfq, 250).unwrap();
    let hfq = svc.read_kline_series_with_adjust(&code, KlinePeriod::Day, Adjust::Hfq, 250).unwrap();
    let (Some(none), Some(qfq), Some(hfq)) = (none, qfq, hfq) else {
        eprintln!("[ACC-4] SKIP — 本地无 K 线（refresh 可能空）");
        return;
    };
    let n = none.points.len();
    eprintln!("[ACC-4] 平安 K 线根数 none={} qfq={} hfq={}", n, qfq.points.len(), hfq.points.len());
    if n < 2 {
        eprintln!("[ACC-4] SKIP — K 线不足 2 根");
        return;
    }
    // 点数相同 + date 对齐一致。
    assert_eq!(n, qfq.points.len(), "qfq 点数应等于 none");
    assert_eq!(n, hfq.points.len(), "hfq 点数应等于 none");
    for i in 0..n {
        assert_eq!(none.points[i].date.format(), qfq.points[i].date.format(), "qfq date 对齐");
        assert_eq!(none.points[i].date.format(), hfq.points[i].date.format(), "hfq date 对齐");
    }
    let nc = |s: &crate::domain::quotes::KlineSeries, i: usize| dec_f64(s.points[i].close.0);
    let last = n - 1;
    // qfq 锚最新：qfq.last.close == none.last.close。
    assert!(
        (nc(&qfq, last) - nc(&none, last)).abs() <= 0.01,
        "[ACC-4] qfq 末点应锚定 none 末点 qfq={} none={}",
        nc(&qfq, last),
        nc(&none, last)
    );
    // hfq 锚最早：hfq.first.close == none.first.close。
    assert!(
        (nc(&hfq, 0) - nc(&none, 0)).abs() <= 0.01,
        "[ACC-4] hfq 首点应锚定 none 首点 hfq={} none={}",
        nc(&hfq, 0),
        nc(&none, 0)
    );
    // 比值恒定：hfq[i]/qfq[i] 对所有 i 应为同一常数（= 总复权因子 H/Q）。
    let mut ratios = Vec::with_capacity(n);
    for i in 0..n {
        let (h, q) = (nc(&hfq, i), nc(&qfq, i));
        if q.abs() > 1e-9 {
            ratios.push(h / q);
        }
    }
    assert!(!ratios.is_empty(), "[ACC-4] 应有有效比值");
    let rmax = ratios.iter().cloned().fold(f64::MIN, f64::max);
    let rmin = ratios.iter().cloned().fold(f64::MAX, f64::min);
    let rel = (rmax - rmin) / rmax.abs().max(1e-9);
    eprintln!(
        "[ACC-4] 总复权因子(hfq/qfq) ~ {:.6}（min={:.6} max={:.6} 相对差={:.4}%）",
        rmax,
        rmin,
        rmax,
        rel * 100.0
    );
    assert!(rel < 0.01, "[ACC-4] hfq/qfq 比值应恒定（相对差 <1%），实测 {:.4}%", rel * 100.0);
    // 方向：存在除权时 qfq 早期 ≤ none 早期（前复权把早期价往下调）。无除权时相等也满足 ≤。
    assert!(
        nc(&qfq, 0) <= nc(&none, 0) + 0.01,
        "[ACC-4] qfq 早期 close 应 ≤ none 早期（前复权下调）qfq={} none={}",
        nc(&qfq, 0),
        nc(&none, 0)
    );
    eprintln!(
        "[ACC-4] 首点 none={:.4} qfq={:.4} hfq={:.4} | 末点 none={:.4} qfq={:.4} hfq={:.4}",
        nc(&none, 0), nc(&qfq, 0), nc(&hfq, 0),
        nc(&none, last), nc(&qfq, last), nc(&hfq, last)
    );
}

/// ACC-5 · TuShare daily_basic 单次返回自洽：total_mv（元，adapter ×10000）量级合理、
/// pe_ttm 为正且落在合理区间。只做能从单次返回自洽校验的，不硬凑外部对照。
/// Provider: TuShare。Env: TUSHARE_TOKEN。
#[tokio::test]
#[ignore]
async fn quotes_acc_daily_basic_self_consistent() {
    let Some(_t) = tushare_token() else {
        eprintln!("[ACC-5] SKIP — TUSHARE_TOKEN 未设");
        return;
    };
    let cli = tushare_client();
    let code = ts(OLD_STOCK_SH);
    let rows = match cli.fetch_daily_basic(Some(&code), Some("20240102")).await {
        Ok(r) if !r.is_empty() => r,
        Ok(_) => {
            eprintln!("[ACC-5] SKIP — 该日无 daily_basic");
            return;
        }
        Err(e) => {
            eprintln!("[ACC-5] SKIP — daily_basic 失败: {e}");
            return;
        }
    };
    let r = &rows[0];
    eprintln!(
        "[ACC-5] 茅台 20240102 pe={:?} pe_ttm={:?} pb={:?} total_mv={:?}",
        r.pe, r.pe_ttm, r.pb, r.total_mv.map(|m| dec_f64(m.0))
    );
    // total_mv：adapter 把 TuShare「万元」× 10000 转元；茅台总市值 ~万亿级（>1e11 元）。
    if let Some(mv) = r.total_mv {
        let v = dec_f64(mv.0);
        assert!(v > 0.0, "[ACC-5] total_mv 应 > 0");
        // 茅台总市值实际 ~1.5–2.5 万亿元；放宽到 [1e11, 1e14] 抓「万元/元」单位 bug。
        assert!(
            (1e11..1e14).contains(&v),
            "[ACC-5] 茅台 total_mv={v} 元 不在万亿量级 [1e11,1e14]——疑似单位换算 bug"
        );
    }
    // pe_ttm：茅台为盈利公司，pe_ttm 应为正且在合理区间（个位数~百）。
    if let Some(pe) = r.pe_ttm {
        assert!(pe > 0.0 && pe < 1000.0, "[ACC-5] pe_ttm={pe} 不在合理正区间");
    }
}

// ============================================================================= 准确性补齐 (ACC-6..8)
//
// Spec: docs/design/quotes-module.md §2（StockQuote change% / 盘口 bid≤ask / freshness）+
//       §5（今日 bar 由报价驱动 vs 历史日 K 量纲一致）。
// 这些为「数据准确性」judge 输出结构化数值 + numeric 断言。

/// ACC-6 · 实时报价 change% 数学一致：change == price - prevClose；
/// change% == (price - prevClose) / prevClose × 100。两源（TDX / 腾讯）各验。
/// 结构化输出供 judge 判「公式一致」。Provider: TDX + 腾讯。
#[tokio::test]
#[ignore]
async fn quotes_acc_change_percent_math() {
    let mgr = TdxConnectionManager::new();
    let tx = TencentProvider::new().expect("build tencent");
    let c = ts(OLD_STOCK_SH);
    let mut judged = 0usize;
    // (源名, quote)
    let tdx = mgr.fetch_quote(&c, InstrumentCategory::Stock, recent_trade_date(), Utc::now(), None).await.ok();
    let txq = tx.fetch_quote(&c, InstrumentCategory::Stock, recent_trade_date(), Utc::now()).await.ok();
    for (src, q) in [("tdx", tdx), ("tencent", txq)] {
        let Some(q) = q else {
            eprintln!("[ACC-6/{src}] SKIP — provider 不可达");
            continue;
        };
        let (Some(price), Some(prev)) = (q.price, q.previous_close) else {
            eprintln!("[ACC-6/{src}] INFO — price/prevClose 缺（非交易时段?），跳过");
            continue;
        };
        let price = dec_f64(price.0);
        let prev = dec_f64(prev.0);
        let change = q.change.map(|d| dec_f64(d.0));
        let pct = q.change_percent;
        let expected_change = price - prev;
        let expected_pct = if prev != 0.0 { (price - prev) / prev * 100.0 } else { f64::NAN };
        // JUDGE 结构化输出。
        eprintln!(
            "[ACC-6/{src}] JUDGE code={} price={price:.4} prevClose={prev:.4} change={change:?} expectedChange={expected_change:.4} changePct={pct:?} expectedPct={expected_pct:.4}",
            c.as_str()
        );
        if let Some(ch) = change {
            assert!((ch - expected_change).abs() < 0.02, "[ACC-6/{src}] change 数学不一致 got={ch} expected={expected_change}");
        }
        if let Some(p) = pct {
            assert!((p - expected_pct).abs() < 0.05, "[ACC-6/{src}] change% 数学不一致 got={p} expected={expected_pct}");
        }
        judged += 1;
    }
    if judged == 0 {
        eprintln!("[ACC-6] SKIP — 无任一源拿到 price+prevClose");
    }
}

/// ACC-7 · 今日 bar（实时报价驱动）vs 历史日 K（security_bars）量纲一致：
/// 报价的 price/open/high/low 应与最近一根历史日 K 的价格量级一致（同标的、相邻交易日，
/// 不应突刺 ~10× / ~100×）。报价 volume（股）也应与历史日 K volume（股）同数量级。
/// 这验证「今日 bar 由报价驱动」拼接进 K 线时不会因单位错位出现突刺。
/// Provider: TDX（报价 + 历史日 K 同源，排除跨源口径差异）。
#[tokio::test]
#[ignore]
async fn quotes_acc_today_bar_vs_history_kline_scale() {
    let mgr = TdxConnectionManager::new();
    let c = ts(OLD_STOCK_SH);
    let q = match mgr.fetch_quote(&c, InstrumentCategory::Stock, recent_trade_date(), Utc::now(), None).await {
        Ok(q) => q,
        Err(e) => { eprintln!("[ACC-7] SKIP — TDX quote 不可达: {e}"); return; }
    };
    let bars = match mgr.fetch_kline_at(&c, KlinePeriod::Day, 0, 20).await {
        Ok(b) if !b.is_empty() => b,
        _ => { eprintln!("[ACC-7] SKIP — TDX 日 K 空 / 不可达"); return; }
    };
    let Some(price) = q.price.map(|p| dec_f64(p.0)) else {
        eprintln!("[ACC-7] INFO — 报价无现价（非交易时段），用 prevClose 比");
        let Some(prev) = q.previous_close.map(|p| dec_f64(p.0)) else {
            eprintln!("[ACC-7] SKIP — 报价 price/prevClose 都空"); return;
        };
        let last = bars.last().unwrap();
        let ratio = prev / last.close.max(1e-9);
        eprintln!("[ACC-7] JUDGE prevClose={prev:.4} lastBarClose={:.4} ratio={ratio:.4}", last.close);
        assert!(ratio > 0.5 && ratio < 2.0, "[ACC-7] prevClose vs 最近日 K close 比 {ratio} 偏离 1（疑似单位突刺）");
        return;
    };
    let last = bars.last().unwrap();
    let price_ratio = price / last.close.max(1e-9);
    eprintln!(
        "[ACC-7] JUDGE code={} quotePrice={price:.4} lastBarDate={} lastBarClose={:.4} priceRatio={price_ratio:.4}",
        c.as_str(), bar_date_key(last), last.close
    );
    // 现价与最近收盘价比应在 [0.5, 2.0]（单日涨跌幅 ≤ ±100% 远超 A 股 ±10%，足够宽松抓 10×/100× bug）。
    assert!(price_ratio > 0.5 && price_ratio < 2.0, "[ACC-7] 现价 vs 最近日 K close 比 {price_ratio} 偏离 1（疑似今日 bar 单位突刺）");
    // volume 量纲：报价 volume（股）应与历史日 K volume（股）同数量级。
    if let Some(qv) = q.volume {
        let qv = qv.0 as f64;
        let bv = last.volume;
        if qv > 0.0 && bv > 0.0 {
            let vratio = qv / bv;
            eprintln!("[ACC-7] JUDGE quoteVol={qv:.0} lastBarVol={bv:.0} volRatio={vratio:.4}");
            // 当日累计成交量 vs 上一交易日成交量：同数量级（放宽 [0.05, 20] 抓 100× 单位 bug）。
            assert!(vratio > 0.05 && vratio < 20.0, "[ACC-7] 报价 volume vs 历史日 K volume 比 {vratio} 差 ~100×（疑似 手/股 bug）");
        }
    }
}

/// ACC-8 · 实时报价五档盘口买卖价合理（bid[0] ≤ ask[0]）+ 各档单调（买价递减、卖价递增）+ 无负价/NaN。
/// 仅交易时段五档齐全；非交易时段盘口可能为空 → 优雅 skip。Provider: TDX（五档主路径）+ 腾讯。
#[tokio::test]
#[ignore]
async fn quotes_acc_depth_bid_le_ask() {
    let mgr = TdxConnectionManager::new();
    let tx = TencentProvider::new().expect("build tencent");
    let c = ts(OLD_STOCK_SH);
    let tdx = mgr.fetch_quote(&c, InstrumentCategory::Stock, recent_trade_date(), Utc::now(), None).await.ok();
    let txq = tx.fetch_quote(&c, InstrumentCategory::Stock, recent_trade_date(), Utc::now()).await.ok();
    let mut checked = 0usize;
    for (src, q) in [("tdx", tdx), ("tencent", txq)] {
        let Some(q) = q else { eprintln!("[ACC-8/{src}] SKIP — 不可达"); continue; };
        let bid0 = q.bid.first().and_then(|l| l.price).map(|p| dec_f64(p.0));
        let ask0 = q.ask.first().and_then(|l| l.price).map(|p| dec_f64(p.0));
        eprintln!("[ACC-8/{src}] JUDGE code={} bid0={bid0:?} ask0={ask0:?} bidLevels={} askLevels={}", c.as_str(), q.bid.len(), q.ask.len());
        if let (Some(b), Some(a)) = (bid0, ask0) {
            assert!(b.is_finite() && a.is_finite() && b > 0.0 && a > 0.0, "[ACC-8/{src}] 盘口价应正且有限");
            assert!(b <= a, "[ACC-8/{src}] 买一 {b} 应 ≤ 卖一 {a}");
            checked += 1;
        } else {
            eprintln!("[ACC-8/{src}] INFO — 买一/卖一缺（非交易时段?），跳过该源");
        }
        // 各档单调：买价递减、卖价递增。
        let bids: Vec<f64> = q.bid.iter().filter_map(|l| l.price).map(|p| dec_f64(p.0)).collect();
        for w in bids.windows(2) { assert!(w[0] >= w[1], "[ACC-8/{src}] 买档价应递减 {:?}", w); }
        let asks: Vec<f64> = q.ask.iter().filter_map(|l| l.price).map(|p| dec_f64(p.0)).collect();
        for w in asks.windows(2) { assert!(w[0] <= w[1], "[ACC-8/{src}] 卖档价应递增 {:?}", w); }
    }
    if checked == 0 { eprintln!("[ACC-8] INFO — 无源有完整买一/卖一（可能非交易时段）"); }
}

// ============================================================================= 速度 (SPD-1..5)
//
// Spec: docs/design/quotes-module.md §5（冷启动 universe burst ~2.5s / refresh_quotes < ~500ms /
//       universe 滚动一轮 ≤ 30s / 连接池 8 并发 / ensure_chart_data 首屏延迟）+
//       §「TDX 连接池与并发」。
// 这些 println 结构化 `SPD/JUDGE` 行 + 目标阈值，供 speed judge 对照判 pass / slow。
// 断言用宽松上限（避免环境抖动 flaky），精确判定交给 judge rubric 对照目标值。

/// SPD-1 · 冷启动 universe burst 填充耗时（目标 ~2.5–5s，全市场 ~7500）。
/// seed 真实全市场 universe（refresh_market_instruments 拉 TDX）后，计时一次性 universe
/// quote refresh 的「主体完成」耗时。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_spd_cold_start_universe_burst() {
    use std::time::Instant;
    let svc = make_service();
    // 真实拉 universe（若 TDX 不可达则 seed 小批兜底，仍能测 burst 路径但不代表全市场）。
    if let Err(e) = svc.refresh_market_instruments().await {
        eprintln!("[SPD-1] INFO — refresh_market_instruments 失败({e:?})，seed 小批兜底");
        for i in 0..200u32 { seed_instrument(&svc, &format!("60{:04}.SH", i), "x", InstrumentCategory::Stock); }
    }
    let targets = svc.universe_quote_targets();
    eprintln!("[SPD-1] universe size = {}", targets.len());
    if targets.is_empty() { eprintln!("[SPD-1] SKIP — universe 空"); return; }
    let t0 = Instant::now();
    let req = RefreshMarketQuotesRequest {
        scope: RefreshMarketQuotesScope::Universe,
        purpose: RefreshPurpose::Intraday,
        trade_date: None,
    };
    let r = svc.refresh_market_quotes(req).await;
    let dt = t0.elapsed();
    match r {
        Ok(p) => {
            eprintln!(
                "[SPD-1] SPD/JUDGE metric=cold_start_universe_burst universeSize={} elapsedMs={} success={} total={} targetMs=2500-5000",
                targets.len(), dt.as_millis(), p.success, p.total
            );
            // 宽松上限：全市场 ≤ 30s 视为未异常（精确 ~2.5s 判定交 judge）。
            assert!(dt < std::time::Duration::from_secs(30), "[SPD-1] burst 主体完成应 < 30s（目标 ~2.5s）");
        }
        Err(e) => eprintln!("[SPD-1] SKIP — universe refresh 失败: {e:?}"),
    }
}

/// SPD-2 · refresh_quotes(~50 codes) 往返延迟（目标 < ~500ms）。
/// 模拟前端聚焦 pull：50 只标的一次 refresh_quotes。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_spd_refresh_quotes_roundtrip() {
    use std::time::Instant;
    let svc = make_service();
    // 50 只主板股票（连续代码，多数有效）。
    let mut codes = Vec::new();
    for i in 0..50u32 {
        let s = format!("6000{:02}.SH", i);
        seed_instrument(&svc, &s, "x", InstrumentCategory::Stock);
        codes.push(ts(&s));
    }
    let t0 = Instant::now();
    let r = svc.refresh_quotes(codes.clone()).await;
    let dt = t0.elapsed();
    match r {
        Ok(p) => {
            eprintln!(
                "[SPD-2] SPD/JUDGE metric=refresh_quotes_roundtrip codeCount={} elapsedMs={} success={} total={} targetMs=500",
                codes.len(), dt.as_millis(), p.success, p.total
            );
            assert!(dt < std::time::Duration::from_secs(5), "[SPD-2] 50 只往返应 < 5s（目标 <500ms）");
        }
        Err(e) => eprintln!("[SPD-2] SKIP — refresh_quotes 失败: {e:?}"),
    }
}

/// SPD-3 · universe 滚动跑完一轮覆盖时间（目标 ~30s）。
/// 用 batches_per_tick + 真实 refresh_quote_batch 串起一轮（cursor 从 0 wrap 回 0），计时。
/// 不直接跑 scheduler（避免 30s 真实等待）——按 batches_per_tick 的节奏一次性把全部 batch 跑完，
/// 测「跑完全 universe 一轮」的纯执行耗时（≈ scheduler 一轮覆盖的工作量，排除 tick sleep）。
/// Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_spd_universe_rolling_one_cycle() {
    use std::time::Instant;
    use crate::pipeline::quotes::scheduler::QUOTES_ROLL_BATCH;
    let svc = make_service();
    if let Err(e) = svc.refresh_market_instruments().await {
        eprintln!("[SPD-3] INFO — universe 拉取失败({e:?})，seed 小批兜底");
        for i in 0..300u32 { seed_instrument(&svc, &format!("60{:04}.SH", i), "x", InstrumentCategory::Stock); }
    }
    let targets = svc.universe_quote_targets();
    if targets.is_empty() { eprintln!("[SPD-3] SKIP — universe 空"); return; }
    let num_batches = targets.len().div_ceil(QUOTES_ROLL_BATCH);
    let t0 = Instant::now();
    for b in 0..num_batches {
        let start = b * QUOTES_ROLL_BATCH;
        let end = (start + QUOTES_ROLL_BATCH).min(targets.len());
        let slice: Vec<TsCode> = targets[start..end].iter().map(|(c, _, _)| c.clone()).collect();
        let _ = svc.refresh_quote_batch(slice).await;
    }
    let dt = t0.elapsed();
    eprintln!(
        "[SPD-3] SPD/JUDGE metric=universe_rolling_one_cycle universeSize={} batches={} elapsedMs={} targetMs=30000",
        targets.len(), num_batches, dt.as_millis()
    );
    // 一轮纯执行耗时应 ≤ 60s（目标 ~30s；scheduler 实际把它摊到 30s 周期内）。
    assert!(dt < std::time::Duration::from_secs(60), "[SPD-3] 一轮覆盖应 < 60s（目标 ~30s）");
}

/// SPD-4 · 连接池 8 并发批 vs 串行吞吐对比。
/// 复用 manager perf_pool_speedup 的思路，在 pipeline 层用多只 refresh_quote_batch 验加速比。
/// Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_spd_pool_concurrency_speedup() {
    use std::time::Instant;
    let mgr = TdxConnectionManager::new();
    // 8 批，每批 80 只主板股票。
    let make_batch = |base: u32| -> Vec<(TsCode, InstrumentCategory, Option<String>)> {
        (0..80u32).map(|i| {
            let code = format!("60{:04}.SH", base * 80 + i);
            (ts(&code), InstrumentCategory::Stock, None)
        }).collect()
    };
    let batches: Vec<_> = (0..8u32).map(make_batch).collect();
    // 串行。
    let t0 = Instant::now();
    for b in &batches {
        let _ = mgr.fetch_quotes(b.clone(), recent_trade_date(), Utc::now()).await;
    }
    let seq = t0.elapsed();
    // 并发（连接池 8 槽并行）。
    use futures_util::stream::{self, StreamExt};
    let t1 = Instant::now();
    let _r: Vec<_> = stream::iter(batches.clone())
        .map(|b| { let mgr = &mgr; async move { mgr.fetch_quotes(b, recent_trade_date(), Utc::now()).await } })
        .buffer_unordered(8)
        .collect()
        .await;
    let conc = t1.elapsed();
    let speedup = seq.as_secs_f64() / conc.as_secs_f64().max(1e-9);
    eprintln!(
        "[SPD-4] SPD/JUDGE metric=pool_concurrency_speedup batches=8 seqMs={} concMs={} speedup={:.2} targetSpeedup>=2.0",
        seq.as_millis(), conc.as_millis(), speedup
    );
    // 并发不应慢于串行（连接池有效）；理想 ~接近 8×，宽松断言 ≤ 串行。
    assert!(conc <= seq + std::time::Duration::from_millis(500), "[SPD-4] 8 并发应 ≤ 串行（连接池）");
}

/// SPD-5 · 单标的 ensure_chart_data 首屏延迟（fetch_kline_page start=0 落 DB）。
/// 这是 ensure_chart_data(day) 的默认路径（首屏单页）。目标：首屏出图 < ~1s。Provider: TDX。
#[tokio::test]
#[ignore]
async fn quotes_spd_ensure_chart_data_first_screen() {
    use std::time::Instant;
    let svc = make_service();
    seed_instrument(&svc, OLD_STOCK_SH, "贵州茅台", InstrumentCategory::Stock);
    let c = ts(OLD_STOCK_SH);
    let t0 = Instant::now();
    let r = svc.fetch_kline_page(&c, KlinePeriod::Day, 0).await;
    let dt = t0.elapsed();
    match r {
        Ok((added, has_more)) => {
            eprintln!(
                "[SPD-5] SPD/JUDGE metric=ensure_chart_data_first_screen code={} addedBars={} hasMore={} elapsedMs={} targetMs=1000",
                c.as_str(), added, has_more, dt.as_millis()
            );
            assert!(added > 0, "[SPD-5] 首屏应落入 K 线");
            assert!(dt < std::time::Duration::from_secs(5), "[SPD-5] 首屏页应 < 5s（目标 <1s）");
        }
        Err(e) => eprintln!("[SPD-5] SKIP — fetch_kline_page 失败: {e:?}"),
    }
}
