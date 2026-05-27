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

use crate::adapters::news::events::{wrap_news_refreshed, NEWS_REFRESHED_EVENT};
use crate::adapters::quotes::events::{wrap_market_quotes_refreshed, MARKET_QUOTES_REFRESHED_EVENT};
use crate::infrastructure::agent::{
    bootstrap as bootstrap_agent_infra, migrations as agent_migrations,
};
use crate::infrastructure::db::{run_migrations, AppDb};
use crate::infrastructure::news::{migrations as news_migrations, NewsRepository, SourceRegistry};
use crate::infrastructure::quotes::{migrations as quotes_migrations, QuotesConfig};
use crate::pipeline::news::scheduler::{
    spawn_news_refresh_scheduler, NewsSchedulerHandle, NEWS_REFRESH_INTERVAL_SECS,
};
use crate::pipeline::news::NewsService;
use crate::pipeline::quotes::scheduler::{
    spawn_full_scheduler, QuotesSchedulerHandle, QuotesSchedulerIntervals,
    QUOTES_REFRESH_INTERVAL_SECS, QUOTES_SUBSCRIBED_INTERVAL_SECS,
};
use crate::pipeline::quotes::service::QuotesService;

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
        adapters::agent::cmd::agent_list_skills,
    ]);

    #[cfg(debug_assertions)]
    specta_builder
        .export(
            specta_typescript::Typescript::default(),
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
                let mut all = Vec::new();
                all.extend(news_migrations());
                all.extend(quotes_migrations());
                all.extend(agent_migrations());
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

            let news_handle: NewsSchedulerHandle = spawn_news_refresh_scheduler(
                Arc::clone(&news_service),
                Duration::from_secs(NEWS_REFRESH_INTERVAL_SECS),
                sink,
            );
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

            // -- Quotes startup catch-up（spec §5）：异步后台 task；不阻塞 setup。
            {
                let svc = Arc::clone(&quotes_service);
                tokio::spawn(async move {
                    // 1) universe enrich（TuShare 可用时；token 缺失会自动 skip）
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
                });
            }

            // -- Agent Infra bootstrap（Phase 1）
            // Spec: docs/design/agent-infra-module.md §5（SkillRegistry / 持久化）。
            // Runtime（Phase 3）会通过 `AgentInfra.registry.register_skill(...)` 注入
            // Quotes / News / Account facade skill，并新增 run_agent / send_user_message command。
            let agent_infra = bootstrap_agent_infra(db.clone());
            app.manage(agent_infra);

            // -- Quotes Scheduler（multi-tick）
            let quotes_handle: QuotesSchedulerHandle = spawn_full_scheduler(
                Arc::clone(&quotes_service),
                QuotesSchedulerIntervals {
                    universe_interval: Duration::from_secs(QUOTES_REFRESH_INTERVAL_SECS),
                    subscribed_interval: Duration::from_secs(QUOTES_SUBSCRIBED_INTERVAL_SECS),
                    daily_tick_interval: Duration::from_secs(60),
                },
            );
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
