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
use crate::domain::agent::runtime::AgentRunStatus;
use crate::domain::shared::Money;
use crate::infrastructure::agent::{
    bootstrap as bootstrap_agent_infra, default_skills_dir, default_workspace_dir,
};
use crate::infrastructure::db::{all_migrations, run_migrations, AppDb};
use crate::infrastructure::news::{NewsRepository, SourceRegistry};
use crate::infrastructure::quotes::QuotesConfig;
use crate::pipeline::account::{
    AccountQuoteGateway, AccountService, AccountServiceConfig, QuotesFacadeGateway,
};
use crate::pipeline::news::scheduler::{
    spawn_news_refresh_scheduler, NewsSchedulerHandle, NEWS_REFRESH_INTERVAL_SECS,
};
use crate::pipeline::news::NewsService;
use crate::pipeline::quotes::scheduler::{
    spawn_full_scheduler, QuotesSchedulerHandle, QuotesSchedulerIntervals,
};
use crate::pipeline::quotes::service::QuotesService;
use rust_decimal::Decimal;

/// 构造 specta command Builder（命令清单的唯一真源）。
/// 既用于 `run()` 的 invoke_handler + 启动期 TS 导出，也用于 test 里离线重新生成
/// `src/bindings.ts`（无需启动 GUI app）。
fn build_specta_builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new().commands(collect_commands![
        adapters::ping::ping,
        adapters::system::open_external,
        adapters::news::cmd::fetch_news,
        adapters::news::cmd::list_news_sources,
        adapters::news::cmd::warm_articles,
        adapters::quotes::cmd::list_market,
        adapters::quotes::cmd::fetch_data,
        adapters::quotes::cmd::scan_market,
        adapters::quotes::cmd::fetch_market_breadth,
        adapters::quotes::cmd::fetch_industry_heatmap,
        adapters::quotes::cmd::set_tushare_token,
        adapters::quotes::cmd::tushare_token_status,
        adapters::quotes::cmd::ensure_chart_data,
        adapters::quotes::cmd::extend_chart_history,
        adapters::quotes::cmd::fetch_kline_page,
        adapters::quotes::cmd::refresh_quotes,
        adapters::quotes::cmd::probe_tdx_hosts,
        adapters::quotes::cmd::forward_log,
        adapters::agent::cmd::agent_list_tools,
        adapters::agent::cmd::agent_channel_presets,
        adapters::agent::cmd::agent_discover_models,
        adapters::agent::cmd::agent_discover_models_for_channel,
        adapters::agent::cmd::agent_add_channel,
        adapters::agent::cmd::agent_update_channel,
        adapters::agent::cmd::agent_list_channels,
        adapters::agent::cmd::agent_remove_channel,
        adapters::agent::cmd::agent_set_active_channel,
        adapters::agent::cmd::agent_get_active_channel,
        adapters::account::cmd::fetch_account,
        // Spec: account-module.md §4 — operate_account 写入口只对 Agent tool /
        // 外部自动化决策运行时暴露，不能注册为 Tauri command 供前端直接 invoke。
        // 函数实现保留为 #[allow(dead_code)]，将由 Phase 3 Agent Runtime
        // 通过 ToolRegistry 注册为 tool。
        adapters::account::cmd::update_watchlist,
        adapters::account::cmd::mark_trigger_handled,
        adapters::account::cmd::rebuild_account_snapshot,
        adapters::account::cmd::account_reset,
        adapters::account::cmd::list_account_archives,
        adapters::agent::runtime_cmd::agent_send_message,
        adapters::agent::runtime_cmd::agent_fetch_state,
        adapters::agent::runtime_cmd::agent_fetch_strategy,
        adapters::agent::runtime_cmd::agent_upsert_strategy,
        adapters::agent::runtime_cmd::agent_run_review,
        adapters::agent::runtime_cmd::agent_list_review_reports,
        adapters::agent::runtime_cmd::agent_list_conversations,
        adapters::agent::runtime_cmd::agent_load_conversation,
        adapters::agent::runtime_cmd::agent_load_tool_calls,
        adapters::agent::runtime_cmd::agent_read_review_report,
        adapters::agent::runtime_cmd::agent_cancel_run,
        adapters::agent::runtime_cmd::agent_get_news_auto_analysis,
        adapters::agent::runtime_cmd::agent_set_news_auto_analysis,
    ])
}

/// 把 specta 类型导出到 `src/bindings.ts`。i64 输出为 TS Number（实际值远小于
/// MAX_SAFE_INTEGER；Money/Price/Amount 走 rust_decimal serialize-as-string 不受影响）。
#[cfg(debug_assertions)]
fn export_ts_bindings(builder: &Builder<tauri::Wry>) {
    builder
        .export(
            specta_typescript::Typescript::default()
                .bigint(specta_typescript::BigIntExportBehavior::Number),
            "../src/bindings.ts",
        )
        .expect("failed to export specta typescript bindings");
}

/// Tauri 主入口。`main.rs` 调用 `gangzi_terminal::run()` 启动 app。
pub fn run() {
    infrastructure::tracing::init();

    let specta_builder = build_specta_builder();

    #[cfg(debug_assertions)]
    export_ts_bindings(&specta_builder);

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(specta_builder.invoke_handler())
        .setup(move |app| {
            specta_builder.mount_events(app);

            // -- DB / migrations
            let db_path = resolve_db_path(app.handle())?;
            let db = AppDb::open(&db_path).expect("failed to open AppDb");
            db.with(|conn| {
                // 全局 migration 列表由 `all_migrations()` 统一维护（infrastructure/db/migrations.rs）。
                // append-only：新增 migration 只能追加到 `all_migrations()` 尾部。
                run_migrations(conn, all_migrations()).expect("failed to apply migrations");
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
            // 除了向前端 emit，还把新入库的 newsId 灌进 Runtime 的 news buffer（滚动 4h），
            // 让 news agent 能被 M/N 触发（spec: agent-runtime-module.md §5 / §6）。
            // NewsBufferService 无状态、只读写 agent_news_buffer 表，故独立 new 一个共享同一 db 即可。
            let nb_repo = Arc::new(
                crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo::new(db.clone()),
            );
            let news_buffer_ingest = Arc::new(
                crate::pipeline::agent_runtime::NewsBufferService::new(
                    Arc::clone(&nb_repo),
                    crate::pipeline::agent_runtime::NewsBufferConfig::default(),
                ),
            );
            // 自动分析开关门 facade（spec §5：关闭时不 ingest）。
            let nb_settings = Arc::new(
                crate::pipeline::agent_runtime::settings::RuntimeSettings::new(Arc::clone(&nb_repo)),
            );
            let app_handle = app.handle().clone();
            let nb_for_sink = Arc::clone(&news_buffer_ingest);
            let settings_for_sink = Arc::clone(&nb_settings);
            let db_for_sink = db.clone();
            let sink: crate::pipeline::news::scheduler::EventSink =
                Arc::new(move |payload| {
                    // 开关门：自动分析关闭时不入队（spec §5 默认关闭）。
                    if settings_for_sink.news_auto_analysis_enabled() {
                        // 合并 newIds ∪ updatedIds ∪ articleUpdatedNewsIds 去重（spec §5 生产者）。
                        // 纯 failed/warnings 变化的 id 不在这三组里，自然不入队。
                        let ids = crate::pipeline::agent_runtime::news_buffer::merge_refreshed_ids(
                            &payload.new_ids,
                            &payload.updated_ids,
                            &payload.article_updated_news_ids,
                        );
                        if !ids.is_empty() {
                            // 批量查 published_at（newest-first 排序锚点，spec §5）。
                            let repo = crate::infrastructure::news::NewsRepository::new(&db_for_sink);
                            let pub_ats = repo.get_news_items_by_ids(&ids).unwrap_or_default();
                            let items: Vec<(String, Option<chrono::DateTime<chrono::Utc>>)> = ids
                                .iter()
                                .enumerate()
                                .map(|(i, id)| {
                                    let pa = pub_ats
                                        .get(i)
                                        .and_then(|o| o.as_ref())
                                        .and_then(|it| it.published_at);
                                    (id.clone(), pa)
                                })
                                .collect();
                            if let Err(e) = nb_for_sink.ingest(&items, chrono::Utc::now()) {
                                tracing::warn!(target: "runtime.news_buffer", error = %e, "ingest news ids failed");
                            }
                        }
                    }
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
            // TuShare token：优先读设置页持久化的 <appData>/tushare.token，回退环境变量。
            let tushare_token = adapters::quotes::cmd::read_persisted_tushare_token(app.handle())
                .or_else(|| std::env::var("TUSHARE_TOKEN").ok().filter(|s| !s.is_empty()));
            let quotes_service = Arc::new(
                QuotesService::new(db.clone(), QuotesConfig { tushare_token })
                    .expect("failed to build QuotesService"),
            );
            app.manage(Arc::clone(&quotes_service));

            // Runtime 句柄占位（OnceLock）：RuntimeServices 在 Account 块之后才构造，但 account
            // event sink 在它之前接线。sink 捕获 holder，runtime 构造后填入；触发时读 holder。
            // Spec: docs/design/agent-runtime-module.md §6 账户自驱 quote tick + §8 幂等。
            let rt_holder: Arc<std::sync::OnceLock<Arc<crate::pipeline::agent_runtime::RuntimeServices>>> =
                Arc::new(std::sync::OnceLock::new());

            // -- Quotes Event sink（纯 UI emit）
            // 账户重建已与 universe 解耦：账户走自有 focused refresh quote tick 自驱（spec §6），
            // 不再消费 `market-quotes-refreshed` 重建账户。此 sink 只把刷新事件透传给前端 / 行情读模型。
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

            // -- 启动预热 TDX 连接池（spec §连接池「启动时探测」）：后台 probe + 预建 active 连接，
            // 让首笔用户请求（含盘后/休市冷开）免去 ~3s 冷探测 + 建连延迟。不阻塞 setup、不依赖交易时段。
            {
                let svc = Arc::clone(&quotes_service);
                tauri::async_runtime::spawn(async move {
                    let _ = tokio::task::spawn_blocking(move || svc.tdx.warm()).await;
                });
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

                    // 3b) 冷启动 universe intraday 首刷（spec §5）：刷新窗内立即跑一次全市场盘中刷新，
                    //     不等 scheduler 首个 +60s tick——否则冷启动后全市场盘中价最长等近 1min 才首刷。
                    //     复用渐进批量（stock→index→fund + 200/批 progress），前端增量填充。
                    if svc.market_time_now().is_in_quote_refresh_window {
                        let req = crate::pipeline::quotes::service::RefreshMarketQuotesRequest {
                            scope: crate::domain::quotes::RefreshMarketQuotesScope::Universe,
                            purpose: crate::domain::quotes::RefreshPurpose::Intraday,
                            trade_date: None,
                        };
                        if let Err(e) = svc.refresh_market_quotes(req).await {
                            tracing::warn!(target: "quotes.startup", error = ?e, "universe intraday first-refresh failed");
                        }
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
            // Spec: docs/design/agent-infra-module.md §5（ToolRegistry / 持久化）。
            // Runtime（Phase 3）会通过 `AgentInfra.registry.register_tool(...)` 注入
            // Quotes / News / Account facade tool，并新增 run_agent / send_user_message command。
            //
            // 本地通用 tool（read/write/edit/run_bash）的约定级沙箱工作区根：
            // Spec: docs/design/agent-runtime-module.md §4.2。取 `<appData>/gangzi/workspace`；
            // 解析不到 appData 时退到占位默认（Phase 3 adapter 可进一步定制注入）。
            let workspace_dir = app
                .handle()
                .path()
                .app_data_dir()
                .map(|d| d.join("gangzi").join("workspace"))
                .unwrap_or_else(|_| default_workspace_dir());
            // Skill（playbook）存盘根 `<appData>/gangzi/skills`，独立于 workspace。
            // Spec: docs/design/agent-runtime-module.md §Skills（渐进披露）。
            let skills_dir = app
                .handle()
                .path()
                .app_data_dir()
                .map(|d| d.join("gangzi").join("skills"))
                .unwrap_or_else(|_| default_skills_dir());
            let agent_infra = bootstrap_agent_infra(db.clone(), workspace_dir, skills_dir);
            // Runtime（Phase 3）复用 Infra 的消息持久化 / payload / channels / registry（克隆句柄，便宜）。
            let runtime_agent_messages_repo = agent_infra.repo.clone();
            let runtime_payload_store = agent_infra.payload_store.clone();
            let runtime_channels_repo = agent_infra.channels_repo.clone();
            let runtime_infra_registry = agent_infra.registry.clone();
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
            // 模拟账户初始资金（单一真源，config 与 init 用同一个值）。
            let initial_cash = Money(Decimal::from(20_000));
            let account_service = Arc::new(AccountService::new(
                db.clone(),
                account_gateway,
                AccountServiceConfig {
                    initial_cash,
                    ..AccountServiceConfig::default()
                },
            ));
            // 初始化账户（幂等）
            if let Err(code) = account_service
                .initialize_account_if_needed(initial_cash)
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
            // 账户触发 → Runtime account_trigger run（实时消费）。复用上方 quotes 块声明的 rt_holder。
            let rt_for_trigger = Arc::clone(&rt_holder);
            let app_handle_b = app.handle().clone();
            let triggered_sink: crate::pipeline::account::service::AccountTriggeredSink =
                Arc::new(move |inner| {
                    // 先消费（去重 → 起 account_trigger run，异步），再 emit 给前端。
                    if let Some(rt) = rt_for_trigger.get() {
                        let t = &inner.trigger;
                        let trigger_id = t.trigger_id.clone();
                        let order_id = t.order_id.clone();
                        let summary = format!(
                            "{:?} {} {}",
                            t.trigger_type,
                            t.ts_code.as_ref().map(|c| c.as_str()).unwrap_or(""),
                            t.threshold.clone().unwrap_or_default(),
                        );
                        match rt.triggers.begin_account_trigger(&trigger_id) {
                            Ok(true) => {
                                let rt2 = Arc::clone(rt);
                                tauri::async_runtime::spawn(async move {
                                    match rt2
                                        .run_account_trigger(trigger_id.clone(), order_id, summary)
                                        .await
                                    {
                                        Ok(run) if run.status == AgentRunStatus::Completed => {
                                            // Only mark handled on successful completion（spec §3/§6/§11）。
                                            // Failed/Cancelled runs should NOT consume the trigger so it can retry.
                                            if let Err(e) = rt2
                                                .deps
                                                .account
                                                .mark_trigger_handled(&trigger_id)
                                                .await
                                            {
                                                tracing::warn!(
                                                    target: "runtime.account_trigger",
                                                    trigger_id = %trigger_id,
                                                    error = %e.message,
                                                    "mark_trigger_handled failed (non-fatal)"
                                                );
                                            }
                                            let _ = rt2.triggers.mark_account_trigger_consumed(
                                                &trigger_id,
                                                Some(&run.run_id),
                                            );
                                        }
                                        Ok(run) => {
                                            // run returned but not Completed (Failed/Cancelled) → don't consume, let it retry
                                            tracing::warn!(
                                                target: "runtime.account_trigger",
                                                trigger_id = %trigger_id,
                                                run_id = %run.run_id,
                                                status = ?run.status,
                                                "account_trigger run non-Completed → trigger not marked handled"
                                            );
                                            let _ = rt2.triggers.mark_account_trigger_failed(
                                                &trigger_id,
                                                &format!("run {} ended with {:?}", run.run_id, run.status),
                                            );
                                        }
                                        Err(e) => {
                                            let _ = rt2
                                                .triggers
                                                .mark_account_trigger_failed(&trigger_id, &e.to_string());
                                            tracing::warn!(target: "runtime.account_trigger", error = %e, "account_trigger run failed");
                                        }
                                    }
                                });
                            }
                            Ok(false) => {} // 已消费 / 处理中 → 不重触发
                            Err(e) => tracing::warn!(target: "runtime.account_trigger", error = %e, "begin_account_trigger failed"),
                        }
                    }
                    let env = wrap_account_triggered(inner, None);
                    if let Err(e) = app_handle_b.emit(ACCOUNT_TRIGGERED_EVENT, env) {
                        tracing::warn!(target: "account.emit", error = %e, "failed to emit account-triggered");
                    }
                });
            account_service.set_triggered_sink(triggered_sink);
            app.manage(Arc::clone(&account_service));

            // 账户触发评估 cadence 统一由 Runtime 自驱 quote tick 承担
            // （spawn_account_eval_tick_scheduler，spec agent-runtime §6/§8）——
            // 旧的 Account BC 60s 纯兜底 eval scheduler 已退役。

            // -- CLI 本地只读端点（spec: docs/design/cli-module.md §2）
            // 127.0.0.1 随机端口 + <appData>/cli.port 发现；复用三个 Gateway（与 Agent 读 tool 同源）。
            {
                let cli_gateways = adapters::cli::CliGateways::new(
                    Arc::clone(&quotes_service),
                    Arc::clone(&news_service),
                    Arc::clone(&account_service),
                );
                let portfile = app
                    .handle()
                    .path()
                    .app_data_dir()
                    .map(|d| d.join("cli.port"))
                    .unwrap_or_else(|_| std::env::temp_dir().join("gangzi-cli.port"));
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = adapters::cli::serve(cli_gateways, portfile).await {
                        tracing::warn!(target: "cli.endpoint", error = %e, "CLI endpoint exited");
                    }
                });
            }

            // -- Quotes Scheduler（multi-tick）— 同样需要 runtime context 包装
            let quotes_handle: QuotesSchedulerHandle =
                tauri::async_runtime::block_on(async {
                    spawn_full_scheduler(
                        Arc::clone(&quotes_service),
                        QuotesSchedulerIntervals::default(),
                    )
                });
            app.manage(quotes_handle);

            // -- Agent Runtime bootstrap（Phase 3）
            // Spec: docs/design/agent-runtime-module.md §10 实现映射
            {
                use crate::infrastructure::agent::runtime_repo::AgentRuntimeRepo;
                use crate::pipeline::agent_runtime::executor::AgentEventSink;
                use crate::pipeline::agent_runtime::recovery::RecoveryService;
                use crate::pipeline::agent_runtime::runs::{RuntimeEvent, RuntimeEventSink};
                use crate::pipeline::agent_runtime::{
                    build_runtime_services, spawn_account_eval_tick_scheduler,
                    spawn_news_buffer_scheduler, RuntimeBootstrap,
                };

                // 启动恢复（§8）：submitting 用 clientOrderId 对账 / running→failed / news 孤儿回 pending。
                // 对账闭包：用 clientOrderId 反查 Account 是否已有对应订单（spec §3/§8「无猜失败盲区」）。
                let recovery_repo = Arc::new(AgentRuntimeRepo::new(db.clone()));
                let recon_account = Arc::clone(&account_service);
                let reconciler = move |coid: &str| -> Option<String> {
                    recon_account
                        .find_order_by_client_order_id(coid)
                        .map(|o| o.order_id)
                };
                match RecoveryService::new(recovery_repo)
                    .recover_on_startup_with(Some(&reconciler))
                {
                    Ok(s) => tracing::info!(
                        target: "runtime.recovery",
                        interrupted = s.interrupted_runs,
                        reconciled = s.reconciled_trades,
                        reconciled_accepted = s.reconciled_accepted,
                        reconciled_failed = s.reconciled_failed,
                        news_reset = s.reset_news_items,
                        consumptions_recovered = s.recovered_consumptions,
                        "runtime startup recovery done"
                    ),
                    Err(e) => tracing::warn!(target: "runtime.recovery", error = %e, "recovery failed"),
                }

                // AgentEvent（token/tool/run 流）→ 前端 agent-event。
                let ah_evt = app.handle().clone();
                let agent_event_sink: AgentEventSink = Arc::new(move |ev| {
                    let env = crate::adapters::agent::wrap_agent_event(ev, None);
                    if let Err(e) = ah_evt.emit(crate::adapters::agent::AGENT_EVENT, env) {
                        tracing::warn!(target: "runtime.emit", error = %e, "emit agent-event failed");
                    }
                });

                // RuntimeEvent（run 起止 / AnalysisResult）→ 前端 kebab 事件。
                let ah_run = app.handle().clone();
                let runtime_event_sink: RuntimeEventSink = Arc::new(move |ev| {
                    let (name, payload) = match ev {
                        RuntimeEvent::RunStarted { run_id, run } => (
                            "agent-run-started",
                            serde_json::json!({"runId": run_id, "mode": run.mode, "trigger": run.trigger}),
                        ),
                        RuntimeEvent::RunFinished { run_id, status, error } => (
                            "agent-run-finished",
                            serde_json::json!({"runId": run_id, "status": status, "error": error}),
                        ),
                        RuntimeEvent::AnalysisResultEmitted { result_id, run_id, kind } => (
                            "agent-analysis-result",
                            serde_json::json!({"resultId": result_id, "runId": run_id, "kind": kind}),
                        ),
                    };
                    if let Err(e) = ah_run.emit(name, payload) {
                        tracing::warn!(target: "runtime.emit", error = %e, "emit runtime event failed");
                    }
                });

                // news buffer age-out 丢弃计数 → 前端 agent-news-buffer-dropped（spec §5/§7）。
                let ah_drop = app.handle().clone();
                let buffer_dropped_sink: Arc<dyn Fn(u32, u32) + Send + Sync> =
                    Arc::new(move |count, window_secs| {
                        let payload = serde_json::json!({
                            "count": count,
                            "windowSecs": window_secs,
                            "occurredAt": chrono::Utc::now().to_rfc3339(),
                        });
                        if let Err(e) = ah_drop.emit("agent-news-buffer-dropped", payload) {
                            tracing::warn!(target: "runtime.emit", error = %e, "emit agent-news-buffer-dropped failed");
                        }
                    });

                let runtime_services = Arc::new(build_runtime_services(RuntimeBootstrap {
                    db: db.clone(),
                    agent_messages_repo: runtime_agent_messages_repo,
                    payload_store: runtime_payload_store,
                    channels: runtime_channels_repo,
                    quotes: Arc::clone(&quotes_service),
                    news: Arc::clone(&news_service),
                    account: Arc::clone(&account_service),
                    agent_event_sink: Some(agent_event_sink),
                    runtime_event_sink: Some(runtime_event_sink),
                    // max_turns / token_budget / review_min_sample_trades / eval_batch_size
                    // 已改由 Runtime settings（spec §8）提供，缺失用缺省。
                    infra_registry: runtime_infra_registry,
                    reports_dir: app
                        .handle()
                        .path()
                        .app_data_dir()
                        .map(|d| d.join("gangzi").join("workspace").join("reviews"))
                        .unwrap_or_else(|_| std::env::temp_dir().join("gangzi-reviews")),
                    buffer_dropped_sink: Some(buffer_dropped_sink),
                }));

                // 填入 OnceLock：账户触发 sink 自此可起 account_trigger run。
                let _ = rt_holder.set(Arc::clone(&runtime_services));

                // 启动恢复 ⑤（spec §8）：从 Account 补扫未 handled trigger，经既有 account_trigger 路由
                // （dedupe 保证不重复）。需 active channel + runtime_services 就绪，故放在此处异步起。
                let rescan_rt = Arc::clone(&runtime_services);
                tauri::async_runtime::spawn(async move {
                    let n = rescan_rt.rescan_unhandled_triggers(200).await;
                    if n > 0 {
                        tracing::info!(target: "runtime.recovery", routed = n, "startup rescan of unhandled triggers done");
                    }
                });

                // news buffer 调度（M/N 触发 + age-out）；基础轮询 60s。
                let news_buffer_handle = tauri::async_runtime::block_on(async {
                    spawn_news_buffer_scheduler(
                        Arc::clone(&runtime_services),
                        std::time::Duration::from_secs(60),
                    )
                });
                app.manage(news_buffer_handle);

                // 账户自驱 quote tick（spec §6）：每 `account_trigger_eval_interval_secs`（缺省 10s）
                // 对 subscribed_codes ∪ core_indexes 做 focused refresh → rebuild → eval 分页耗尽。
                // 账户与 universe 全市场刷新解耦——不再搭 universe 便车。
                let account_tick_handle = tauri::async_runtime::block_on(async {
                    spawn_account_eval_tick_scheduler(Arc::clone(&runtime_services))
                });
                app.manage(account_tick_handle);
                app.manage(runtime_services);
            }

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

#[cfg(test)]
mod specta_export_tests {
    use super::*;

    /// 离线重新生成 `src/bindings.ts`（无需启动 GUI app）。命令清单变更后跑
    /// `cargo test export_ts_bindings_is_current` 即可刷新前端类型。
    #[test]
    fn export_ts_bindings_is_current() {
        let builder = build_specta_builder();
        export_ts_bindings(&builder);
    }

    /// 用 app 完全一致的 migration 列表 + 顺序在内存库跑一遍，验证迁移序安全
    /// （reorder 后 agent-002 落到全局末尾，不重跑已应用的 quotes 建表）。
    #[test]
    fn full_migration_list_applies_clean() {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|conn| {
            run_migrations(conn, all_migrations()).expect("full migration list must apply clean");
            // agent M002 列存在
            let cols: Vec<String> = conn
                .prepare("PRAGMA table_info(agent_provider_channels)")
                .unwrap()
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert!(cols.iter().any(|c| c == "api_key"));
            assert!(cols.iter().any(|c| c == "is_active"));
            // 全局末尾追加的 Account-owned 表存在。
            let day_eq_cols: Vec<String> = conn
                .prepare("PRAGMA table_info(account_day_equity)")
                .unwrap()
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert!(day_eq_cols.iter().any(|c| c == "open_equity"));
            assert!(day_eq_cols.iter().any(|c| c == "high_equity"));
        });
    }

    /// 存量 DB 升级路径：先 apply 到 agent 末尾（模拟旧版本 DB），再追加 tail，
    /// 验证只补 account_day_equity 一条、不重跑既有建表（CREATE TABLE 不 panic）。
    #[test]
    fn migration_tail_safe_on_existing_db() {
        use crate::infrastructure::account::migrations as account_mig;
        use crate::infrastructure::agent::migrations as agent_mig;
        use crate::infrastructure::news::migrations as news_mig;
        use crate::infrastructure::quotes::migrations as quotes_mig;
        let db = AppDb::open_in_memory().unwrap();
        db.with(|conn| {
            // ① 旧版本：news → account → quotes → agent 基础段（无任何 tail）。
            let mut old = Vec::new();
            old.extend(news_mig::migrations());
            old.extend(account_mig::migrations());
            old.extend(quotes_mig::migrations());
            old.extend(agent_mig::migrations_base());
            let old_len = old.len();
            run_migrations(conn, old).expect("old migration set applies");
            let uv_before: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(uv_before as usize, old_len, "旧 DB user_version = 旧 migration 数");

            // ② 升级：全量 all_migrations() 再 apply —— 只补 tail，既有建表不重跑。
            let full = all_migrations();
            let full_len = full.len();
            run_migrations(conn, full).expect("升级追加 tail 不破存量 DB");
            let uv_after: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(uv_after as usize, full_len, "升级后只前进 tail 长度");
            // 新表可用。
            conn.execute(
                "INSERT INTO account_day_equity (trade_date, open_equity, high_equity, captured_at)
                 VALUES ('20260605', '1000000', '1000000', '2026-06-05T00:00:00Z')",
                [],
            )
            .unwrap();
        });
    }

    /// 一次性把测试服务商配置写入指定 DB（migration-safe）。`#[ignore]`，凭证 / DB
    /// 路径全走环境变量（不硬编码 secret）。先对 real DB 的副本跑验证迁移安全，再对
    /// real DB 跑记录配置。env：SEED_DB_PATH + 各 *_KEY；缺哪个跳哪个。
    #[test]
    #[ignore]
    fn seed_provider_channels() {
        use crate::domain::agent::{ProviderChannel, WireFormat};
        use crate::infrastructure::agent::ProviderChannelsRepo;

        let db_path = std::env::var("SEED_DB_PATH").expect("SEED_DB_PATH required");
        let db = AppDb::open(&std::path::PathBuf::from(&db_path)).unwrap();
        // 应用全量 migration（与 run() 同序）——既有 DB 只补未应用的尾部。
        db.with(|conn| {
            run_migrations(conn, all_migrations()).expect("migrations apply clean on seed DB");
        });
        let repo = ProviderChannelsRepo::new(db.clone());

        let mk = |id: &str, provider: &str, wf: WireFormat, base: &str, key: String, model: &str| {
            ProviderChannel {
                channel_id: id.into(),
                provider: provider.into(),
                wire_format: wf,
                base_url: Some(base.into()),
                api_key: key,
                model: model.into(),
                stream: true,
                enabled: true,
                supports_vision: false,
                supports_thinking: false,
                max_output_tokens: None,
                context_window_tokens: None,
                thinking_budget_tokens: None,
            }
        };

        let mut seeded: Vec<&str> = Vec::new();
        if let Ok(k) = std::env::var("SEED_DS_KEY") {
            let id = "ch_seed_deepseek";
            let _ = repo.remove(id);
            repo.add(&mk(id, "DeepSeek", WireFormat::ChatCompletions, "https://api.deepseek.com", k, "deepseek-v4-flash")).unwrap();
            seeded.push(id);
        }
        if let Ok(k) = std::env::var("SEED_ANT_KEY") {
            let id = "ch_seed_anthropic";
            let base = std::env::var("SEED_ANT_BASE").unwrap_or_else(|_| "https://api.anthropic.com".into());
            let model = std::env::var("SEED_ANT_MODEL")
                .unwrap_or_else(|_| "claude-opus-4-5-20251101".into());
            let _ = repo.remove(id);
            repo.add(&mk(id, "Anthropic", WireFormat::Messages, &base, k, &model)).unwrap();
            seeded.push(id);
        }
        if let Ok(k) = std::env::var("SEED_OAI_KEY") {
            let id = "ch_seed_openai";
            let base = std::env::var("SEED_OAI_BASE").unwrap_or_else(|_| "https://api.openai.com".into());
            let _ = repo.remove(id);
            repo.add(&mk(id, "OpenAI", WireFormat::Responses, &base, k, "gpt-5")).unwrap();
            seeded.push(id);
        }
        // 设一个当前模型：优先 deepseek（快），否则任意已 seed 的。
        if let Some(active) = seeded.iter().find(|s| **s == "ch_seed_deepseek").or_else(|| seeded.first()) {
            repo.set_active(active).unwrap();
        }
        let all = repo.list().unwrap();
        println!("seeded {} channels; total in DB = {}", seeded.len(), all.len());
        for c in &all {
            println!("  {} ({}) [{:?}] active={} keySet={}", c.model, c.provider, c.wire_format, c.channel_id, !c.api_key.is_empty());
        }
        assert!(!seeded.is_empty(), "no SEED_*_KEY env provided");
    }
}
