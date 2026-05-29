//! gangzi-terminal lib entry.
//!
//! Spec: docs/design/architecture.md §2 (分层约束)
//!
//! 后端按 4 层组织，依赖方向单向：adapters → pipeline → infrastructure → domain。

pub mod adapters;
pub mod domain;
pub mod infrastructure;
pub mod pipeline;

use std::sync::Arc;
use std::time::Duration;

use tauri::{Emitter, Manager};
use tauri_specta::{collect_commands, Builder};

use crate::adapters::account::events::{
    wrap_account_triggered, wrap_account_updated, ACCOUNT_TRIGGERED_EVENT, ACCOUNT_UPDATED_EVENT,
};
use crate::adapters::news::events::{wrap_news_refreshed, NEWS_REFRESHED_EVENT};
use crate::adapters::quotes::events::{
    wrap_market_quotes_refresh_progress, wrap_market_quotes_refreshed,
    MARKET_QUOTES_REFRESHED_EVENT, MARKET_QUOTES_REFRESH_PROGRESS_EVENT,
};
use crate::domain::shared::Money;
use crate::infrastructure::account::migrations as account_migrations;
use crate::infrastructure::agent::{
    bootstrap as bootstrap_agent_infra, migrations as agent_migrations,
};
use crate::infrastructure::db::{run_migrations, AppDb};
use crate::infrastructure::news::{migrations as news_migrations, NewsRepository, SourceRegistry};
use crate::infrastructure::quotes::{migrations as quotes_migrations, QuotesConfig};
use crate::pipeline::account::scheduler::{
    spawn_account_eval_scheduler, AccountSchedulerHandle, ACCOUNT_EVAL_BATCH_SIZE,
    ACCOUNT_EVAL_INTERVAL_SECS,
};
use crate::pipeline::account::{
    AccountQuoteGateway, AccountService, AccountServiceConfig, QuotesFacadeGateway,
};
use crate::pipeline::news::scheduler::{
    spawn_news_refresh_scheduler, NewsSchedulerHandle, NEWS_REFRESH_INTERVAL_SECS,
};
use crate::pipeline::news::NewsService;
use crate::pipeline::quotes::scheduler::{
    spawn_full_scheduler, QuotesSchedulerHandle, QuotesSchedulerIntervals,
    QUOTES_REFRESH_INTERVAL_SECS, QUOTES_SUBSCRIBED_INTERVAL_SECS,
};
use crate::pipeline::quotes::service::QuotesService;
use rust_decimal::Decimal;

/// Tauri 主入口。`main.rs` 调用 `gangzi_terminal::run()` 启动 app。
pub fn run() {
    infrastructure::tracing::init();

    let specta_builder = Builder::<tauri::Wry>::new().commands(collect_commands![
        adapters::ping::ping,
        adapters::news::cmd::fetch_news,
        adapters::news::cmd::list_news_sources,
        adapters::news::cmd::warm_articles,
        adapters::quotes::cmd::list_market,
        adapters::quotes::cmd::fetch_data,
        adapters::quotes::cmd::scan_market,
        adapters::quotes::cmd::fetch_market_breadth,
        adapters::quotes::cmd::fetch_industry_heatmap,
        adapters::quotes::cmd::ensure_chart_data,
        adapters::quotes::cmd::extend_chart_history,
        adapters::quotes::cmd::fetch_kline_page,
        adapters::quotes::cmd::set_quote_hotset,
        adapters::quotes::cmd::forward_log,
        adapters::agent::cmd::agent_list_skills,
        adapters::account::cmd::fetch_account,
        // Spec: account-module.md §4 — operate_account 写入口只对 Agent tool /
        // 外部自动化决策运行时暴露，不能注册为 Tauri command 供前端直接 invoke。
        // 函数实现保留为 #[allow(dead_code)]，将由 Phase 3 Agent Runtime
        // 通过 SkillRegistry 注册为 skill。
        adapters::account::cmd::update_watchlist,
        adapters::account::cmd::mark_trigger_handled,
        adapters::account::cmd::rebuild_account_snapshot,
    ]);

    #[cfg(debug_assertions)]
    specta_builder
        .export(
            specta_typescript::Typescript::default()
                // i64 在我们这里只用于 ageMs / Shares / Volume / TimestampMs 等，
                // 实际值远小于 Number.MAX_SAFE_INTEGER (2^53-1)。允许 TS Number 输出。
                // Money / Price / Amount 走 rust_decimal serialize-as-string，不受此影响。
                .bigint(specta_typescript::BigIntExportBehavior::Number),
            "../src/bindings.ts",
        )
        .expect("failed to export specta typescript bindings");

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(specta_builder.invoke_handler())
        .setup(move |app| {
            specta_builder.mount_events(app);

            // -- DB / migrations
            let db_path = resolve_db_path(app.handle())?;
            let db = AppDb::open(&db_path).expect("failed to open AppDb");
            db.with(|conn| {
                // rusqlite_migration 用 user_version 跟踪"已 apply 第 N 个 migration"，
                // **按全局位置 (0-indexed) 判定**，不感知 BC 拆分。所以这里的拼接顺序
                // 必须是 append-only：新增 BC migration 时只能加到当前列表末尾，
                // 否则会把后面 BC 的旧 migration 错位变成"需要重跑"，CREATE TABLE 直接 panic。
                //
                // 当前固定顺序（不要重排）：news → agent → account → quotes
                // quotes 排最后是因为 quotes 是目前唯一有 M002 的 BC（D1 引入 xdxr 表）；
                // 其他 BC 加 M002 时，自行把那个 BC 移到当前末尾。
                let mut all = Vec::new();
                all.extend(news_migrations());
                all.extend(agent_migrations());
                all.extend(account_migrations());
                all.extend(quotes_migrations());
                run_migrations(conn, all).expect("failed to apply migrations");
            });

            // -- News BC bootstrap
            let registry = Arc::new(SourceRegistry::new());
            {
                let repo = NewsRepository::new(&db);
                if let Err(e) = registry.bootstrap(&repo) {
                    tracing::error!(target: "news.bootstrap", error = %e, "failed to bootstrap news source registry");
                }
            }

            let news_service = Arc::new(
                NewsService::new(db.clone(), Arc::clone(&registry))
                    .expect("failed to build NewsService (reqwest client)"),
            );
            app.manage(db.clone());
            app.manage(Arc::clone(&news_service));

            // -- News Event sink
            let app_handle = app.handle().clone();
            let sink: crate::pipeline::news::scheduler::EventSink =
                Arc::new(move |payload| {
                    let envelope = wrap_news_refreshed(payload, None);
                    if let Err(e) = app_handle.emit(NEWS_REFRESHED_EVENT, envelope) {
                        tracing::warn!(target: "news.refresh.emit", error = %e, "failed to emit news-refreshed");
                    }
                });
            news_service.set_event_sink(Arc::clone(&sink));

            // Tauri setup 闭包跑在 main thread，没有 current tokio runtime context；
            // scheduler 内部 `tokio::spawn` 需要 runtime handle。用 tauri::async_runtime::block_on
            // 进入 runtime context 让内部 spawn 工作。
            let news_handle: NewsSchedulerHandle =
                tauri::async_runtime::block_on(async {
                    spawn_news_refresh_scheduler(
                        Arc::clone(&news_service),
                        Duration::from_secs(NEWS_REFRESH_INTERVAL_SECS),
                        sink,
                    )
                });
            app.manage(news_handle);

            // -- Quotes BC bootstrap
            let quotes_service = Arc::new(
                QuotesService::new(db.clone(), QuotesConfig::default())
                    .expect("failed to build QuotesService"),
            );
            app.manage(Arc::clone(&quotes_service));

            // -- Quotes Event sink
            let app_handle = app.handle().clone();
            let quotes_sink: crate::pipeline::quotes::service::RefreshEventSink =
                Arc::new(move |payload| {
                    let envelope = wrap_market_quotes_refreshed(payload, None);
                    if let Err(e) = app_handle.emit(MARKET_QUOTES_REFRESHED_EVENT, envelope) {
                        tracing::warn!(target: "quotes.refresh.emit", error = %e, "failed to emit market-quotes-refreshed");
                    }
                });
            quotes_service.set_event_sink(quotes_sink);

            // -- Quotes Refresh Progress sink（spec §5 全市场刷新执行契约）
            let app_handle = app.handle().clone();
            let quotes_progress_sink: crate::pipeline::quotes::service::RefreshProgressSink =
                Arc::new(move |payload| {
                    let envelope = wrap_market_quotes_refresh_progress(payload, None);
                    if let Err(e) =
                        app_handle.emit(MARKET_QUOTES_REFRESH_PROGRESS_EVENT, envelope)
                    {
                        tracing::warn!(target: "quotes.refresh.emit", error = %e, "failed to emit market-quotes-refresh-progress");
                    }
                });
            quotes_service.set_progress_sink(quotes_progress_sink);

            // -- Quotes cold-start seed（spec §5 universe — seed drift, see seed.rs）：
            // 在异步 TDX universe 刷新启动之前，先把 ~80 条内置热门标的 upsert 进 DB，
            // 让 UI 第一帧（启动到首次 refresh 完成的 ~10s 窗口）就能看到非空市场列表。
            // 同步 + 幂等；预期 <100ms。失败不阻断启动，仅日志。
            {
                let repo = crate::infrastructure::quotes::QuotesRepository::new(&db);
                match crate::infrastructure::quotes::seed_builtin_instruments(&repo) {
                    Ok(n) => tracing::info!(target: "quotes.seed", count = n, "builtin universe seed upserted"),
                    Err(e) => tracing::warn!(target: "quotes.seed", error = %e, "builtin universe seed failed"),
                }
            }

            // -- TuShare 健康探针（spec §2 "TuShare 健康状态"）：启动 ping。
            // 在 universe enrich 之前完成，让 refresh_market_instruments 看到正确的 is_available。
            // 用 block_on + 内置 timeout（health 自己的 fetch_trade_cal timeout = 10s；
            // 这里再裹一层 5s 上限避免启动阻塞）。
            {
                let hc = Arc::clone(quotes_service.health());
                let _ = tauri::async_runtime::block_on(async move {
                    let timeout = std::time::Duration::from_secs(5);
                    let _ = tokio::time::timeout(timeout, hc.initial_ping()).await;
                });
                tracing::info!(
                    target: "quotes.tushare.health",
                    state = ?quotes_service.health().state(),
                    "tushare initial health probe completed"
                );
            }

            // -- Quotes startup catch-up（spec §5）：异步后台 task；不阻塞 setup。
            // 用 tauri::async_runtime::spawn 而非 tokio::spawn — setup 闭包没有
            // current tokio runtime context；tauri::async_runtime 提供同等接口。
            {
                let svc = Arc::clone(&quotes_service);
                tauri::async_runtime::spawn(async move {
                    // 1) universe enrich（TuShare 可用时；health gate 自动 skip）
                    if let Err(e) = svc.refresh_market_instruments().await {
                        tracing::warn!(target: "quotes.startup", error = ?e, "refresh_market_instruments failed");
                    }
                    // 2) 交易日历 ±30 天
                    let today = chrono::Utc::now().date_naive();
                    let start = (today - chrono::Duration::days(30))
                        .format("%Y%m%d")
                        .to_string();
                    let end = (today + chrono::Duration::days(30))
                        .format("%Y%m%d")
                        .to_string();
                    if let Err(e) = svc.refresh_trade_calendar(&start, &end).await {
                        tracing::warn!(target: "quotes.startup", error = ?e, "refresh_trade_calendar failed");
                    }
                    // 3) 启动后立即跑一次 subscribed quote refresh（核心指数热数据）
                    let core = svc.core_indexes();
                    let req = crate::pipeline::quotes::service::RefreshMarketQuotesRequest {
                        scope: crate::domain::quotes::RefreshMarketQuotesScope::Subscribed {
                            ts_codes: core,
                        },
                        purpose: crate::domain::quotes::RefreshPurpose::Intraday,
                        trade_date: None,
                    };
                    if let Err(e) = svc.refresh_market_quotes(req).await {
                        tracing::warn!(target: "quotes.startup", error = ?e, "core quote refresh failed");
                    }

                    // 4) K 线日线预热（核心指数）— 小而快（~4 codes），先跑，UI 切到指数 K 线立即可见。
                    //    Universe 级 K 线刷新交给 scheduler 16:00 / 下次启动后台。
                    let core = svc.core_indexes();
                    let kline_scope = crate::domain::quotes::RefreshDataScope::Subscribed {
                        ts_codes: core.clone(),
                    };
                    if let Err(e) = svc.refresh_klines(kline_scope, vec![crate::domain::quotes::KlinePeriod::Day]).await {
                        tracing::warn!(target: "quotes.startup", error = ?e, "kline warmup failed");
                    }

                    // 5) xdxr 预热（核心指数 + 后续 watchlist）— 即使指数没事件也要写 refresh_state
                    //    让 read 路径从状态 A（qfq_missing）→ 状态 B（无 warning，干净）。
                    //    Spec: quotes-module.md §2 "本地复权计算" + §5 后台刷新 xdxr 行。
                    let xdxr_scope = crate::domain::quotes::RefreshDataScope::Subscribed { ts_codes: core };
                    if let Err(e) = svc.refresh_xdxr_events(xdxr_scope).await {
                        tracing::warn!(target: "quotes.startup", error = ?e, "xdxr warmup failed");
                    }

                    // 6) Close snapshot catch-up（universe，慢 — 单独 spawn 不阻塞后续）
                    //
                    // scheduler 在 h==15 && m>=30 触发一次 close snapshot；如果用户在 15:30
                    // 之后冷启动 app，会永久错过当天窗口。这里启动时检查 latest_completed
                    // _trade_date 的 close snapshot 是否完整；不完整就立即触发一次。
                    //
                    // Spec: quotes-module.md §5 "失败时可低频重试直到获得最新已完成交易日快照"。
                    let ctx = svc.market_time_now();
                    let last_td = ctx.latest_completed_trade_date;
                    if !svc.close_snapshot_complete(last_td).await {
                        tracing::info!(
                            target: "quotes.startup",
                            trade_date = %last_td.format(),
                            "close snapshot incomplete; triggering universe catch-up (background)"
                        );
                        let svc_close = Arc::clone(&svc);
                        tauri::async_runtime::spawn(async move {
                            let req = crate::pipeline::quotes::service::RefreshMarketQuotesRequest {
                                scope: crate::domain::quotes::RefreshMarketQuotesScope::Universe,
                                purpose: crate::domain::quotes::RefreshPurpose::Close,
                                trade_date: Some(last_td),
                            };
                            if let Err(e) = svc_close.refresh_market_quotes(req).await {
                                tracing::warn!(target: "quotes.startup", error = ?e, "close snapshot catch-up failed");
                            }
                        });
                    }
                });
            }

            // -- Agent Infra bootstrap（Phase 1）
            // Spec: docs/design/agent-infra-module.md §5（SkillRegistry / 持久化）。
            // Runtime（Phase 3）会通过 `AgentInfra.registry.register_skill(...)` 注入
            // Quotes / News / Account facade skill，并新增 run_agent / send_user_message command。
            let agent_infra = bootstrap_agent_infra(db.clone());
            app.manage(agent_infra);

            // -- Account BC bootstrap（Phase 2）
            // Spec: docs/design/account-module.md §3 数据流 + §5 调度期望
            // Account 通过 Quotes facade 读 snapshot（fail-closed on stale / missing for 即时成交）；
            // 行情 cache 通过 QuotesFacadeGateway 直接复用 quotes_service.cache()。
            let account_gateway: std::sync::Arc<dyn AccountQuoteGateway> =
                std::sync::Arc::new(QuotesFacadeGateway::new(
                    db.clone(),
                    std::sync::Arc::clone(quotes_service.cache()),
                ));
            let account_service = Arc::new(AccountService::new(
                db.clone(),
                account_gateway,
                AccountServiceConfig {
                    initial_cash: Money(Decimal::from(1_000_000)),
                    ..AccountServiceConfig::default()
                },
            ));
            // 初始化账户（幂等）
            if let Err(code) = account_service
                .initialize_account_if_needed(Money(Decimal::from(1_000_000)))
            {
                tracing::error!(
                    target: "account.bootstrap",
                    error = ?code,
                    "failed to initialize account"
                );
            }

            // Account event sinks
            let app_handle_a = app.handle().clone();
            let updated_sink: crate::pipeline::account::service::AccountUpdatedSink =
                Arc::new(move |inner| {
                    let env = wrap_account_updated(inner, None);
                    if let Err(e) = app_handle_a.emit(ACCOUNT_UPDATED_EVENT, env) {
                        tracing::warn!(target: "account.emit", error = %e, "failed to emit account-updated");
                    }
                });
            account_service.set_updated_sink(updated_sink);
            let app_handle_b = app.handle().clone();
            let triggered_sink: crate::pipeline::account::service::AccountTriggeredSink =
                Arc::new(move |inner| {
                    let env = wrap_account_triggered(inner, None);
                    if let Err(e) = app_handle_b.emit(ACCOUNT_TRIGGERED_EVENT, env) {
                        tracing::warn!(target: "account.emit", error = %e, "failed to emit account-triggered");
                    }
                });
            account_service.set_triggered_sink(triggered_sink);
            app.manage(Arc::clone(&account_service));

            // Account eval scheduler — 同样需要 runtime context 包装
            let account_handle: AccountSchedulerHandle =
                tauri::async_runtime::block_on(async {
                    spawn_account_eval_scheduler(
                        Arc::clone(&account_service),
                        std::time::Duration::from_secs(ACCOUNT_EVAL_INTERVAL_SECS),
                        ACCOUNT_EVAL_BATCH_SIZE,
                    )
                });
            app.manage(account_handle);

            // -- Quotes Scheduler（multi-tick）— 同样需要 runtime context 包装
            let quotes_handle: QuotesSchedulerHandle =
                tauri::async_runtime::block_on(async {
                    spawn_full_scheduler(
                        Arc::clone(&quotes_service),
                        QuotesSchedulerIntervals {
                            universe_interval: Duration::from_secs(QUOTES_REFRESH_INTERVAL_SECS),
                            subscribed_interval: Duration::from_secs(
                                QUOTES_SUBSCRIBED_INTERVAL_SECS,
                            ),
                            daily_tick_interval: Duration::from_secs(60),
                            hot_interval: Duration::from_secs(
                                crate::pipeline::quotes::scheduler::QUOTES_HOT_INTERVAL_SECS,
                            ),
                        },
                    )
                });
            app.manage(quotes_handle);

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// 解析 SQLite 文件路径：默认 `<app_data_dir>/gangzi.db`。
fn resolve_db_path(handle: &tauri::AppHandle) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let dir = handle.path().app_data_dir()?;
    std::fs::create_dir_all(&dir).ok();
    Ok(dir.join("gangzi.db"))
}
