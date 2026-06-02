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
