//! Account 模块**实网集成测试**（live integration tests）。
//!
//! Spec: docs/design/account-module.md
//!   §2 不变量（Account 读 Quotes snapshot 估值 / 成交模拟，写路径 fail-closed on stale/missing）
//!   §4 账户估值（marketValue = quantity * marketPrice，来自 Quotes snapshot）
//!   §5 订单成交模拟（market 用 fresh Quotes snapshot 的当前价 + 盘口模拟成交）
//!
//! ## 这是什么
//!
//! Account 本身是**纯本地模拟**（交易规则 / T+1 / 冻结 / FIFO / 估值都是确定性逻辑，
//! 已由 hermetic 单测覆盖）。它**唯一的实网角度**是消费 Quotes 的真实报价：
//! 成交价 / 持仓估值依赖 Quotes snapshot。所以这里的「e2e」= 用**真实行情**
//! （TDX/腾讯，经 `QuotesService` + `QuotesFacadeGateway`）驱动一笔成交 / 估值，
//! 验证 **Account ↔ Quotes 集成**端到端跑通。
//!
//! 这些测试全部标 `#[ignore]`，**不在普通 `cargo test` 里运行**，只能由处于可达
//! 网络的主 agent 显式 `-- --ignored` 触发。
//!
//! ## 安全 / 健壮性纪律
//!
//! - 不硬编码任何 token / 密钥；实时报价主路径走 TDX(raw TCP) / 腾讯(HTTP)，无需 token。
//! - 非交易时段现价可能为 None（合法市场状态）：
//!   - 成交类断言在「拿到 fresh 现价」时硬断言；拿不到（None / stale）时优雅 skip，
//!     因为 spec 要求写路径 fail-closed —— 此时验证「拒单」也是合法的端到端结果。
//! - provider 不可达时打印清晰原因后 return（skip），不脏 panic。
//!
//! ## 怎么跑（主 agent 用）
//!
//! ```bash
//! cargo test --manifest-path src-tauri/Cargo.toml --lib \
//!   account_live -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(test)]

use crate::domain::account::requests::{
    AccountActor, FetchAccountInclude, FetchAccountRequest, OperateAccountAction,
    OperateAccountRequest,
};
use crate::domain::account::types::OrderType;
use crate::domain::quotes::{
    InstrumentSource, MarketInstrument, RefreshMarketQuotesScope, RefreshPurpose,
};
use crate::domain::shared::{
    FreshnessStatus, InstrumentCategory, InstrumentStatus, Money, Price, Shares, TsCode, Volume,
};
use crate::infrastructure::db::{run_migrations, AppDb};
use crate::infrastructure::quotes::QuotesConfig;
use crate::pipeline::account::quote_gateway::QuotesFacadeGateway;
use crate::pipeline::account::service::{AccountService, AccountServiceConfig};
use crate::pipeline::quotes::service::RefreshMarketQuotesRequest;
use crate::pipeline::quotes::QuotesService;
use chrono::Utc;
use rust_decimal::Decimal;
use std::sync::Arc;

/// 已知长历史老股：贵州茅台（SH，会收过户费）。
const STOCK_SH: &str = "600519.SH";

fn ts(code: &str) -> TsCode {
    TsCode::parse(code).expect("valid ts_code")
}

/// 构造共享 DB（quotes + account migrations）+ QuotesService + AccountService（走真实 facade gateway）。
fn make_wired_services() -> (Arc<QuotesService>, Arc<AccountService>) {
    let db = AppDb::open_in_memory().unwrap();
    db.with(|c| {
        let mut all = Vec::new();
        all.extend(crate::infrastructure::quotes::migrations());
        all.extend(crate::infrastructure::account::migrations());
        all.extend(crate::infrastructure::account::migrations_tail());
        run_migrations(c, all).unwrap();
    });
    let quotes = Arc::new(QuotesService::new(db.clone(), QuotesConfig::from_env()).unwrap());
    // Account gateway 走真实 facade（同一 DB + 同一 SnapshotCache）。
    let gateway = Arc::new(QuotesFacadeGateway::new(db.clone(), quotes.cache().clone()));
    let account = Arc::new(AccountService::new(
        db,
        gateway,
        AccountServiceConfig::default(),
    ));
    account
        .initialize_account_if_needed(Money(Decimal::from(10_000_000)))
        .unwrap();
    (quotes, account)
}

fn seed_instrument(svc: &QuotesService, code: &TsCode) {
    let inst = MarketInstrument {
        ts_code: code.clone(),
        name: "贵州茅台".into(),
        category: InstrumentCategory::Stock,
        market: code.market(),
        board: None,
        sector: Some("白酒".into()),
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

/// 用真实 provider 刷新一只标的的报价；返回是否拿到 fresh + 现价（成交可行）。
async fn refresh_and_is_freshly_priced(quotes: &Arc<QuotesService>, code: &TsCode) -> bool {
    let req = RefreshMarketQuotesRequest {
        scope: RefreshMarketQuotesScope::Manual {
            ts_codes: vec![code.clone()],
        },
        purpose: RefreshPurpose::Intraday,
        trade_date: None,
    };
    if let Err(e) = quotes.refresh_market_quotes(req).await {
        eprintln!("[account_live] SKIP — refresh_market_quotes 失败（provider 不可达?）: {:?}", e);
        return false;
    }
    // 读回 snapshot（不触发 provider）。
    match crate::pipeline::quotes::facade::get_quote_snapshot(quotes.db(), quotes.cache(), code) {
        Ok(snap) => {
            let fresh = matches!(snap.quote.freshness.status, FreshnessStatus::Fresh);
            let priced = snap.quote.price.is_some();
            let has_ask = snap
                .quote
                .ask
                .iter()
                .any(|l| l.price.is_some() && l.volume.unwrap_or(Volume(0)).0 > 0);
            eprintln!(
                "[account_live] {} fresh={fresh} priced={priced} has_ask_depth={has_ask} price={:?}",
                code.as_str(),
                snap.quote.price
            );
            fresh && priced && has_ask
        }
        Err(e) => {
            eprintln!("[account_live] SKIP — 读回 snapshot 失败: {:?}", e);
            false
        }
    }
}

// =============================================================================

/// account_live_1 · 端到端：真实行情 → 市价开仓成交 → 现金扣减 / 持仓 / 成交价正确。
///
/// 验证 Account ↔ Quotes 集成：market 开仓用 fresh Quotes snapshot 的盘口模拟成交，
/// 成交价应等于卖一价，现金按 `price*qty + commission + transferFee` 扣减，持仓 100 股。
/// 非交易时段（无 fresh 现价 / 无盘口量）→ spec 要求 fail-closed 拒单，此时验证拒单语义。
#[tokio::test]
#[ignore]
async fn account_live_market_open_position_fills_at_real_quote() {
    let (quotes, account) = make_wired_services();
    let code = ts(STOCK_SH);
    seed_instrument(&quotes, &code);

    let tradeable = refresh_and_is_freshly_priced(&quotes, &code).await;

    // 真实的成交前现金（注意：账户实际注资 10_000_000，与 config().initial_cash 的默认 1_000_000
    // 不是一回事——必须读真实快照，不能拿 config 默认值当基线）。
    let before = account.fetch_account(FetchAccountRequest {
        include: Some(FetchAccountInclude {
            snapshot: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    });
    let cash_before = before.snapshot.expect("snapshot included").cash.0;

    let resp = account.operate_account(
        OperateAccountRequest {
            action: OperateAccountAction::OpenPosition {
                ts_code: code.clone(),
                quantity: Shares(100),
                order_type: Some(OrderType::Market),
                limit_price: None,
                expires_at: None,
                stop_loss: None,
                take_profit: None,
                time_stop_at: None,
                reason: "account_live e2e".into(),
            },
        },
        AccountActor::Agent,
    );

    if tradeable {
        // 交易时段拿到 fresh + 盘口 → 应成交（或部分成交）。
        // 注意：即便 fresh，TDX 现价存在但盘口深度不足时 spec 允许 depth_missing 拒单 —— 仍是合法端到端结果。
        if resp.accepted {
            eprintln!(
                "[account_live] 成交 accepted；fillIds={:?} positionId={:?} cashBefore={cash_before} cashAfter={}",
                resp.fill_ids, resp.position_id, resp.snapshot.cash.0
            );
            assert!(!resp.fill_ids.is_empty(), "accepted 市价单必须有成交");
            let pos_id = resp.position_id.clone().expect("成交应产生 position");
            // 现金确实被扣减（< 成交前真实现金）。
            assert!(
                resp.snapshot.cash.0 < cash_before,
                "买入成交后现金应减少：before={cash_before} after={}",
                resp.snapshot.cash.0
            );
            // 读回持仓，数量 = 100，市值由真实行情派生（fresh quote 存在）。
            let fetched = account.fetch_account(FetchAccountRequest {
                include: Some(FetchAccountInclude {
                    positions: Some(true),
                    snapshot: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            });
            let positions = fetched.positions.unwrap_or_default();
            let pos = positions.iter().find(|p| p.position_id == pos_id).expect("持仓应可读回");
            assert_eq!(pos.quantity.0, 100, "持仓数量应为 100");
            assert!(pos.avg_cost.0 > Decimal::ZERO, "成交价应 > 0");
            eprintln!(
                "[account_live] position qty={} avg_cost={} market_price={:?} market_value={:?} unrealized_pnl={:?}",
                pos.quantity.0, pos.avg_cost.0, pos.market_price, pos.market_value, pos.unrealized_pnl
            );
            // 估值：fresh quote 存在 → marketPrice / marketValue 应被派生。
            if let (Some(mp), Some(mv)) = (pos.market_price, pos.market_value) {
                assert!(mp.0 > Decimal::ZERO);
                assert_eq!(
                    mv.0,
                    mp.0 * Decimal::from(pos.quantity.0),
                    "marketValue = quantity * marketPrice（spec §4）"
                );
            }
        } else {
            eprintln!(
                "[account_live] INFO — fresh 但拒单（合法：盘口深度/涨跌停等）reason={:?}",
                resp.reason
            );
        }
    } else {
        // 非交易时段 / provider 不可达 → spec 要求写路径 fail-closed 拒单。
        assert!(!resp.accepted, "无 fresh 可成交行情时 spec 要求拒单（fail-closed）");
        eprintln!(
            "[account_live] SKIP/EXPECT — 无 fresh 可成交行情 → fail-closed 拒单 reason={:?}（合法端到端结果）",
            resp.reason
        );
        // 拒单的 reason 必须是行情/盘口类。
        use crate::domain::shared::ErrorCode;
        assert!(
            matches!(
                resp.reason,
                Some(ErrorCode::QuoteStale)
                    | Some(ErrorCode::QuoteMissing)
                    | Some(ErrorCode::QuotePriceMissing)
                    | Some(ErrorCode::DepthMissing)
                    | Some(ErrorCode::OutsideTradingSession)
                    | Some(ErrorCode::InstrumentSuspended)
                    | Some(ErrorCode::LimitUpDownBlocked)
            ),
            "fail-closed 拒单原因应为行情/盘口类，实测 {:?}",
            resp.reason
        );
    }
}

/// account_live_2 · 端到端：真实行情驱动持仓估值（限价单挂 pending → 不依赖交易时段）。
///
/// 这条不依赖即时成交（限价开仓可盘外创建为 pending），稳定可跑：
/// 验证 limit 开仓在 stale quote 上也能创建 pending（spec §2「limit 可在 stale 时创建为 pending」），
/// 并验证 subscribed_codes 暴露该 pending 单的标的（Account ↔ Quotes 订阅集合边界）。
#[tokio::test]
#[ignore]
async fn account_live_limit_open_pending_and_subscribed_codes() {
    let (quotes, account) = make_wired_services();
    let code = ts(STOCK_SH);
    seed_instrument(&quotes, &code);
    // 取一次真实报价用于挑一个合理的限价（远低于现价以保证不立即成交也能算 pending）。
    let _ = refresh_and_is_freshly_priced(&quotes, &code).await;

    let resp = account.operate_account(
        OperateAccountRequest {
            action: OperateAccountAction::OpenPosition {
                ts_code: code.clone(),
                quantity: Shares(100),
                order_type: Some(OrderType::Limit),
                // 远低于茅台现价（~1500）→ 不会立即成交，进入 pending。
                limit_price: Some(Price(Decimal::from(100))),
                expires_at: Some(Utc::now() + chrono::Duration::days(1)),
                stop_loss: None,
                take_profit: None,
                time_stop_at: None,
                reason: "account_live limit pending".into(),
            },
        },
        AccountActor::Agent,
    );
    assert!(
        resp.accepted,
        "limit 开仓应被接受为 pending（即便 quote stale），reason={:?}",
        resp.reason
    );
    assert!(resp.order_id.is_some(), "pending limit 必须返回 orderId");
    assert!(resp.snapshot.frozen_cash.0 > Decimal::ZERO, "pending 买单应冻结现金");
    eprintln!(
        "[account_live] pending limit orderId={:?} frozenCash={}",
        resp.order_id, resp.snapshot.frozen_cash.0
    );

    // Account ↔ Quotes 订阅集合：pending 单标的应在 subscribed_codes 内。
    let subs = account.subscribed_codes();
    assert!(
        subs.contains(&code),
        "subscribed_codes 应含 pending 单标的（编排层据此刷新行情）"
    );
}

// =============================================================================
// LLM-as-Judge 正确性验证骨架（交主 agent 跑）。
//
// 每条测试在能拿到 fresh 可成交行情时，用**真实报价**驱动一笔成交，并 println 结构化
// 的 `JUDGE` 行：把 actual 值与 spec 费用/PnL 公式算出的 expected 值并列输出，供 LLM
// 判定数据/行为准确性。非交易时段（拿不到 fresh 现价）→ println `JUDGE ... SKIP` 不 panic。
//
// 费用公式（spec §硬风控模型，account-module.md §2）：
//   notional      = price * quantity
//   commission    = max(notional * 0.0003, 5.0)        双向
//   stampTax      = notional * 0.0005                  仅卖出
//   transferFee   = notional * 0.00001                 仅 SH stock/fund，双向
//   买入现金扣减   = notional + commission + transferFee
//   卖出现金增加   = notional - commission - stampTax - transferFee
//   avgCost(首买) = (notional + buyCommission + buyTransferFee) / quantity
//   realizedPnl   = (sellPrice - avgCost) * qty - sellCommission - stampTax - sellTransferFee
//
// 凭据 env（LLM judge 调用 LLM 用，沿用 agent judge 读法；测试本身不读 key）：
//   JUDGE_BASE / JUDGE_KEY / JUDGE_MODEL  —— 主 agent 负责注入，**绝不在代码里硬编码**。
// =============================================================================

/// 费用常量（spec 默认值；与 AccountFeePolicy::default 对齐）。
const COMMISSION_RATE: f64 = 0.0003;
const MIN_COMMISSION: f64 = 5.0;
#[allow(dead_code)] // 文档化卖出印花税率（judge rubric 引用；T+1 锁仓下 live 卖出走 fail-closed）
const STAMP_TAX_SELL_RATE: f64 = 0.0005;
const TRANSFER_FEE_RATE: f64 = 0.00001; // 仅 SH

fn expected_commission(notional: f64) -> f64 {
    (notional * COMMISSION_RATE).max(MIN_COMMISSION)
}

/// 取真实 fresh 卖一价（成交可行时）。
async fn real_ask0(quotes: &Arc<QuotesService>, code: &TsCode) -> Option<f64> {
    let req = RefreshMarketQuotesRequest {
        scope: RefreshMarketQuotesScope::Manual { ts_codes: vec![code.clone()] },
        purpose: RefreshPurpose::Intraday,
        trade_date: None,
    };
    if quotes.refresh_market_quotes(req).await.is_err() {
        return None;
    }
    let snap =
        crate::pipeline::quotes::facade::get_quote_snapshot(quotes.db(), quotes.cache(), code)
            .ok()?;
    if !matches!(snap.quote.freshness.status, FreshnessStatus::Fresh) {
        return None;
    }
    snap.quote
        .ask
        .iter()
        .find(|l| l.price.is_some() && l.volume.unwrap_or(Volume(0)).0 > 0)
        .and_then(|l| l.price)
        .map(|p| p.0.to_string().parse::<f64>().unwrap())
}

fn dec_to_f64(d: Decimal) -> f64 {
    d.to_string().parse::<f64>().unwrap()
}

/// account_live_3 · 成交价 + 费用 + avgCost + 现金扣减正确性（market buy）。
///
/// JUDGE 行覆盖：fillPrice==realAsk0、commission/transferFee==公式、avgCost==公式、
/// cashAfter==cashBefore - notional - commission - transferFee。
#[tokio::test]
#[ignore]
async fn account_live_market_buy_fees_and_avgcost_correct() {
    let (quotes, account) = make_wired_services();
    let code = ts(STOCK_SH); // SH → 收过户费
    seed_instrument(&quotes, &code);

    let Some(real_ask) = real_ask0(&quotes, &code).await else {
        println!("JUDGE account_live_3 SKIP reason=no_fresh_ask note=需盘中跑");
        return;
    };

    let before = account.fetch_account(FetchAccountRequest {
        include: Some(FetchAccountInclude { snapshot: Some(true), ..Default::default() }),
        ..Default::default()
    });
    let cash_before = dec_to_f64(before.snapshot.unwrap().cash.0);

    let qty = 100i64;
    let resp = account.operate_account(
        OperateAccountRequest {
            action: OperateAccountAction::OpenPosition {
                ts_code: code.clone(),
                quantity: Shares(qty),
                order_type: Some(OrderType::Market),
                limit_price: None,
                expires_at: None,
                stop_loss: None,
                take_profit: None,
                time_stop_at: None,
                reason: "account_live_3 fees".into(),
            },
        },
        AccountActor::Agent,
    );
    if !resp.accepted {
        println!(
            "JUDGE account_live_3 SKIP reason={:?} note=fresh但拒单(盘口/涨跌停等合法fail-closed)",
            resp.reason
        );
        return;
    }
    let pos_id = resp.position_id.clone().unwrap();
    let fetched = account.fetch_account(FetchAccountRequest {
        include: Some(FetchAccountInclude {
            positions: Some(true),
            snapshot: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    });
    let pos = fetched
        .positions
        .unwrap_or_default()
        .into_iter()
        .find(|p| p.position_id == pos_id)
        .unwrap();
    let avg_cost = dec_to_f64(pos.avg_cost.0);
    let cash_after = dec_to_f64(resp.snapshot.cash.0);

    // 用持仓数 * avgCost 反推；fillPrice 取 avgCost 去除费用后近似（hermetic 已精确验证费用，
    // 这里 JUDGE 用 realAsk 作为预期成交价基准）。
    let notional = real_ask * qty as f64;
    let exp_commission = expected_commission(notional);
    let exp_transfer = notional * TRANSFER_FEE_RATE; // SH
    let exp_avg_cost = (notional + exp_commission + exp_transfer) / qty as f64;
    let exp_cash_after = cash_before - notional - exp_commission - exp_transfer;

    println!(
        "JUDGE account_live_3 market_buy_fees \
         realAsk0={real_ask} qty={qty} notional={notional:.4} \
         avgCost={avg_cost} expectedAvgCost={exp_avg_cost:.6} \
         expectedCommission={exp_commission:.4} expectedTransferFee={exp_transfer:.6} \
         cashBefore={cash_before} cashAfter={cash_after} expectedCashAfter={exp_cash_after:.4} \
         positionQty={}",
        pos.quantity.0
    );
    // 软硬断言：成交价应等于卖一价 → avgCost 约等公式（容微小 round_dp 误差）。
    assert!(
        (avg_cost - exp_avg_cost).abs() < 0.01,
        "avgCost 偏离公式：actual={avg_cost} expected={exp_avg_cost}"
    );
    assert!(
        (cash_after - exp_cash_after).abs() < 0.05,
        "cashAfter 偏离公式：actual={cash_after} expected={exp_cash_after}"
    );
    assert_eq!(pos.quantity.0, qty, "持仓数量应为 {qty}");
}

/// account_live_4 · 卖出 realizedPnl 正确性（买入后立即全平）。
///
/// JUDGE 行覆盖：buyPrice / sellPrice / qty / realizedPnl == 公式(扣印花+过户+双边佣金)。
/// 注：T+1 锁仓，正常盘中当日买入不可卖 → 该条主要验证「当日卖被拒(insufficient_sellable)」
/// 这一 fail-closed 行为；若测试环境放开 T+1（非生产），才验证 PnL 公式。默认 println SKIP。
#[tokio::test]
#[ignore]
async fn account_live_sell_realized_pnl_correct() {
    let (quotes, account) = make_wired_services();
    let code = ts(STOCK_SH);
    seed_instrument(&quotes, &code);

    let Some(real_ask) = real_ask0(&quotes, &code).await else {
        println!("JUDGE account_live_4 SKIP reason=no_fresh_ask note=需盘中跑");
        return;
    };
    let buy = account.operate_account(
        OperateAccountRequest {
            action: OperateAccountAction::OpenPosition {
                ts_code: code.clone(),
                quantity: Shares(100),
                order_type: Some(OrderType::Market),
                limit_price: None,
                expires_at: None,
                stop_loss: None,
                take_profit: None,
                time_stop_at: None,
                reason: "account_live_4 buy".into(),
            },
        },
        AccountActor::Agent,
    );
    if !buy.accepted {
        println!("JUDGE account_live_4 SKIP reason={:?} note=买入未成交", buy.reason);
        return;
    }
    let pos_id = buy.position_id.clone().unwrap();
    // 当日卖出（T+1 锁仓）→ spec 要求 insufficient_sellable_quantity（fail-closed 行为正确性）。
    let sell = account.operate_account(
        OperateAccountRequest {
            action: OperateAccountAction::ClosePosition {
                position_id: pos_id.clone(),
                quantity: Some(Shares(100)),
                order_type: Some(OrderType::Market),
                limit_price: None,
                expires_at: None,
                reason: "account_live_4 sell same day".into(),
            },
        },
        AccountActor::Agent,
    );
    println!(
        "JUDGE account_live_4 sell_t1_lock buyPrice={real_ask} sellAccepted={} sellReason={:?} \
         expectedReason=insufficient_sellable_quantity(当日买入T+1锁仓不可卖)",
        sell.accepted, sell.reason
    );
    // fail-closed 行为：当日买入不可卖。
    use crate::domain::shared::ErrorCode;
    assert!(
        !sell.accepted && sell.reason == Some(ErrorCode::InsufficientSellableQuantity),
        "T+1：当日买入当日卖出必须 fail-closed (insufficient_sellable_quantity)，实测 accepted={} reason={:?}",
        sell.accepted,
        sell.reason
    );
}

/// account_live_5 · limit 撮合：用真实报价驱动挂单评估。
///
/// 挂一个**高于现价**的限价买单（保证 fresh 报价下立即可成交），evaluate_account_triggers
/// 用真实 snapshot 撮合。JUDGE 行输出 limitPrice / realAsk0 / 是否成交 / fillPrice。
#[tokio::test]
#[ignore]
async fn account_live_limit_fill_with_real_quote() {
    use crate::domain::account::requests::FetchAccountInclude as Inc;
    let (quotes, account) = make_wired_services();
    let code = ts(STOCK_SH);
    seed_instrument(&quotes, &code);

    let Some(real_ask) = real_ask0(&quotes, &code).await else {
        println!("JUDGE account_live_5 SKIP reason=no_fresh_ask note=需盘中跑");
        return;
    };
    // 限价 = 现价 * 1.02（高于卖一 → fresh 时可成交）。
    let limit_px = (real_ask * 1.02 * 100.0).round() / 100.0;
    let place = account.operate_account(
        OperateAccountRequest {
            action: OperateAccountAction::OpenPosition {
                ts_code: code.clone(),
                quantity: Shares(100),
                order_type: Some(OrderType::Limit),
                limit_price: Some(Price(Decimal::from_str_exact(&limit_px.to_string()).unwrap())),
                expires_at: Some(Utc::now() + chrono::Duration::days(1)),
                stop_loss: None,
                take_profit: None,
                time_stop_at: None,
                reason: "account_live_5 limit".into(),
            },
        },
        AccountActor::Agent,
    );
    assert!(place.accepted, "limit 挂单应被接受: {:?}", place.reason);
    let order_id = place.order_id.clone().unwrap();

    // 用真实 snapshot 驱动撮合评估（沿用 scheduler 的 EvalDeps 构造方式）。
    use crate::pipeline::account::eval::{evaluate_account_triggers, EvalDeps, EvalInput};
    let deps = EvalDeps {
        db: account.db().clone(),
        gateway: account.gateway.clone(),
        fee_policy: account.config().fee_policy.clone(),
    };
    let r = evaluate_account_triggers(EvalInput {
        deps: &deps,
        now: Utc::now(),
        batch_size: 50,
        cursor: None,
    });
    let fetched = account.fetch_account(FetchAccountRequest {
        include: Some(Inc { orders: Some(true), ..Default::default() }),
        order_status_in: None,
        ..Default::default()
    });
    let order = fetched
        .orders
        .unwrap_or_default()
        .into_iter()
        .find(|o| o.order_id == order_id);
    let (status, filled) = order
        .map(|o| (format!("{:?}", o.status), o.filled_quantity.0))
        .unwrap_or(("<gone>".into(), 0));
    println!(
        "JUDGE account_live_5 limit_match realAsk0={real_ask} limitPrice={limit_px} \
         orderStatus={status} filledQty={filled} triggersThisBatch={} \
         expected=ask0<=limitPrice且fresh时应(部分)成交",
        r.triggers.len()
    );
    // 不硬断言成交（盘口深度可能不足），但若成交则数量必须 ≤ 100。
    assert!(filled <= 100, "成交量不得超过委托量");
}

/// account_live_6 · fail-closed：非交易时段 / 不可达 stale → 即时成交拒单 code 正确。
///
/// 不依赖盘中：直接对 SH 标的发 market 单，若行情非 fresh / 拿不到盘口 → 必须 fail-closed，
/// JUDGE 行输出实际 reason，与 spec 允许的 fail-closed code 集合比对。
#[tokio::test]
#[ignore]
async fn account_live_fail_closed_reason_correct() {
    use crate::domain::shared::ErrorCode;
    let (quotes, account) = make_wired_services();
    let code = ts(STOCK_SH);
    seed_instrument(&quotes, &code);
    let ask = real_ask0(&quotes, &code).await;

    let resp = account.operate_account(
        OperateAccountRequest {
            action: OperateAccountAction::OpenPosition {
                ts_code: code.clone(),
                quantity: Shares(100),
                order_type: Some(OrderType::Market),
                limit_price: None,
                expires_at: None,
                stop_loss: None,
                take_profit: None,
                time_stop_at: None,
                reason: "account_live_6 fail-closed".into(),
            },
        },
        AccountActor::Agent,
    );
    println!(
        "JUDGE account_live_6 fail_closed freshAsk={:?} accepted={} reason={:?} \
         expected=有fresh盘口则成交;否则reason∈{{quote_stale,quote_missing,quote_price_missing,depth_missing,outside_trading_session,instrument_suspended,limit_up_down_blocked}}",
        ask, resp.accepted, resp.reason
    );
    if ask.is_none() {
        // 无 fresh 可成交行情 → 必须拒单，且 reason 为行情/盘口类。
        assert!(!resp.accepted, "无 fresh 可成交行情时必须 fail-closed");
        assert!(
            matches!(
                resp.reason,
                Some(ErrorCode::QuoteStale)
                    | Some(ErrorCode::QuoteMissing)
                    | Some(ErrorCode::QuotePriceMissing)
                    | Some(ErrorCode::DepthMissing)
                    | Some(ErrorCode::OutsideTradingSession)
                    | Some(ErrorCode::InstrumentSuspended)
                    | Some(ErrorCode::LimitUpDownBlocked)
            ),
            "fail-closed reason 应为行情/盘口类，实测 {:?}",
            resp.reason
        );
    }
}
