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
use crate::infrastructure::db::{run_migrations, AppDb};
use crate::infrastructure::news::{migrations as news_migrations, NewsRepository, SourceRegistry};
use crate::pipeline::news::scheduler::{
    spawn_news_refresh_scheduler, NewsSchedulerHandle, NEWS_REFRESH_INTERVAL_SECS,
};
use crate::pipeline::news::NewsService;

/// Tauri 主入口。`main.rs` 调用 `gangzi_terminal::run()` 启动 app。
pub fn run() {
    infrastructure::tracing::init();

    let specta_builder = Builder::<tauri::Wry>::new().commands(collect_commands![
        adapters::ping::ping,
        adapters::news::cmd::fetch_news,
        adapters::news::cmd::list_news_sources,
        adapters::news::cmd::warm_articles,
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

            // -- Event sink：scheduler + warm_articles 共用同一个 emit 回调
            let app_handle = app.handle().clone();
            let sink: crate::pipeline::news::scheduler::EventSink =
                Arc::new(move |payload| {
                    let envelope = wrap_news_refreshed(payload, None);
                    if let Err(e) = app_handle.emit(NEWS_REFRESHED_EVENT, envelope) {
                        tracing::warn!(target: "news.refresh.emit", error = %e, "failed to emit news-refreshed");
                    }
                });
            // 注入给 service：warm_articles 在 articleUpdatedCount > 0 时通过它 emit
            news_service.set_event_sink(Arc::clone(&sink));

            // -- Scheduler：默认 60s tick
            let handle: NewsSchedulerHandle = spawn_news_refresh_scheduler(
                Arc::clone(&news_service),
                Duration::from_secs(NEWS_REFRESH_INTERVAL_SECS),
                sink,
            );
            app.manage(handle);

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
