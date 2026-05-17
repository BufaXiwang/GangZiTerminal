mod adapters; // Tauri commands 边界
mod domain; // DDD domain 层（types + 业务规则）
mod infrastructure; // I/O 适配 + cross-cutting infra
mod pipeline; // application 用例编排 + 顶级 chat / scheduler 等

#[tauri::command]
fn open_external_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("只允许打开 http/https 原文链接。".to_string());
    }
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(|err| err.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let handle = app.handle().clone();
            // 初始化结构化日志——写到 app data dir 下 gangzi-terminal.log，按天滚。
            // 在 recover/spawn_all 之前调用，让那些动作就能落到日志里。
            // _guard 保留为静态生命周期，否则日志线程立刻 drop 写不出去。
            let log_guard = infrastructure::logging::init(&handle);
            std::mem::forget(log_guard); // 进程结束才清，简单粗暴

            // 代理池：从 KV 恢复用户在 Settings 配的 proxy list
            infrastructure::quotes::realtime::proxy_pool::hydrate(&handle);

            // 自选股 watchlist：从 KV 恢复到内存（account 模块）
            infrastructure::account::watchlist::hydrate(&handle);

            // Legacy positions 反向生成 events（启动一次，已迁移过的跳过）
            if let Err(e) = infrastructure::account::migration::migrate_legacy_positions(&handle) {
                tracing::warn!(error = %e, "legacy positions migration 失败（跳过，不阻塞启动）");
            }

            // Tokio 任务：scheduler 启动后台 loop（news / market / account / kline warm）
            pipeline::scheduler::spawn_all(app.handle().clone());
            Ok(())
        })
        // IPC surface = "前端真正会调用的 API"。
        // 内部写命令（append_chat_message / replace_* / save_* / claim/mark/revert news 等）
        // 不再暴露——它们是 pipeline 的实现细节，只通过 Rust 函数调用。
        // 把这些从 IPC 拿掉之后，"后端 pipeline 是唯一业务写入口"就从约定变成边界。
        .invoke_handler(tauri::generate_handler![
            // 应用初始化 / 用户 UI 设置
            adapters::app_state_commands::initialize_database,
            adapters::app_state_commands::load_app_state,
            adapters::app_state_commands::save_app_state,
            // 流水线触发（用户点击 / 计划任务）
            pipeline::chat::send_chat_message_now,
            adapters::news_commands::run_news_refresh,
            // 模拟账户 IPC（adapters/account_commands.rs）
            adapters::account_commands::get_account_snapshot,
            adapters::account_commands::list_positions,
            adapters::account_commands::list_position_events,
            adapters::account_commands::list_watchlist,
            adapters::account_commands::list_watchlist_with_info,
            adapters::account_commands::add_watchlist_code,
            adapters::account_commands::remove_watchlist_code,
            adapters::account_commands::get_default_watchlist,
            adapters::account_commands::reset_simulation_account,
            // 只读 list/get/count——前端 refetch 时用
            infrastructure::news::repository::get_news_items_by_ids,
            infrastructure::agent::repository::list_chat_messages,
            infrastructure::news::repository::list_news_items,
            infrastructure::account::repository::list_simulated_positions,
            infrastructure::account::repository::list_position_events_batch,
            infrastructure::agent::repository::search_chat_messages,
            // UI 直接渲染的辅助命令
            adapters::news_commands::fetch_article_content, // hover 看资讯原文
            adapters::quotes_commands::fetch_a_share_klines, // 日/周/月 K（TuShare）
            adapters::quotes_commands::fetch_a_share_minutes, // 分时（EM trends2）
            adapters::quotes_commands::fetch_minute_klines, // 分钟 K（1/5/15/30/60m, EM klines）
            adapters::quotes_commands::fetch_a_share_quotes, // 实时报价（基础字段）
            adapters::quotes_commands::get_market_overview, // 四大指数 + breadth
            adapters::quotes_commands::fetch_top_list,
            adapters::quotes_commands::fetch_moneyflow,
            adapters::quotes_commands::fetch_north_flow,
            adapters::quotes_commands::fetch_north_top10,
            adapters::quotes_commands::fetch_margin_summary,
            adapters::quotes_commands::fetch_company_events,
            adapters::quotes_commands::fetch_concept_list,
            adapters::quotes_commands::fetch_concept_members,
            adapters::quotes_commands::fetch_concept_performance,
            adapters::quotes_commands::scan_market,
            adapters::quotes_commands::scan_market_query,
            adapters::quotes_commands::fetch_stock_profile,
            // 今日市场——全市场列表 + 旁路实时
            adapters::market_commands::list_market_instruments, // 全市场静态档案（一次拉）
            pipeline::market_refresh::run_market_quote_refresh_cmd, // 手动触发旁路刷新
            pipeline::market_refresh::snapshot_market_quotes,   // 首次进页面 hydrate 全部当前快照
            pipeline::market_refresh::snapshot_market_quotes_for,
            // TuShare 能力探测（dev / 一次性）
            infrastructure::quotes::tushare::probe::probe_tushare_capabilities,
            open_external_url, // 打开浏览器
            // 数据源配置（SettingsPage → 数据源）
            pipeline::stocks::save_tushare_token,
            // Agent provider 配置（SettingsPage → AI 配置）
            adapters::agent_commands::get_agent_config,
            adapters::agent_commands::set_agent_config,
            adapters::agent_commands::verify_provider_model,
            // 实时报价代理池 + 三源健康度（SettingsPage → 网络 tab）
            adapters::proxy_commands::get_proxy_pool,
            adapters::proxy_commands::set_proxy_pool,
            adapters::proxy_commands::get_realtime_health,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn main() {
    run();
}
