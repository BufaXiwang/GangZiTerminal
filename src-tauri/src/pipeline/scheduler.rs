//! 后端调度循环——把所有"定时器"全部从 React useEffect 挪到这里。
//!
//! Tokio task，setup 阶段 spawn，进程生命周期内常驻：
//! - news_refresh_loop：按 refreshInterval 拉资讯（agent 在 chat 里 search_news 用）
//! - stocks_refresh_loop / market_universe_loop / market_quote_loop / kline_warm_loop
//! - account_snapshot_loop / tushare_probe_once
//!
//! 每条 loop 在 tick 时从 app_state 读最新设置（autoRefresh / refreshInterval）——
//! 前端改设置后下一 tick 即生效。
//!
//! 可靠性约束：
//! - **错误不能吞**：每个 tick 的 Err 都 `tracing::warn!` 落日志
//! - **连续失败熔断**：[`FailureCounter`] 累计连续失败次数，触达阈值后退避更长

use crate::pipeline;
use serde_json::Value;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// 默认 60 秒一刷——盘中能看到分钟级变化。前端可以在设置里调到 15/30 秒
/// 或拉长到 5 分钟。
const DEFAULT_REFRESH_INTERVAL_MS: u64 = 60_000;
/// 盘外刷新间隔倍数——盘外行情几乎不动，没必要按盘中节奏一直拉。
const OFF_HOURS_INTERVAL_MULTIPLIER: u64 = 5;

const KEY_AUTO_REFRESH: &str = "gangzi-terminal.auto-refresh";
const KEY_REFRESH_INTERVAL: &str = "gangzi-terminal.refresh-interval";

pub fn spawn_all(app: AppHandle) {
    // briefing/review 已下线——agent 只剩 chat 能力，由用户主动发起
    tauri::async_runtime::spawn(news_refresh_loop(app.clone()));
    tauri::async_runtime::spawn(stocks_refresh_loop(app.clone()));
    tauri::async_runtime::spawn(market_quote_loop(app.clone()));
    tauri::async_runtime::spawn(market_universe_loop(app.clone()));
    tauri::async_runtime::spawn(kline_warm_loop(app.clone()));
    tauri::async_runtime::spawn(account_snapshot_loop(app.clone()));
    tauri::async_runtime::spawn(tushare_probe_once(app));
}

// ====== 全市场 universe 刷新 loop ======
//
// 与 market_quote_loop（active_set 高频小批，15s）并行：
// - 数据源：全市场股票 + 指数 + 基金（≈13000 标的）
// - 三段刷新：股票（按涨跌幅倒序）→ 指数 → 基金
// - 实现：TdxConnectionPool 8 连接并行，BJ 走 EM 合流
// - 频率：盘中 60s / 盘外 5min / 周末 30min
// - 启动延迟 20s，给 stocks_refresh_loop 先 hydrate 三表的时间

async fn market_universe_loop(app: AppHandle) {
    tokio::time::sleep(Duration::from_secs(20)).await;

    loop {
        let started = std::time::Instant::now();
        let summary = crate::pipeline::market_universe::run_universe_refresh(&app).await;
        let elapsed_ms = started.elapsed().as_millis();
        tracing::info!(
            stocks = summary.stock_count,
            indexes = summary.index_count,
            funds = summary.fund_count,
            elapsed_ms,
            "universe loop tick 完成"
        );

        let interval = market_universe_interval();
        tokio::time::sleep(interval).await;
    }
}

fn market_universe_interval() -> Duration {
    let beijing = chrono::Utc::now() + chrono::Duration::hours(8);
    use chrono::Datelike;
    let weekday = beijing.weekday();
    let is_weekend = matches!(weekday, chrono::Weekday::Sat | chrono::Weekday::Sun);

    if is_weekend {
        Duration::from_secs(1800) // 周末 30min
    } else if crate::domain::quotes::is_a_share_trading_hours() {
        Duration::from_secs(60) // 盘中 60s
    } else {
        Duration::from_secs(300) // 盘外 5min
    }
}

// ====== Account 快照 loop ======
//
// 维护 ACCOUNT_SNAPSHOT（in-memory）的"新鲜度"，三种触发：
// 1. **事件触发**：listen `market-quotes-refreshed`——quotes refresh 完成后立即重算
//    （MARKET_SNAPSHOT 刚更新，account 估值会跟着变）
// 2. **写后触发**：AccountService 在每个写操作后已经主动 put cache + emit；这里不重复
// 3. **兜底定时**：盘中 10s / 盘外 60s 强制 refresh——覆盖"snapshot 没人改但价没刷新"边角
//
// emit `account-snapshot-updated` 让前端 hook 重新拉 IPC `get_account_snapshot`。

async fn account_snapshot_loop(app: AppHandle) {
    // 1. 注册事件监听——market-quotes-refreshed 一来就 spawn 一次刷新
    {
        use tauri::Listener;
        let app_for_handler = app.clone();
        app.listen("market-quotes-refreshed", move |_event| {
            let app = app_for_handler.clone();
            tauri::async_runtime::spawn(async move {
                refresh_account_snapshot(&app).await;
            });
        });
    }

    // 2. 启动延迟——等其他 loop 先 hydrate
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 3. 兜底定时循环
    loop {
        refresh_account_snapshot(&app).await;
        let secs = if crate::domain::quotes::is_a_share_trading_hours() {
            10
        } else {
            60
        };
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }
}

async fn refresh_account_snapshot(app: &AppHandle) {
    let service = crate::pipeline::account::AccountService::new(app.clone());
    match service.snapshot() {
        Ok(snap) => {
            crate::infrastructure::account::snapshot_cache::put(snap);
            let _ = app.emit(
                crate::pipeline::account::service::EVENT_ACCOUNT_SNAPSHOT_UPDATED,
                serde_json::json!({}),
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "account snapshot 刷新失败");
        }
    }
}

// ====== K 线预热 ======
//
// 启动后 30s 跑一次，把 Account subscriptions 的日/周/月 K 拉到本地。
// 之后每天 16:00 北京时间盘后再跑（确保当天数据已落库）。

async fn kline_warm_loop(app: AppHandle) {
    tokio::time::sleep(Duration::from_secs(30)).await;
    crate::pipeline::kline_warm::warm_klines_once(&app).await;

    loop {
        let wait = duration_until_next_16_beijing();
        tracing::info!(secs = wait.as_secs(), "下一次 K 线预热等待");
        tokio::time::sleep(wait).await;
        crate::pipeline::kline_warm::warm_klines_once(&app).await;
    }
}

fn duration_until_next_16_beijing() -> Duration {
    let beijing = chrono::Utc::now() + chrono::Duration::hours(8);
    let today_16 = beijing
        .date_naive()
        .and_hms_opt(16, 0, 0)
        .expect("16:00 always valid");
    let target = if beijing.naive_utc() < today_16 {
        today_16
    } else {
        today_16 + chrono::Duration::days(1)
    };
    let delta = target - beijing.naive_utc();
    Duration::from_secs(delta.num_seconds().max(60) as u64)
}

// ====== TuShare 能力探测（启动一次） ======
//
// 用 app_state KEY_TUSHARE_PROBE_DONE 记标记——已跑过就跳过，避免每次重启都打一遍。
// 删该 key（或第一次跑）即触发一轮探测，结果写 app data dir 下 tushare-probe-result.json。

const KEY_TUSHARE_PROBE_DONE: &str = "gangzi-terminal.tushare-probe-done";

async fn tushare_probe_once(app: AppHandle) {
    // 等其它 scheduler 起来 + stocks/indexes/funds 档案就绪
    tokio::time::sleep(Duration::from_secs(20)).await;

    // 已跑过就跳
    if let Ok(Some(_)) = crate::infrastructure::app_state::load_app_state_value(&app, KEY_TUSHARE_PROBE_DONE) {
        tracing::debug!("TuShare probe 已跑过，跳过");
        return;
    }

    tracing::info!("启动 TuShare 能力探测——大约 60 秒");
    let results = crate::infrastructure::quotes::tushare::probe::run_probe(&app).await;

    // 落盘 JSON
    if let Ok(json) = serde_json::to_string_pretty(&results) {
        if let Ok(dir) = tauri::Manager::path(&app).app_data_dir() {
            let path = dir.join("tushare-probe-result.json");
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!(path = %path.display(), err = %e, "写 probe 结果失败");
            } else {
                tracing::info!(path = %path.display(), "probe 结果已落盘");
                // 记标记
                let _ = crate::infrastructure::app_state::save_app_state_value(
                    &app,
                    KEY_TUSHARE_PROBE_DONE,
                    &serde_json::json!({
                        "ranAt": chrono::Utc::now().to_rfc3339(),
                        "total": results.len(),
                    }),
                );
            }
        }
    }
}

// ====== 实时行情刷新 ======
//
// 订阅集（Account watchlist + open positions）和核心指数高频刷新。频率：
// - 盘中（A 股 9:30-11:30 + 13:00-15:00）：15s（TDX 主路径，多服务器，无风控压力）
// - 盘后（其它时段）：60s
// - 周末：10min
//
// 实现走 EM ulist.np 分批 + 并发 + 重试。详见 pipeline::market_refresh。

async fn market_quote_loop(app: AppHandle) {
    // 启动延迟 8s——等 stocks_refresh_loop 先把档案表 hydrate（首次启动时）
    tokio::time::sleep(Duration::from_secs(8)).await;

    loop {
        // 先跑一次
        match crate::pipeline::market_refresh::run_market_quote_refresh(&app).await {
            Ok(summary) => tracing::info!(
                total = summary.total,
                success = summary.success,
                "订阅集行情刷新完成"
            ),
            Err(e) => tracing::warn!(error = %e, "订阅集行情刷新失败"),
        }

        // 等下一轮
        let interval = market_quote_interval();
        tracing::debug!(secs = interval.as_secs(), "下一轮订阅集行情刷新等待");
        tokio::time::sleep(interval).await;
    }
}

fn market_quote_interval() -> Duration {
    let beijing = chrono::Utc::now() + chrono::Duration::hours(8);
    use chrono::Datelike;
    let weekday = beijing.weekday();
    let is_weekend = matches!(weekday, chrono::Weekday::Sat | chrono::Weekday::Sun);

    // TDX 主路径接入后频率大幅放宽：16 公共 HQ 服务器分散 + 私有协议，
    // 单 IP 风控基本无效。15s 接近 A 股一档 (1Hz) 极限。
    if is_weekend {
        Duration::from_secs(600) // 周末 10min
    } else if crate::domain::quotes::is_a_share_trading_hours() {
        Duration::from_secs(15) // 盘中 15s
    } else {
        Duration::from_secs(60) // 盘外 1min
    }
}

// ====== 资讯自动刷新 ======

async fn news_refresh_loop(app: AppHandle) {
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut counter = FailureCounter::new("news_refresh");
    if read_bool(&app, KEY_AUTO_REFRESH, true) {
        match pipeline::news::run_news_refresh(app.clone()).await {
            Ok(_) => counter.success(),
            Err(e) => {
                tracing::warn!(error = %e, "首次 news refresh 失败");
                counter.fail(&app).await;
            }
        }
    }

    loop {
        // 资讯不像行情有明确"盘中盘外"概念——但盘外的资讯流量也确实更稀，
        // 让两个 loop 共用 effective_refresh_interval_ms 是合理简化
        let interval_ms = effective_refresh_interval_ms(&app);
        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        if !read_bool(&app, KEY_AUTO_REFRESH, true) {
            continue;
        }
        match pipeline::news::run_news_refresh(app.clone()).await {
            Ok(_) => counter.success(),
            Err(e) => {
                tracing::warn!(error = %e, consecutive = counter.count(), "news refresh 失败");
                counter.fail(&app).await;
            }
        }
    }
}

// ====== 失败熔断器 ======

/// 累计连续失败——达阈值后强制 sleep 一段冷静期，防止 Eastmoney/NewsNow 长时间
/// 挂时每 N 秒喊一次错。重启策略：成功一次即重置。
///
/// 这个不是"分布式 circuit breaker"——只是单进程内 best effort 的退避。
struct FailureCounter {
    name: &'static str,
    consecutive: AtomicU32,
}

impl FailureCounter {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            consecutive: AtomicU32::new(0),
        }
    }

    fn count(&self) -> u32 {
        self.consecutive.load(Ordering::SeqCst)
    }

    fn success(&mut self) {
        let prev = self.consecutive.swap(0, Ordering::SeqCst);
        if prev > 0 {
            tracing::info!(name = self.name, recovered_after = prev, "loop 恢复正常");
        }
    }

    /// 累计 +1。达阈值时 emit 状态 + sleep 一段冷静期；调用者下一 tick 自然进退避。
    /// 阈值 5 次（≈ 2.5 分钟连续失败）→ 冷静期 5 分钟。
    /// 阈值 20 次（≈ 10 分钟连续失败）→ 冷静期 20 分钟（封顶）。
    async fn fail(&mut self, app: &AppHandle) {
        let n = self.consecutive.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 5 || n == 10 || n == 20 {
            // 每达一个里程碑发一次 status，UI 可见
            crate::pipeline::emit_status(
                app,
                "loop-degraded",
                &format!(
                    "{} 连续失败 {n} 次，正在退避——可能远端服务异常或网络故障",
                    self.name
                ),
            );
        }
        // 冷静期：5 次后睡 30s，10 次后睡 2 分钟，20 次后睡 5 分钟（封顶）。
        // 不在所有失败都睡——前几次是常规 transient，让 loop 自己的 ticker 节流就行。
        let cooldown = match n {
            0..=4 => return,
            5..=9 => Duration::from_secs(30),
            10..=19 => Duration::from_secs(120),
            _ => Duration::from_secs(300),
        };
        tracing::warn!(
            name = self.name,
            consecutive = n,
            cooldown_secs = cooldown.as_secs(),
            "进入熔断退避"
        );
        tokio::time::sleep(cooldown).await;
    }
}

// ====== 全市场股票档案 ======
//
// 每天 08:30 北京时间拉一次 Eastmoney clist 全市场 5800+ 票，写入 stocks 表。
// 启动时若表空也立刻拉一次（首次安装兜底）。失败不影响其它流水线，只日志告警。

/// 北京时间下一个 08:30 距现在的时长。如果当前已过 08:30，算明天的。
fn duration_until_next_8_30_beijing() -> Duration {
    let beijing = chrono::Utc::now() + chrono::Duration::hours(8);
    let today_830 = beijing
        .date_naive()
        .and_hms_opt(8, 30, 0)
        .expect("hardcoded 8:30 always valid");
    let target = if beijing.naive_utc() < today_830 {
        today_830
    } else {
        today_830 + chrono::Duration::days(1)
    };
    let delta = target - beijing.naive_utc();
    Duration::from_secs(delta.num_seconds().max(60) as u64)
}

async fn stocks_refresh_loop(app: AppHandle) {
    // 启动延迟一点点，避免和其它 loop 启动时同时挤一波请求
    tokio::time::sleep(Duration::from_secs(3)).await;

    // 1. 冷启动：三张表（stocks / indexes / funds）任一为空就立刻拉一次全套
    let stocks_n = crate::infrastructure::quotes::repository::count_stocks(&app).unwrap_or(0);
    let indexes_n = crate::infrastructure::quotes::repository::count_indexes(&app).unwrap_or(0);
    let funds_n = crate::infrastructure::quotes::repository::count_funds(&app).unwrap_or(0);
    if stocks_n == 0 || indexes_n == 0 || funds_n == 0 {
        tracing::info!(
            stocks = stocks_n,
            indexes = indexes_n,
            funds = funds_n,
            "全市场档案有缺，启动时拉取..."
        );
        refresh_universe_once(&app).await;
    } else {
        tracing::info!(
            stocks = stocks_n,
            indexes = indexes_n,
            funds = funds_n,
            "全市场档案完整，跳过启动拉取"
        );
    }

    // 2. 每天 08:30 北京时间盘前刷新——三表同时刷
    loop {
        let wait = duration_until_next_8_30_beijing();
        tracing::info!(secs = wait.as_secs(), "下一次全市场档案刷新等待");
        tokio::time::sleep(wait).await;
        refresh_universe_once(&app).await;
    }
}

async fn refresh_universe_once(app: &AppHandle) {
    let (s, i, f) = crate::pipeline::stocks::refresh_universe(app).await;
    tracing::info!(stocks = s, indexes = i, funds = f, "全市场档案刷新完成");
}

/// 根据用户设置 + 当前时段返回有效刷新间隔。盘外乘以 OFF_HOURS_INTERVAL_MULTIPLIER
/// 让 Eastmoney 不被无意义刷爆。交易时段判定见 `quotes::is_a_share_trading_hours`。
fn effective_refresh_interval_ms(app: &AppHandle) -> u64 {
    let user_pref = read_u64(app, KEY_REFRESH_INTERVAL, DEFAULT_REFRESH_INTERVAL_MS);
    let base = user_pref.max(15_000);
    if crate::domain::quotes::is_a_share_trading_hours() {
        base
    } else {
        base.saturating_mul(OFF_HOURS_INTERVAL_MULTIPLIER)
    }
}

// ====== 设置读取 helpers ======

fn read_bool(app: &AppHandle, key: &str, default: bool) -> bool {
    crate::infrastructure::app_state::load_app_state_value(app, key)
        .ok()
        .flatten()
        .and_then(|v| v.as_bool())
        .unwrap_or(default)
}


fn read_u64(app: &AppHandle, key: &str, default: u64) -> u64 {
    crate::infrastructure::app_state::load_app_state_value(app, key)
        .ok()
        .flatten()
        .and_then(|v: Value| v.as_u64().or_else(|| v.as_i64().map(|n| n.max(0) as u64)))
        .unwrap_or(default)
}
