mod adapters; // Tauri commands 边界
mod domain; // DDD domain 层（types + 业务规则）
mod infrastructure; // I/O 适配 + cross-cutting infra
mod pipeline; // application 用例编排 + 顶级 chat / scheduler 等

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

            // spec account-module.md §2/§4：账户事件流的首个事实必须是
            // `account_initialized`；幂等，已存在则跳过。
            {
                let svc = pipeline::account::AccountService::new(handle.clone());
                if let Err(e) = svc.initialize_account_if_needed(
                    infrastructure::account::INITIAL_CASH,
                ) {
                    tracing::warn!(error = %e, "initialize_account_if_needed 失败（继续启动）");
                }
            }

            // 注入 Agent tool registry 工厂（pipeline 不依赖 adapters，靠 OnceLock 反转控制）。
            // factory 按 profile 过滤 allowed tools，spec §2 / §4。
            pipeline::agent::tools::install_registry_factory(
                adapters::agent_tools::registry_factory,
            );

            // StrategyCard：首次启动 seed baseline 策略卡
            if let Err(e) =
                infrastructure::agent_runtime::strategy_cards_repo::seed_baseline_if_empty(&handle)
            {
                tracing::warn!(error = %e, "seed baseline strategy card 失败（跳过，不阻塞启动）");
            }

            // Agent Runtime 恢复：把进程退出时还在 running 的 run 标记 failed
            if let Err(e) = infrastructure::agent_runtime::runs_repo::recover_interrupted(&handle) {
                tracing::warn!(error = %e, "recover interrupted agent runs 失败");
            }
            if let Err(e) = infrastructure::agent_runtime::news_buffer_repo::recover_orphans(&handle)
            {
                tracing::warn!(error = %e, "recover news buffer orphans 失败");
            }
            match infrastructure::agent_runtime::trade_intents_repo::recover_submitted(&handle) {
                Ok((scanned, recovered, rejected)) if scanned > 0 => {
                    tracing::info!(
                        scanned,
                        recovered,
                        rejected,
                        "TradeIntent submitted recovery 完成"
                    );
                    if rejected > 0 {
                        // spec agent-runtime-module.md §8「不可恢复错误写入失败状态和 UI 可见事件」
                        use tauri::Emitter;
                        let _ = handle.emit(
                            "trade-intent-recovery-failed",
                            serde_json::json!({
                                "scanned": scanned,
                                "recovered": recovered,
                                "rejected": rejected,
                                "message": "submission_unknown_no_account_effect",
                            }),
                        );
                    }
                }
                Err(e) => tracing::warn!(error = %e, "TradeIntent recovery 失败"),
                _ => {}
            }

            // 后台 loop：news / market / account / kline warm
            pipeline::scheduler::spawn_all(app.handle().clone());

            // Agent Runtime 调度（事件路由 + recovery flows + 后台 run）
            pipeline::agent_runtime::spawn(app.handle().clone());
            Ok(())
        })
        // IPC surface = "前端真正会调用的 API"。
        .invoke_handler(tauri::generate_handler![
            // 应用初始化 / 用户 UI 设置
            adapters::app_state_commands::initialize_database,
            adapters::app_state_commands::load_app_state,
            adapters::app_state_commands::save_app_state,
            // Agent Runtime / Chat 入口
            adapters::chat_commands::send_chat_message_now,
            adapters::news_commands::run_news_refresh,
            // 模拟账户 IPC
            adapters::account_commands::get_account_snapshot,
            adapters::account_commands::list_positions,
            adapters::account_commands::list_simulated_positions,
            adapters::account_commands::list_position_events,
            adapters::account_commands::list_position_events_batch,
            adapters::account_commands::list_watchlist,
            adapters::account_commands::list_watchlist_with_info,
            adapters::account_commands::add_watchlist_code,
            adapters::account_commands::remove_watchlist_code,
            adapters::account_commands::get_default_watchlist,
            adapters::account_commands::reset_simulation_account,
            // 只读 list/get/count——前端 refetch 时用
            adapters::news_commands::get_news_items_by_ids,
            adapters::chat_commands::list_chat_messages,
            adapters::news_commands::list_news_items,
            adapters::chat_commands::search_chat_messages,
            // UI 直接渲染的辅助命令
            adapters::news_commands::fetch_article_content,
            adapters::quotes_commands::fetch_a_share_klines,
            adapters::quotes_commands::fetch_a_share_minutes,
            adapters::quotes_commands::fetch_minute_klines,
            adapters::quotes_commands::fetch_a_share_quotes,
            adapters::quotes_commands::get_market_overview,
            adapters::quotes_commands::fetch_company_events,
            adapters::quotes_commands::scan_market,
            adapters::quotes_commands::scan_market_query,
            adapters::quotes_commands::fetch_stock_profile,
            // 今日市场
            adapters::market_commands::list_market_instruments,
            adapters::market_commands::run_market_quote_refresh_cmd,
            adapters::market_commands::snapshot_market_quotes,
            adapters::market_commands::snapshot_market_quotes_for,
            // TuShare 能力探测
            adapters::quotes_commands::probe_tushare_capabilities,
            adapters::app_commands::open_external_url,
            // 数据源配置
            adapters::quotes_commands::save_tushare_token,
            // Agent provider 配置
            adapters::agent_commands::get_agent_config,
            adapters::agent_commands::set_agent_config,
            adapters::agent_commands::verify_provider_model,
            adapters::agent_commands::get_agent_health,
            // 实时报价代理池 + 三源健康度
            adapters::proxy_commands::get_proxy_pool,
            adapters::proxy_commands::set_proxy_pool,
            adapters::proxy_commands::get_realtime_health,
            // Agent Runtime 命令
            adapters::runtime_commands::send_agent_message,
            adapters::runtime_commands::fetch_agent_state,
            adapters::runtime_commands::cancel_agent_run,
            adapters::strategy_commands::fetch_strategy_cards,
            adapters::strategy_commands::upsert_strategy_card,
            // Quotes / News / Account canonical commands（spec-aligned 入口）
            adapters::quotes_canonical::fetch_data,
            adapters::quotes_canonical::list_market,
            adapters::news_canonical::fetch_news,
            adapters::news_canonical::list_news_sources,
            adapters::news_canonical::refresh_news_canonical,
            adapters::news_canonical::warm_articles,
            adapters::account_canonical::fetch_account,
            adapters::account_canonical::operate_account,
            adapters::account_canonical::update_watchlist,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn main() {
    run();
}
