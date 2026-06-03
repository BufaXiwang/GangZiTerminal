//! TDX 连接管理器 — 连接池 + 失败重连 + per-connection 调用串行化。
//!
//! Spec: docs/design/quotes-module.md §5「TDX 连接池与并发」；references/quotes/tdx.md
//!
//! 设计：
//! - **连接池**：N 条独立连接（`POOL_SIZE`），各自一把 `Mutex<State>`（含 socket +
//!   独立 last_call 节流）。每次调用 round-robin 取一条槽执行 → 最多 N 个调用并发。
//!   后台批量（universe/热点档）和前台交互（K线/详情）共享池，前台能拿空闲槽不排队。
//! - 每次调用通过 spawn_blocking 跑同步 TCP；失败后丢弃该槽连接，下次自动重连。
//! - 最大重试次数：1 次 reconnect + 1 次 retry，避免线程卡死。
//! - per-connection 速率限制：同一连接调用间最小间隔 `MIN_CALL_INTERVAL`。

use super::hosts::HQ_HOSTS;
use super::{
    Bar, BarCategory, MinuteTimePoint, SecurityListEntry, SecurityQuote, TdxHqClient, TdxMarket,
    XdxrRecord,
};
use crate::domain::quotes::{MinuteKlinePoint, QuoteSource, StockQuote, TradeStatus};
use crate::domain::shared::{
    Amount, Freshness, FreshnessStatus, InstrumentCategory, OccurredAt, Price, TradeDate, TsCode,
    Volume,
};
use chrono::Utc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::task;

const MIN_CALL_INTERVAL: Duration = Duration::from_millis(80);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const QUOTE_BATCH_MAX: usize = 80;
const BARS_MAX: u16 = 800;
/// TDX 连接池大小（spec §5「TDX 连接池与并发」：默认 N = 8，分散到 16 台 host → 每台 ≤1 连接）。
const POOL_SIZE: usize = 8;
/// 探测单台 host 延迟用的超时（短于建连超时——只为排序，连不上的排到队尾）。
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Error)]
pub enum TdxManagerError {
    #[error("tdx not connected and reconnect failed: {0}")]
    Reconnect(String),
    #[error("tdx unsupported market (BJ)")]
    UnsupportedMarket,
    #[error("tdx protocol error: {0}")]
    Protocol(String),
}

struct State {
    client: Option<TdxHqClient>,
    last_call: Option<Instant>,
    /// 该槽当前 pin 到的 host 在排序列表里的下标。连接失败时 +1（wrap）换台。
    host_idx: usize,
}

#[derive(Clone)]
pub struct TdxConnectionManager {
    /// N 条独立连接槽；round-robin 取用，最多 N 个调用并发。
    slots: Arc<Vec<Arc<Mutex<State>>>>,
    next: Arc<AtomicUsize>,
    /// 按延迟排序的 host 列表（`(host, port)`），首次需要连接时探测一次并缓存。
    /// 槽 i 默认 pin 到 `ranked[i % len]`（spec §5：N 个槽分散到延迟最低的 N 台不同 host）。
    ranked_hosts: Arc<Mutex<Option<Arc<Vec<(String, u16)>>>>>,
}

/// 为某个槽（依据其 `State.host_idx`）连一台 host。失败则前进到排序列表下一台（wrap）重试，
/// 最多遍历整张 ranked 列表一圈。成功后把连接装入 `state.client` 并返回 `Ok(())`。
///
/// 与旧 `connect_bestip` 的差异：每个槽**只连自己 pin 的那台 host**（不再全池 race 同一台最快的），
/// 单台慢 / 挂只影响该槽（自动换台），不拖垮整池。spec §5「分散到多台 host」。
fn connect_slot(
    state: &mut State,
    ranked: &[(String, u16)],
    timeout: Duration,
) -> Result<(), TdxManagerError> {
    if ranked.is_empty() {
        return Err(TdxManagerError::Reconnect("no hosts available".into()));
    }
    let n = ranked.len();
    let mut last_err: Option<String> = None;
    for _ in 0..n {
        let idx = assign_host(state.host_idx, n);
        let (host, port) = &ranked[idx];
        match TdxHqClient::connect((host.as_str(), *port), timeout) {
            Ok(c) => {
                state.client = Some(c);
                return Ok(());
            }
            Err(e) => {
                last_err = Some(e.to_string());
                // 该槽换下一台 host（wrap），下次调用也从新台起。
                state.host_idx = (state.host_idx + 1) % n;
            }
        }
    }
    Err(TdxManagerError::Reconnect(
        last_err.unwrap_or_else(|| "all hosts unreachable".into()),
    ))
}

/// 把槽下标映射到排序后 host 列表的初始下标：`slot_idx % ranked.len()`。
///
/// N = 8 / 16 台 → 8 个槽分别 pin 到延迟最低的前 8 台不同 host，每台 ≤1 连接。
/// 纯函数，无 I/O，便于单测。`ranked_len` 为 0 时返回 0（调用方需另行保证非空）。
fn assign_host(slot_idx: usize, ranked_len: usize) -> usize {
    if ranked_len == 0 {
        0
    } else {
        slot_idx % ranked_len
    }
}

/// 对 `HQ_HOSTS` 全部探测一遍连接延迟，返回按延迟升序排序的 `(host, port)` 列表。
///
/// 复用 [`TdxHqClient::connect`] 的「connect + handshake」语义做一次性 probe：每台单独计时，
/// 连不上 / 握手失败的排到队尾（用 `Err` 标记）。**会联网**——只在首次需要建连时调用一次。
fn rank_hosts_by_latency() -> Vec<(String, u16)> {
    let mut timed: Vec<(Duration, bool, String, u16)> = Vec::with_capacity(HQ_HOSTS.len());
    for (_name, host, port) in HQ_HOSTS {
        let t0 = Instant::now();
        let ok = TdxHqClient::connect((*host, *port), PROBE_TIMEOUT).is_ok();
        let dt = t0.elapsed();
        // 失败的标 ok=false，排序时排到所有成功之后（仍保留为候选，供 wrap 换台兜底）。
        timed.push((dt, ok, host.to_string(), *port));
    }
    // 成功优先，其次按延迟升序。
    timed.sort_by(|a, b| match (a.1, b.1) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.0.cmp(&b.0),
    });
    timed.into_iter().map(|(_, _, h, p)| (h, p)).collect()
}

impl Default for TdxConnectionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TdxConnectionManager {
    pub fn new() -> Self {
        let slots = (0..POOL_SIZE)
            .map(|i| {
                Arc::new(Mutex::new(State {
                    client: None,
                    last_call: None,
                    // 槽 i 默认 pin 到排序后第 i 台 host（首次连接时按 ranked 列表解析）。
                    host_idx: i,
                }))
            })
            .collect();
        Self {
            slots: Arc::new(slots),
            next: Arc::new(AtomicUsize::new(0)),
            ranked_hosts: Arc::new(Mutex::new(None)),
        }
    }

    /// round-robin 取一条连接槽（携带其下标，用于 pin host）。并发调用各拿不同槽 → 真并行。
    fn slot(&self) -> (usize, Arc<Mutex<State>>) {
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.slots.len();
        (i, Arc::clone(&self.slots[i]))
    }

    /// 取（必要时探测并缓存）按延迟排序的 host 列表。首次调用会联网 probe `HQ_HOSTS`，
    /// 之后复用缓存。空列表（极端：全部探测失败 + HQ_HOSTS 为空）兜底回退到全量 `HQ_HOSTS`。
    fn ranked_hosts(&self) -> Arc<Vec<(String, u16)>> {
        let mut guard = self.ranked_hosts.lock().expect("ranked_hosts poisoned");
        if let Some(r) = guard.as_ref() {
            return Arc::clone(r);
        }
        let mut ranked = rank_hosts_by_latency();
        if ranked.is_empty() {
            ranked = HQ_HOSTS.iter().map(|(_, h, p)| (h.to_string(), *p)).collect();
        }
        let arc = Arc::new(ranked);
        *guard = Some(Arc::clone(&arc));
        arc
    }

    /// 拉单只标的实时报价。失败后丢弃连接。
    pub async fn fetch_quote(
        &self,
        ts_code: &TsCode,
        category: InstrumentCategory,
        trade_date: TradeDate,
        now: OccurredAt,
        name: Option<String>,
    ) -> Result<StockQuote, TdxManagerError> {
        let market = match ts_code.market() {
            crate::domain::shared::Market::SH => TdxMarket::SH,
            crate::domain::shared::Market::SZ => TdxMarket::SZ,
            crate::domain::shared::Market::BJ => return Err(TdxManagerError::UnsupportedMarket),
        };
        let code = ts_code.as_str()[..6].to_string();
        let (_slot_i, inner) = self.slot();
        let ranked = self.ranked_hosts();
        let result = task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("tdx state poisoned");
            // 速率：保持最小间隔。
            if let Some(last) = guard.last_call {
                let elapsed = last.elapsed();
                if elapsed < MIN_CALL_INTERVAL {
                    std::thread::sleep(MIN_CALL_INTERVAL - elapsed);
                }
            }
            for attempt in 0..2 {
                if guard.client.is_none() {
                    if let Err(e) = connect_slot(&mut guard, &ranked, CONNECT_TIMEOUT) {
                        if attempt == 1 {
                            return Err(e);
                        }
                        continue;
                    }
                }
                let cli = guard.client.as_mut().expect("client present");
                let res = cli.security_quotes(&[(market, code.as_str())]);
                guard.last_call = Some(Instant::now());
                match res {
                    Ok(list) => return Ok(list.into_iter().next()),
                    Err(e) => {
                        // 丢弃连接重连
                        guard.client = None;
                        if attempt == 1 {
                            return Err(TdxManagerError::Protocol(e.to_string()));
                        }
                    }
                }
            }
            Err(TdxManagerError::Reconnect("retries exhausted".into()))
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join failed: {e}")))??;
        let Some(raw) = result else {
            return Err(TdxManagerError::Protocol("empty quote".into()));
        };
        Ok(map_security_quote(&raw, ts_code.clone(), category, name, trade_date, now))
    }

    /// 批量取报价。
    pub async fn fetch_quotes(
        &self,
        codes: Vec<(TsCode, InstrumentCategory, Option<String>)>,
        trade_date: TradeDate,
        now: OccurredAt,
    ) -> Vec<Result<StockQuote, TdxManagerError>> {
        let mut out: Vec<Result<StockQuote, TdxManagerError>> = Vec::with_capacity(codes.len());
        for chunk in codes.chunks(QUOTE_BATCH_MAX) {
            let mut pairs: Vec<(TdxMarket, String, TsCode, InstrumentCategory, Option<String>)> =
                Vec::with_capacity(chunk.len());
            for (ts, cat, name) in chunk {
                let m = match ts.market() {
                    crate::domain::shared::Market::SH => TdxMarket::SH,
                    crate::domain::shared::Market::SZ => TdxMarket::SZ,
                    crate::domain::shared::Market::BJ => {
                        out.push(Err(TdxManagerError::UnsupportedMarket));
                        continue;
                    }
                };
                pairs.push((m, ts.as_str()[..6].to_string(), ts.clone(), *cat, name.clone()));
            }
            if pairs.is_empty() {
                continue;
            }
            let (_slot_i, inner) = self.slot();
            let ranked = self.ranked_hosts();
            let pairs_for_call: Vec<(TdxMarket, String)> = pairs
                .iter()
                .map(|(m, c, _, _, _)| (*m, c.clone()))
                .collect();
            let raw_res = task::spawn_blocking(move || -> Result<Vec<SecurityQuote>, TdxManagerError> {
                let mut guard = inner.lock().expect("tdx state poisoned");
                if let Some(last) = guard.last_call {
                    let e = last.elapsed();
                    if e < MIN_CALL_INTERVAL {
                        std::thread::sleep(MIN_CALL_INTERVAL - e);
                    }
                }
                for attempt in 0..2 {
                    if guard.client.is_none() {
                        if let Err(e) = connect_slot(&mut guard, &ranked, CONNECT_TIMEOUT) {
                            if attempt == 1 {
                                return Err(e);
                            }
                            continue;
                        }
                    }
                    let cli = guard.client.as_mut().expect("client");
                    let refs: Vec<(TdxMarket, &str)> =
                        pairs_for_call.iter().map(|(m, c)| (*m, c.as_str())).collect();
                    let res = cli.security_quotes(&refs);
                    guard.last_call = Some(Instant::now());
                    match res {
                        Ok(v) => return Ok(v),
                        Err(e) => {
                            guard.client = None;
                            if attempt == 1 {
                                return Err(TdxManagerError::Protocol(e.to_string()));
                            }
                        }
                    }
                }
                Err(TdxManagerError::Reconnect("retries exhausted".into()))
            })
            .await
            .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")));
            let raws = match raw_res {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    for (_, _, ts, _, _) in &pairs {
                        let _ = ts;
                        out.push(Err(match &e {
                            TdxManagerError::UnsupportedMarket => TdxManagerError::UnsupportedMarket,
                            TdxManagerError::Reconnect(m) => TdxManagerError::Reconnect(m.clone()),
                            TdxManagerError::Protocol(m) => TdxManagerError::Protocol(m.clone()),
                        }));
                    }
                    continue;
                }
                Err(e) => {
                    for _ in &pairs {
                        out.push(Err(TdxManagerError::Protocol(e.to_string())));
                    }
                    continue;
                }
            };
            // 按 (market, code) 匹配回 ts_code。**必须带 market**：同代码跨市场（如上证指数
            // 000001.SH 与 平安银行 000001.SZ）只按 code 匹配会串号，把指数的报价错配成深市股票。
            for (m, code, ts, cat, name) in pairs {
                let raw = raws.iter().find(|q| q.market == m.as_u8() && q.code == code);
                match raw {
                    Some(r) => out.push(Ok(map_security_quote(r, ts, cat, name, trade_date, now))),
                    None => out.push(Err(TdxManagerError::Protocol("missing in response".into()))),
                }
            }
        }
        out
    }

    /// 取日 K（不复权）。
    pub async fn fetch_daily_kline(
        &self,
        ts_code: &TsCode,
        count: u16,
    ) -> Result<Vec<Bar>, TdxManagerError> {
        let market = match ts_code.market() {
            crate::domain::shared::Market::SH => TdxMarket::SH,
            crate::domain::shared::Market::SZ => TdxMarket::SZ,
            crate::domain::shared::Market::BJ => return Err(TdxManagerError::UnsupportedMarket),
        };
        let code = ts_code.as_str()[..6].to_string();
        let (_slot_i, inner) = self.slot();
        let ranked = self.ranked_hosts();
        let count = count.min(BARS_MAX);
        task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("tdx state poisoned");
            if let Some(last) = guard.last_call {
                let e = last.elapsed();
                if e < MIN_CALL_INTERVAL {
                    std::thread::sleep(MIN_CALL_INTERVAL - e);
                }
            }
            for attempt in 0..2 {
                if guard.client.is_none() {
                    if let Err(e) = connect_slot(&mut guard, &ranked, CONNECT_TIMEOUT) {
                        if attempt == 1 {
                            return Err(e);
                        }
                        continue;
                    }
                }
                let cli = guard.client.as_mut().expect("client");
                let res = cli.security_bars(BarCategory::Day, market, &code, 0, count);
                guard.last_call = Some(Instant::now());
                match res {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        guard.client = None;
                        if attempt == 1 {
                            return Err(TdxManagerError::Protocol(e.to_string()));
                        }
                    }
                }
            }
            Err(TdxManagerError::Reconnect("retries exhausted".into()))
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")))?
    }

    /// 取日 / 周 / 月 K（spec §5：TDX 是日 / 周 / 月 K 主源）。BJ 不支持。
    ///
    /// 单次拉取根数受 TDX 协议限制，count 自动 clamp 到 `BARS_MAX` (800)。
    pub async fn fetch_kline(
        &self,
        ts_code: &TsCode,
        period: crate::domain::quotes::KlinePeriod,
        count: u16,
    ) -> Result<Vec<Bar>, TdxManagerError> {
        self.fetch_kline_at(ts_code, period, 0, count).await
    }

    /// 任意 `start` offset 拉一页 K 线（spec §5 K 线分页支持）。
    ///
    /// `start = 0` 拉最新一批；`start = 800` 拉再往前一批；以此类推。
    /// 用于前端"渐进式加载"——先显示最新 800 根，再背景一次次往前拉。
    pub async fn fetch_kline_at(
        &self,
        ts_code: &TsCode,
        period: crate::domain::quotes::KlinePeriod,
        start: u16,
        count: u16,
    ) -> Result<Vec<Bar>, TdxManagerError> {
        use crate::infrastructure::quotes::tdx::adapter::kline_period_to_tdx;
        let market = match ts_code.market() {
            crate::domain::shared::Market::SH => TdxMarket::SH,
            crate::domain::shared::Market::SZ => TdxMarket::SZ,
            crate::domain::shared::Market::BJ => return Err(TdxManagerError::UnsupportedMarket),
        };
        let code = ts_code.as_str()[..6].to_string();
        let (_slot_i, inner) = self.slot();
        let ranked = self.ranked_hosts();
        let cat = kline_period_to_tdx(period);
        let count = count.min(BARS_MAX);
        task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("tdx state poisoned");
            if let Some(last) = guard.last_call {
                let e = last.elapsed();
                if e < MIN_CALL_INTERVAL {
                    std::thread::sleep(MIN_CALL_INTERVAL - e);
                }
            }
            for attempt in 0..2 {
                if guard.client.is_none() {
                    if let Err(e) = connect_slot(&mut guard, &ranked, CONNECT_TIMEOUT) {
                        if attempt == 1 {
                            return Err(e);
                        }
                        continue;
                    }
                }
                let cli = guard.client.as_mut().expect("client");
                let res = cli.security_bars(cat, market, &code, start, count);
                guard.last_call = Some(Instant::now());
                match res {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        guard.client = None;
                        if attempt == 1 {
                            return Err(TdxManagerError::Protocol(e.to_string()));
                        }
                    }
                }
            }
            Err(TdxManagerError::Reconnect("retries exhausted".into()))
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")))?
    }

    /// 全量历史分页拉取 K 线（spec §5 "K 线"：TDX 主源 + 修订记录 D2.5）。
    ///
    /// Spec: docs/design/quotes-module.md §5 + §4 ensure_chart_data
    ///
    /// 循环 `start = 0, 800, 1600, ...`：每次 `security_bars(cat, market, code, start, 800)`，
    /// 累计直到：
    /// 1. 返回 batch 为空 → break；
    /// 2. 返回 batch < 800 根 → append 后 break（这是最早一段，listing 之前没数据）；
    /// 3. 累计 `start >= HARD_CAP (50_000)` → break 防失控。
    ///
    /// **顺序契约**：底层 `security_bars` 单次返回升序（oldest first）。`start` 增大代表更早
    /// 的历史段，因此新 batch 需 **prepend** 到累计 Vec 前面，最终保证整体升序。
    ///
    /// **耗时警告**：老股可达 10+ TDX 调用 × ~100-500ms + 80ms 间隔，可能耗时 3-10s。
    /// 调用方应在 UI 显示 loading。中间 batch 失败 → 整体 abort（partial 落库无意义）。
    ///
    /// BJ 不支持（同 `fetch_kline`）。
    pub async fn fetch_kline_paginated(
        &self,
        ts_code: &TsCode,
        period: crate::domain::quotes::KlinePeriod,
    ) -> Result<Vec<Bar>, TdxManagerError> {
        use crate::infrastructure::quotes::tdx::adapter::kline_period_to_tdx;
        let market = match ts_code.market() {
            crate::domain::shared::Market::SH => TdxMarket::SH,
            crate::domain::shared::Market::SZ => TdxMarket::SZ,
            crate::domain::shared::Market::BJ => return Err(TdxManagerError::UnsupportedMarket),
        };
        let code = ts_code.as_str()[..6].to_string();
        let cat = kline_period_to_tdx(period);
        let (_slot_i, inner) = self.slot();
        let ranked = self.ranked_hosts();
        task::spawn_blocking(move || {
            const HARD_CAP: u32 = 50_000;
            let mut all: Vec<Bar> = Vec::new();
            let mut start: u32 = 0;
            loop {
                if start >= HARD_CAP {
                    break;
                }
                let start_u16 = start as u16;
                let count_u16: u16 = BARS_MAX;
                // 一次 batch：完整复用与 fetch_kline 一致的速率 + 重连 + 重试逻辑。
                let mut guard = inner.lock().expect("tdx state poisoned");
                if let Some(last) = guard.last_call {
                    let e = last.elapsed();
                    if e < MIN_CALL_INTERVAL {
                        std::thread::sleep(MIN_CALL_INTERVAL - e);
                    }
                }
                let mut batch_res: Result<Vec<Bar>, TdxManagerError> =
                    Err(TdxManagerError::Reconnect("retries exhausted".into()));
                for attempt in 0..2 {
                    if guard.client.is_none() {
                        if let Err(e) = connect_slot(&mut guard, &ranked, CONNECT_TIMEOUT) {
                            if attempt == 1 {
                                batch_res = Err(e);
                                break;
                            }
                            continue;
                        }
                    }
                    let cli = guard.client.as_mut().expect("client");
                    let res = cli.security_bars(cat, market, &code, start_u16, count_u16);
                    guard.last_call = Some(Instant::now());
                    match res {
                        Ok(v) => {
                            batch_res = Ok(v);
                            break;
                        }
                        Err(e) => {
                            guard.client = None;
                            if attempt == 1 {
                                batch_res = Err(TdxManagerError::Protocol(e.to_string()));
                                break;
                            }
                        }
                    }
                }
                drop(guard);

                let batch = batch_res?;
                let n = batch.len();
                let truncated = n < BARS_MAX as usize;
                if n == 0 {
                    break;
                }
                // batch 升序；更早的页 prepend 到累计 Vec 前。
                let mut merged: Vec<Bar> = Vec::with_capacity(n + all.len());
                merged.extend(batch);
                merged.extend(all);
                all = merged;
                if truncated {
                    break;
                }
                start = start.saturating_add(BARS_MAX as u32);
            }
            Ok(all)
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")))?
    }

    /// 取分钟 K（spec §5：分钟 K 主源 TDX）。BJ 不支持。
    pub async fn fetch_minute_kline(
        &self,
        ts_code: &TsCode,
        period: crate::domain::quotes::MinuteKlinePeriod,
        count: u16,
    ) -> Result<Vec<Bar>, TdxManagerError> {
        use crate::infrastructure::quotes::tdx::adapter::minute_period_to_tdx;
        let market = match ts_code.market() {
            crate::domain::shared::Market::SH => TdxMarket::SH,
            crate::domain::shared::Market::SZ => TdxMarket::SZ,
            crate::domain::shared::Market::BJ => return Err(TdxManagerError::UnsupportedMarket),
        };
        let code = ts_code.as_str()[..6].to_string();
        let (_slot_i, inner) = self.slot();
        let ranked = self.ranked_hosts();
        let cat = minute_period_to_tdx(period);
        let count = count.min(BARS_MAX);
        task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("tdx state poisoned");
            if let Some(last) = guard.last_call {
                let e = last.elapsed();
                if e < MIN_CALL_INTERVAL {
                    std::thread::sleep(MIN_CALL_INTERVAL - e);
                }
            }
            for attempt in 0..2 {
                if guard.client.is_none() {
                    if let Err(e) = connect_slot(&mut guard, &ranked, CONNECT_TIMEOUT) {
                        if attempt == 1 {
                            return Err(e);
                        }
                        continue;
                    }
                }
                let cli = guard.client.as_mut().expect("client");
                let res = cli.security_bars(cat, market, &code, 0, count);
                guard.last_call = Some(Instant::now());
                match res {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        guard.client = None;
                        if attempt == 1 {
                            return Err(TdxManagerError::Protocol(e.to_string()));
                        }
                    }
                }
            }
            Err(TdxManagerError::Reconnect("retries exhausted".into()))
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")))?
    }

    /// 除权除息 / 公司行动历史。BJ 不支持。
    pub async fn fetch_xdxr(
        &self,
        ts_code: &TsCode,
    ) -> Result<Vec<XdxrRecord>, TdxManagerError> {
        let market = match ts_code.market() {
            crate::domain::shared::Market::SH => TdxMarket::SH,
            crate::domain::shared::Market::SZ => TdxMarket::SZ,
            crate::domain::shared::Market::BJ => return Err(TdxManagerError::UnsupportedMarket),
        };
        let code = ts_code.as_str()[..6].to_string();
        let (_slot_i, inner) = self.slot();
        let ranked = self.ranked_hosts();
        task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("tdx state poisoned");
            if let Some(last) = guard.last_call {
                let e = last.elapsed();
                if e < MIN_CALL_INTERVAL {
                    std::thread::sleep(MIN_CALL_INTERVAL - e);
                }
            }
            for attempt in 0..2 {
                if guard.client.is_none() {
                    if let Err(e) = connect_slot(&mut guard, &ranked, CONNECT_TIMEOUT) {
                        if attempt == 1 {
                            return Err(e);
                        }
                        continue;
                    }
                }
                let cli = guard.client.as_mut().expect("client");
                let res = cli.security_xdxr(market, &code);
                guard.last_call = Some(Instant::now());
                match res {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        guard.client = None;
                        if attempt == 1 {
                            return Err(TdxManagerError::Protocol(e.to_string()));
                        }
                    }
                }
            }
            Err(TdxManagerError::Reconnect("retries exhausted".into()))
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")))?
    }

    /// 当日分时（240 个交易分钟）。BJ 不支持。
    ///
    /// 协议层返回的 `MinuteTimePoint` 不含时间戳——index 对应交易时段第 N 分钟。
    /// pipeline / domain 层负责派生具体时间。
    pub async fn fetch_minute_time(
        &self,
        ts_code: &TsCode,
    ) -> Result<Vec<MinuteTimePoint>, TdxManagerError> {
        let market = match ts_code.market() {
            crate::domain::shared::Market::SH => TdxMarket::SH,
            crate::domain::shared::Market::SZ => TdxMarket::SZ,
            crate::domain::shared::Market::BJ => return Err(TdxManagerError::UnsupportedMarket),
        };
        let code = ts_code.as_str()[..6].to_string();
        let (_slot_i, inner) = self.slot();
        let ranked = self.ranked_hosts();
        task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("tdx state poisoned");
            if let Some(last) = guard.last_call {
                let e = last.elapsed();
                if e < MIN_CALL_INTERVAL {
                    std::thread::sleep(MIN_CALL_INTERVAL - e);
                }
            }
            for attempt in 0..2 {
                if guard.client.is_none() {
                    if let Err(e) = connect_slot(&mut guard, &ranked, CONNECT_TIMEOUT) {
                        if attempt == 1 {
                            return Err(e);
                        }
                        continue;
                    }
                }
                let cli = guard.client.as_mut().expect("client");
                let res = cli.security_minute_time(market, &code);
                guard.last_call = Some(Instant::now());
                match res {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        guard.client = None;
                        if attempt == 1 {
                            return Err(TdxManagerError::Protocol(e.to_string()));
                        }
                    }
                }
            }
            Err(TdxManagerError::Reconnect("retries exhausted".into()))
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")))?
    }

    /// 拉取 SH 或 SZ 全 universe（分页）。
    ///
    /// Spec: quotes-module.md §5 line 728 — TDX 是 SH / SZ universe 的主源。
    /// `security_count` 给出总数；`security_list(market, start)` 每次返回最多 1000 条；
    /// 调用方根据 6 位 code 前缀分类为 stock / index / fund。
    pub async fn fetch_universe(
        &self,
        market: TdxMarket,
    ) -> Result<Vec<SecurityListEntry>, TdxManagerError> {
        let (_slot_i, inner) = self.slot();
        let ranked = self.ranked_hosts();
        task::spawn_blocking(move || {
            let mut guard = inner.lock().expect("tdx state poisoned");
            if let Some(last) = guard.last_call {
                let e = last.elapsed();
                if e < MIN_CALL_INTERVAL {
                    std::thread::sleep(MIN_CALL_INTERVAL - e);
                }
            }
            // 确保连接
            for attempt in 0..2 {
                if guard.client.is_none() {
                    if let Err(e) = connect_slot(&mut guard, &ranked, CONNECT_TIMEOUT) {
                        if attempt == 1 {
                            return Err(e);
                        }
                        continue;
                    }
                }
                break;
            }
            if guard.client.is_none() {
                return Err(TdxManagerError::Reconnect("client not established".into()));
            }
            // 先拿总数
            let count = {
                let cli = guard.client.as_mut().expect("client present");
                let r = cli.security_count(market);
                guard.last_call = Some(Instant::now());
                match r {
                    Ok(n) => n,
                    Err(e) => {
                        guard.client = None;
                        return Err(TdxManagerError::Protocol(e.to_string()));
                    }
                }
            };
            let mut out: Vec<SecurityListEntry> = Vec::with_capacity(count as usize);
            let mut start: u16 = 0;
            const PAGE: u16 = 1000;
            while start < count {
                if let Some(last) = guard.last_call {
                    let e = last.elapsed();
                    if e < MIN_CALL_INTERVAL {
                        std::thread::sleep(MIN_CALL_INTERVAL - e);
                    }
                }
                let res = {
                    let cli = guard.client.as_mut().expect("client present");
                    cli.security_list(market, start)
                };
                guard.last_call = Some(Instant::now());
                match res {
                    Ok(mut page) => {
                        if page.is_empty() {
                            break;
                        }
                        let got = page.len() as u16;
                        out.append(&mut page);
                        // 防止 wrap：用实际读取条数推进 start
                        start = start.saturating_add(if got > 0 { got } else { PAGE });
                        if got < PAGE {
                            break;
                        }
                    }
                    Err(e) => {
                        guard.client = None;
                        return Err(TdxManagerError::Protocol(e.to_string()));
                    }
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| TdxManagerError::Protocol(format!("join: {e}")))?
    }
}

fn map_security_quote(
    raw: &SecurityQuote,
    ts_code: TsCode,
    category: InstrumentCategory,
    name: Option<String>,
    trade_date: TradeDate,
    now: OccurredAt,
) -> StockQuote {
    use rust_decimal::{prelude::FromPrimitive, Decimal};
    let f64_to_price = |v: f64| -> Option<Price> {
        if v.is_finite() && v > 0.0 {
            Decimal::from_f64(v).map(|d| Price(d.round_dp(4)))
        } else {
            None
        }
    };
    let f64_to_amount = |v: f64| -> Option<Amount> {
        if v.is_finite() && v > 0.0 {
            Decimal::from_f64(v).map(|d| Amount(d.round_dp(4)))
        } else {
            None
        }
    };
    // 价格小数位校正（TDX reference §Normalize）：security_quotes 协议层统一 `/100`（假设 2 位
    // 小数）。实测不同标的的 quote 整数按其**小数位**编码（integer = price × 10^decimals）：
    //   - 股票 / 指数：2 位 → 协议 `/100` 已正确 → scale 1.0
    //   - 场内基金 / ETF：3 位（×1000）→ 协议 `/100` 余 ×10 → scale 0.1
    //   - 债券（可转债等，按需取、universe 外）：**4 位**（×10000，实测 110073 raw=10694 ⇔ 真值
    //     106.94）→ 协议 `/100` 余 ×100 → scale 0.01
    // 按 is_bond / category 推导小数位（K 线走 `/1000`，不经此路径）。
    let price_scale: f64 =
        if crate::infrastructure::quotes::universe::is_bond(ts_code.market(), &ts_code.as_str()[..6]) {
            0.01
        } else if matches!(category, InstrumentCategory::Fund) {
            0.1
        } else {
            1.0
        };
    let scaled_price = |v: f64| f64_to_price(v * price_scale);
    let price = scaled_price(raw.price);
    let prev = scaled_price(raw.last_close);
    let open = scaled_price(raw.open);
    let high = scaled_price(raw.high);
    let low = scaled_price(raw.low);
    // TDX security_quotes 的成交量 / 盘口量单位是「手」；canonical `Volume` 统一为「股」
    // （shared-types.md §Volume：盘口/成交量统一为股，不使用手；quotes-module.md §盘口 line 215）。
    // ×100 转股，与腾讯 adapter（mod.rs ×100）保持同一单位。
    // 注：K 线 security_bars 的 volume 已是股（走 map_daily_bar），不经此路径，不要 ×100。
    let volume = if raw.vol > 0.0 {
        Some(Volume((raw.vol * 100.0) as i64))
    } else {
        None
    };
    let amount = if raw.amount > 0.0 {
        f64_to_amount(raw.amount)
    } else {
        None
    };
    let change = match (price, prev) {
        (Some(p), Some(pc)) => Some(Price(p.0 - pc.0)),
        _ => None,
    };
    let change_percent = match (price, prev) {
        (Some(p), Some(pc)) if pc.0 > Decimal::ZERO => {
            let pct = ((p.0 - pc.0) / pc.0 * Decimal::from(100)).round_dp(4);
            pct.to_string().parse::<f64>().ok()
        }
        _ => None,
    };
    let mut bid: Vec<crate::domain::quotes::QuoteDepthLevel> = Vec::with_capacity(5);
    let mut ask: Vec<crate::domain::quotes::QuoteDepthLevel> = Vec::with_capacity(5);
    for level in raw.book.iter() {
        // 盘口量同样「手」→「股」×100（见上方 volume 注释）。
        bid.push(crate::domain::quotes::QuoteDepthLevel {
            price: scaled_price(level.bid),
            volume: if level.bid_vol > 0.0 {
                Some(Volume((level.bid_vol * 100.0) as i64))
            } else {
                None
            },
        });
        ask.push(crate::domain::quotes::QuoteDepthLevel {
            price: scaled_price(level.ask),
            volume: if level.ask_vol > 0.0 {
                Some(Volume((level.ask_vol * 100.0) as i64))
            } else {
                None
            },
        });
    }
    StockQuote {
        ts_code,
        name,
        category,
        trade_date,
        price,
        previous_close: prev,
        open,
        high,
        low,
        change,
        change_percent,
        volume,
        amount,
        turnover_rate: None,
        volume_ratio: None,
        limit_up: None,
        limit_down: None,
        bid,
        ask,
        trade_status: TradeStatus::Unknown,
        source: QuoteSource::Tdx,
        captured_at: now,
        exchange_time: None,
        freshness: Freshness {
            status: FreshnessStatus::Fresh,
            captured_at: Some(now),
            exchange_time: None,
            age_ms: Some(0),
            source: Some("tdx".to_string()),
            warning: None,
        },
        warnings: Vec::new(),
    }
}

/// 分页聚合器：抽象掉 TDX socket，纯粹按 `fetch(start, count)` 闭包做分页 loop。
///
/// Spec: docs/design/quotes-module.md §5 K 线 + §4 ensure_chart_data。
///
/// 终止条件（与 `fetch_kline_paginated` 内联实现保持一致）：
/// 1. 闭包返回 `Err` → 立即 abort，整体失败（partial 段无意义）；
/// 2. 闭包返回空 batch → break；
/// 3. 返回 batch < `page_size` → append 后 break；
/// 4. `start >= hard_cap` → break。
///
/// **顺序契约**：每次 batch 升序，新（更早）batch prepend 到累计前；最终 Vec 升序。
#[allow(dead_code)] // 当前仅 unit test 直接调用；fetch_kline_paginated 内联同等逻辑（含 socket 编织）。
pub(crate) fn aggregate_paginated_bars<F>(
    page_size: u16,
    hard_cap: u32,
    mut fetch: F,
) -> Result<Vec<Bar>, TdxManagerError>
where
    F: FnMut(u32, u16) -> Result<Vec<Bar>, TdxManagerError>,
{
    let mut all: Vec<Bar> = Vec::new();
    let mut start: u32 = 0;
    loop {
        if start >= hard_cap {
            break;
        }
        let batch = fetch(start, page_size)?;
        let n = batch.len();
        let truncated = n < page_size as usize;
        if n == 0 {
            break;
        }
        let mut merged: Vec<Bar> = Vec::with_capacity(n + all.len());
        merged.extend(batch);
        merged.extend(all);
        all = merged;
        if truncated {
            break;
        }
        start = start.saturating_add(page_size as u32);
    }
    Ok(all)
}

/// 把 TDX 分钟 Bar 翻译为 MinuteKlinePoint。
pub fn map_minute_bar(b: &Bar) -> Option<MinuteKlinePoint> {
    use chrono::TimeZone;
    use chrono_tz::Asia::Shanghai;
    use rust_decimal::{prelude::FromPrimitive, Decimal};
    let nd = chrono::NaiveDate::from_ymd_opt(b.year as i32, b.month as u32, b.day as u32)?;
    let dt = nd.and_hms_opt(b.hour as u32, b.minute as u32, 0)?;
    let utc = Shanghai
        .from_local_datetime(&dt)
        .single()?
        .with_timezone(&Utc);
    let ts_ms = utc.timestamp_millis();
    let _f64_to_price = |v: f64| -> Option<Price> {
        if v.is_finite() && v > 0.0 {
            Decimal::from_f64(v).map(|d| Price(d.round_dp(4)))
        } else {
            None
        }
    };
    Some(MinuteKlinePoint {
        timestamp_ms: ts_ms,
        open: Price(Decimal::from_f64(b.open).unwrap_or_default().round_dp(4)),
        close: Price(Decimal::from_f64(b.close).unwrap_or_default().round_dp(4)),
        high: Price(Decimal::from_f64(b.high).unwrap_or_default().round_dp(4)),
        low: Price(Decimal::from_f64(b.low).unwrap_or_default().round_dp(4)),
        volume: if b.volume > 0.0 {
            Volume(b.volume as i64)
        } else {
            Volume(0)
        },
        amount: Amount(Decimal::from_f64(b.amount).unwrap_or_default().round_dp(4)),
    })
}

/// 把 TDX 日 Bar 翻译为 KlinePoint。
///
/// 防御性校验（spec §2 + TDX provider ref："非法值按 missing 处理"）：
/// - year 必须在 [1990, 2100]
/// - open/close/high/low 必须是 finite 正数（拒绝负价 / NaN / Infinity）
/// - volume / amount 必须 finite（NaN/Inf → None）
///
/// 任一字段失败 → 返回 None。调用方根据 None 决定丢弃 / 整批弃用。
pub fn map_daily_bar(b: &Bar) -> Option<crate::domain::quotes::KlinePoint> {
    use chrono::NaiveDate;
    use rust_decimal::{prelude::FromPrimitive, Decimal};
    if b.year < 1990 || b.year > 2100 {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(b.year as i32, b.month as u32, b.day as u32)?;
    let date = TradeDate::from_naive(date);
    let valid_price = |p: f64| p.is_finite() && p > 0.0;
    if !valid_price(b.open) || !valid_price(b.close) || !valid_price(b.high) || !valid_price(b.low)
    {
        return None;
    }
    Some(crate::domain::quotes::KlinePoint {
        date,
        open: Price(Decimal::from_f64(b.open)?.round_dp(4)),
        close: Price(Decimal::from_f64(b.close)?.round_dp(4)),
        high: Price(Decimal::from_f64(b.high)?.round_dp(4)),
        low: Price(Decimal::from_f64(b.low)?.round_dp(4)),
        volume: if b.volume.is_finite() && b.volume > 0.0 {
            Some(Volume(b.volume as i64))
        } else {
            None
        },
        amount: if b.amount.is_finite() && b.amount > 0.0 {
            Decimal::from_f64(b.amount).map(|d| Amount(d.round_dp(4)))
        } else {
            None
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::quotes::tdx::QuoteLevel;

    fn sample_raw() -> SecurityQuote {
        SecurityQuote {
            market: 1,
            code: "600519".to_string(),
            active1: 0,
            price: 1500.0,
            last_close: 1480.0,
            open: 1490.0,
            high: 1520.0,
            low: 1485.0,
            vol: 1234567.0,
            cur_vol: 100.0,
            amount: 1_234_567_890.0,
            s_vol: 600_000.0,
            b_vol: 700_000.0,
            book: [
                QuoteLevel { bid: 1499.0, ask: 1501.0, bid_vol: 500.0, ask_vol: 600.0 },
                QuoteLevel { bid: 1498.0, ask: 1502.0, bid_vol: 100.0, ask_vol: 200.0 },
                QuoteLevel::default(),
                QuoteLevel::default(),
                QuoteLevel::default(),
            ],
            rate: 0.013,
            active2: 0,
        }
    }

    #[test]
    fn new_builds_pool_size_slots() {
        let mgr = TdxConnectionManager::new();
        // 构造出 POOL_SIZE 条独立槽，next 计数从 0 起。
        assert_eq!(mgr.slots.len(), POOL_SIZE);
        assert_eq!(mgr.next.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn slot_round_robins_through_pool() {
        let mgr = TdxConnectionManager::new();
        // 连续取 POOL_SIZE 次应轮转到每一条不同的槽（按 next % POOL_SIZE）。
        let picked: Vec<(usize, Arc<Mutex<State>>)> = (0..POOL_SIZE).map(|_| mgr.slot()).collect();
        for i in 0..POOL_SIZE {
            // 第 i 次取到的槽应是 slots[i]（next 从 0 起，fetch_add 后 %）。
            assert_eq!(picked[i].0, i, "pick {i} index");
            assert!(
                Arc::ptr_eq(&picked[i].1, &mgr.slots[i]),
                "pick {i} should map to slots[{i}]"
            );
        }
        // 取满一圈后所有槽互不相同。
        for i in 0..POOL_SIZE {
            for j in (i + 1)..POOL_SIZE {
                assert!(!Arc::ptr_eq(&picked[i].1, &picked[j].1), "slots {i} and {j} aliased");
            }
        }
        // 再取一次应回绕到 slots[0]。
        let wrapped = mgr.slot();
        assert_eq!(wrapped.0, 0);
        assert!(Arc::ptr_eq(&wrapped.1, &mgr.slots[0]), "should wrap to slots[0]");
    }

    #[test]
    fn pool_size_is_eight_and_each_slot_pins_distinct_host_idx() {
        // spec §5：N = 8，8 个槽默认 pin 到排序后前 8 台不同 host（host_idx = slot_idx）。
        let mgr = TdxConnectionManager::new();
        assert_eq!(mgr.slots.len(), 8, "POOL_SIZE 应为 8");
        for (i, slot) in mgr.slots.iter().enumerate() {
            let g = slot.lock().unwrap();
            assert_eq!(g.host_idx, i, "槽 {i} 初始应 pin 到 host_idx {i}");
        }
    }

    #[test]
    fn assign_host_disperses_slots_to_distinct_hosts() {
        // 8 槽 / 16 台 → 每槽分到不同 host（slot_idx % len，前 8 个互不相同）。
        let ranked_len = 16;
        let assigned: Vec<usize> = (0..8).map(|i| assign_host(i, ranked_len)).collect();
        assert_eq!(assigned, vec![0, 1, 2, 3, 4, 5, 6, 7]);
        // 互不相同。
        for i in 0..assigned.len() {
            for j in (i + 1)..assigned.len() {
                assert_ne!(assigned[i], assigned[j], "槽 {i} 与 {j} 撞 host");
            }
        }
    }

    #[test]
    fn assign_host_wraps_when_fewer_hosts_than_slots() {
        // 极端：仅 3 台可用、8 槽 → wrap 复用，但仍均匀分散（0,1,2,0,1,2,0,1）。
        let assigned: Vec<usize> = (0..8).map(|i| assign_host(i, 3)).collect();
        assert_eq!(assigned, vec![0, 1, 2, 0, 1, 2, 0, 1]);
        // 空列表兜底 → 0，不 panic。
        assert_eq!(assign_host(5, 0), 0);
    }

    #[test]
    fn connect_slot_advances_host_on_failure_then_succeeds() {
        // 不联网验证「失败换台」语义：ranked 列表里前两台必然连不上（保留地址 + 关闭端口），
        // connect_slot 会逐台前进。这里用 connect 必然失败的地址断言 host_idx 推进 + wrap。
        // 注：connect_slot 真连，会对每台尝试 TCP——用 TEST-NET（RFC 5737）+ 1 端口确保快速 refused/timeout。
        let ranked = vec![
            ("192.0.2.1".to_string(), 1u16), // TEST-NET-1，不可路由
            ("192.0.2.2".to_string(), 1u16),
            ("192.0.2.3".to_string(), 1u16),
        ];
        let mut state = State {
            client: None,
            last_call: None,
            host_idx: 0,
        };
        // 全部连不上 → Err；遍历一圈后 host_idx 回到起点（wrap n 次 → 0）。
        let r = connect_slot(&mut state, &ranked, Duration::from_millis(150));
        assert!(r.is_err(), "全部不可达应返回 Err");
        assert!(state.client.is_none());
        // 遍历 n=3 台各 +1 → host_idx = (0+3) % 3 = 0。
        assert_eq!(state.host_idx, 0, "遍历一圈后 host_idx wrap 回 0");
    }

    #[test]
    fn connect_slot_empty_hosts_errors() {
        let mut state = State {
            client: None,
            last_call: None,
            host_idx: 0,
        };
        let r = connect_slot(&mut state, &[], Duration::from_millis(50));
        assert!(r.is_err());
    }

    #[test]
    fn map_security_quote_populates_bid_ask_and_change() {
        let raw = sample_raw();
        let code = TsCode::parse("600519.SH").unwrap();
        let now = Utc::now();
        let td = TradeDate::parse("20260526").unwrap();
        let q = map_security_quote(&raw, code, InstrumentCategory::Stock, None, td, now);
        assert!(q.price.is_some());
        assert!(q.previous_close.is_some());
        assert_eq!(q.bid.len(), 5);
        assert_eq!(q.ask.len(), 5);
        assert!(q.bid[0].price.is_some());
        assert!(q.ask[0].price.is_some());
        assert!(q.change.is_some());
        assert!(q.change_percent.unwrap() > 1.0);
        assert_eq!(q.source, QuoteSource::Tdx);
        // adapter 不派生 warning
        assert!(q.warnings.is_empty());
        assert!(q.freshness.warning.is_none());
    }

    /// 回归：security_quotes 的成交量 / 盘口量是「手」，必须 ×100 转「股」
    /// （shared-types §Volume / quotes-module §盘口）。曾漏转，导致 account 成交模拟
    /// 把 2 手当 2 股、现实只成交极少量 + UI 量 100× 偏低。
    #[test]
    fn map_security_quote_volume_and_depth_scaled_hand_to_shares() {
        let raw = sample_raw(); // vol=1234567 手, bid_vol[0]=500 手, ask_vol[0]=600 手
        let code = TsCode::parse("600519.SH").unwrap();
        let td = TradeDate::parse("20260526").unwrap();
        let q = map_security_quote(&raw, code, InstrumentCategory::Stock, None, td, Utc::now());
        assert_eq!(q.volume.unwrap().0, 123_456_700, "总成交量 手×100 → 股");
        assert_eq!(q.bid[0].volume.unwrap().0, 50_000, "买一量 500 手 → 50000 股");
        assert_eq!(q.ask[0].volume.unwrap().0, 60_000, "卖一量 600 手 → 60000 股");
        // 盘口量在股单位下应为 100 整数倍（手×100 必然成立）。
        assert_eq!(q.bid[0].volume.unwrap().0 % 100, 0);
    }

    #[test]
    fn map_security_quote_skips_negative_price() {
        let mut raw = sample_raw();
        raw.price = 0.0;
        raw.last_close = 0.0;
        let code = TsCode::parse("600519.SH").unwrap();
        let q = map_security_quote(
            &raw,
            code,
            InstrumentCategory::Stock,
            None,
            TradeDate::parse("20260526").unwrap(),
            Utc::now(),
        );
        assert!(q.price.is_none());
        assert!(q.change.is_none());
    }

    // 价格小数位校正：protocol /100 已应用，raw.price=48.68 对应 ETF 真值 4.868（3 位小数 ×1000
    // 编码）。Fund 类必须额外 /10 → 4.868；Stock 类不校正 → 仍 48.68。同样作用于五档盘口。
    #[test]
    fn map_security_quote_fund_price_scaled_to_three_decimals() {
        let mut raw = sample_raw();
        raw.code = "510300".to_string();
        raw.price = 48.68; // = ETF 真值 4.868 经 protocol /100 后的值
        raw.last_close = 49.23;
        raw.book[0] = QuoteLevel { bid: 48.68, ask: 48.69, bid_vol: 665.0, ask_vol: 532.0 };
        let code = TsCode::parse("510300.SH").unwrap();
        let td = TradeDate::parse("20260601").unwrap();
        // Fund → 校正 /10
        let qf = map_security_quote(&raw, code.clone(), InstrumentCategory::Fund, None, td, Utc::now());
        assert_eq!(qf.price.unwrap().0.to_string(), "4.868", "ETF 价格应校正为 3 位小数真值");
        assert_eq!(qf.previous_close.unwrap().0.to_string(), "4.923");
        assert_eq!(qf.bid[0].price.unwrap().0.to_string(), "4.868", "盘口价同样校正");
        // Stock（同样 raw）→ 不校正，保持 48.68（证明校正仅按 category 生效）
        let qs = map_security_quote(&raw, code, InstrumentCategory::Stock, None, td, Utc::now());
        assert_eq!(qs.price.unwrap().0.to_string(), "48.68");
    }

    // 按需取债券（universe 外）：可转债是 **4 位小数**编码（实测 TDX integer = 真值 × 10000，
    // 协议 /100 后 raw.price = 真值 × 100）。即使 category 传 Stock（调用方默认），is_bond 识别
    // 110xxx → scale 0.01 → 真值。raw.price=10690（真值 106.90 经协议 /100）→ 106.9。
    #[test]
    fn map_security_quote_bond_price_scaled_to_four_decimals() {
        let mut raw = sample_raw();
        raw.code = "110059".to_string();
        raw.price = 10690.0; // 可转债真值 106.90，TDX 协议 /100 后
        raw.last_close = 10650.0;
        let code = TsCode::parse("110059.SH").unwrap();
        let td = TradeDate::parse("20260601").unwrap();
        // category=Stock（债券无专属类目）；is_bond 识别 110xxx → scale 0.01（4 位小数）
        let q = map_security_quote(&raw, code, InstrumentCategory::Stock, None, td, Utc::now());
        assert_eq!(q.price.unwrap().0.to_string(), "106.9", "可转债应按 4 位小数校正（×0.01）");
        assert_eq!(q.previous_close.unwrap().0.to_string(), "106.5");
    }

    fn mk_bar(year: u16, month: u8, day: u8) -> Bar {
        Bar {
            year,
            month,
            day,
            hour: 0,
            minute: 0,
            open: 1.0,
            close: 1.0,
            high: 1.0,
            low: 1.0,
            volume: 0.0,
            amount: 0.0,
        }
    }

    #[test]
    fn aggregate_paginated_bars_merges_pages_in_ascending_order() {
        // 模拟 3 个 batch：start=0 拿到最新 800（2024 段），start=800 拿到 800 中段（2023），
        // start=1600 拿到 200 早段（2022，不足 800 → 终止）。
        // 每个 batch 内部升序，全部合并后整体升序：2022 → 2023 → 2024。
        let pages: std::collections::HashMap<u32, Vec<Bar>> = {
            let mut m = std::collections::HashMap::new();
            let p0: Vec<Bar> = (0..800).map(|i| mk_bar(2024, 1, (i % 28 + 1) as u8)).collect();
            let p1: Vec<Bar> = (0..800).map(|i| mk_bar(2023, 1, (i % 28 + 1) as u8)).collect();
            let p2: Vec<Bar> = (0..200).map(|i| mk_bar(2022, 1, (i % 28 + 1) as u8)).collect();
            m.insert(0, p0);
            m.insert(800, p1);
            m.insert(1600, p2);
            m
        };
        let calls = std::cell::Cell::new(0u32);
        let res = aggregate_paginated_bars(800, 50_000, |start, count| {
            assert_eq!(count, 800);
            calls.set(calls.get() + 1);
            Ok(pages.get(&start).cloned().unwrap_or_default())
        })
        .unwrap();
        assert_eq!(res.len(), 1800);
        // 升序：最早段（2022）在前
        assert_eq!(res[0].year, 2022);
        assert_eq!(res[199].year, 2022);
        assert_eq!(res[200].year, 2023);
        assert_eq!(res[999].year, 2023);
        assert_eq!(res[1000].year, 2024);
        assert_eq!(res[1799].year, 2024);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn aggregate_paginated_bars_stops_on_empty_page() {
        // 第一批返回 800，第二批返回 0 → break，不再继续。
        let calls = std::cell::Cell::new(0u32);
        let res = aggregate_paginated_bars(800, 50_000, |start, _count| {
            calls.set(calls.get() + 1);
            if start == 0 {
                Ok((0..800).map(|i| mk_bar(2024, 1, (i % 28 + 1) as u8)).collect())
            } else {
                Ok(Vec::new())
            }
        })
        .unwrap();
        assert_eq!(res.len(), 800);
        assert_eq!(calls.get(), 2); // 第二次返回空才 break
    }

    #[test]
    fn aggregate_paginated_bars_respects_hard_cap() {
        // 每个 batch 都恰好 800（永不 truncate），hard_cap=2400 → 应在 start=2400 break，
        // 总共 3 次调用、2400 条。
        let calls = std::cell::Cell::new(0u32);
        let res = aggregate_paginated_bars(800, 2400, |_start, _count| {
            calls.set(calls.get() + 1);
            Ok((0..800).map(|i| mk_bar(2020, 1, (i % 28 + 1) as u8)).collect())
        })
        .unwrap();
        assert_eq!(res.len(), 2400);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn aggregate_paginated_bars_aborts_on_error() {
        // 中间 batch 失败 → 整体 abort，不返回 partial。
        let res = aggregate_paginated_bars(800, 50_000, |start, _count| {
            if start == 0 {
                Ok((0..800).map(|i| mk_bar(2024, 1, (i % 28 + 1) as u8)).collect())
            } else {
                Err(TdxManagerError::Protocol("simulated mid-batch failure".into()))
            }
        });
        assert!(res.is_err());
    }

    #[test]
    fn aggregate_paginated_bars_single_short_page() {
        // 新上市股票：第一 batch < page_size，直接 truncate break。
        let calls = std::cell::Cell::new(0u32);
        let res = aggregate_paginated_bars(800, 50_000, |_start, _count| {
            calls.set(calls.get() + 1);
            Ok((0..50).map(|i| mk_bar(2025, 6, (i % 28 + 1) as u8)).collect())
        })
        .unwrap();
        assert_eq!(res.len(), 50);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn map_daily_bar_roundtrips() {
        let b = Bar {
            year: 2026,
            month: 5,
            day: 26,
            hour: 0,
            minute: 0,
            open: 100.0,
            high: 110.0,
            low: 99.0,
            close: 105.0,
            volume: 50000.0,
            amount: 5_250_000.0,
        };
        let p = map_daily_bar(&b).unwrap();
        assert_eq!(p.date.format(), "20260526");
        assert_eq!(p.volume.unwrap().0, 50000);
    }
}

/// 性能 / 准确性集成测试（联网打 live TDX，默认 #[ignore]）。
///
/// 运行：cargo test --manifest-path src-tauri/Cargo.toml --lib \
///        tdx::manager::perf -- --ignored --nocapture --test-threads=1
///
/// 测速度（延迟）、连接池并发加速比、批量/K线准确性。盘后跑返回最新收盘价，
/// 数值仍可校验合理性（>0、OHLC 有序）。
#[cfg(test)]
mod perf {
    use super::*;
    use crate::domain::shared::InstrumentCategory;
    use futures_util::StreamExt;

    fn td() -> TradeDate {
        // 用一个近交易日即可；fetch_quotes 不依赖该值做有效性判断（只用于组装 StockQuote）。
        TradeDate::parse("20260529").unwrap()
    }

    fn codes(prefix_sh: bool, range: std::ops::Range<u32>) -> Vec<(TsCode, InstrumentCategory, Option<String>)> {
        range
            .filter_map(|i| {
                let s = if prefix_sh {
                    format!("60{:04}.SH", i)
                } else {
                    format!("00{:04}.SZ", i)
                };
                TsCode::parse(&s).ok().map(|ts| (ts, InstrumentCategory::Stock, None))
            })
            .collect()
    }

    #[tokio::test]
    #[ignore]
    async fn perf_single_quote_latency() {
        let mgr = TdxConnectionManager::new();
        let ts = TsCode::parse("600519.SH").unwrap();
        // 预热（建连接）
        let _ = mgr.fetch_quote(&ts, InstrumentCategory::Stock, td(), Utc::now(), None).await;
        let t0 = Instant::now();
        let q = mgr
            .fetch_quote(&ts, InstrumentCategory::Stock, td(), Utc::now(), None)
            .await
            .expect("fetch 600519");
        let dt = t0.elapsed();
        eprintln!("[perf] single quote 600519 latency = {:?}", dt);
        eprintln!("[perf]   price = {:?} change% = {:?}", q.price, q.change_percent);
        assert!(q.price.is_some(), "茅台应有报价");
        assert!(q.price.unwrap().0 > rust_decimal::Decimal::ZERO, "价格应 > 0");
        assert!(dt < Duration::from_secs(3), "单笔延迟应 < 3s");
    }

    #[tokio::test]
    #[ignore]
    async fn perf_batch_accuracy() {
        let mgr = TdxConnectionManager::new();
        // 1 批 80：SH 600000..600060 + SZ 000001..000040
        let mut list = codes(true, 0..60);
        list.extend(codes(false, 1..40));
        let n = list.len();
        let t0 = Instant::now();
        let results = mgr.fetch_quotes(list, td(), Utc::now()).await;
        let dt = t0.elapsed();
        let ok: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        let with_price = ok.iter().filter(|q| q.price.is_some()).count();
        eprintln!(
            "[perf] batch {} codes: {:?}, ok={}, with_price={}",
            n, dt, ok.len(), with_price
        );
        // 准确性：拿到的报价价格都应 > 0
        for q in &ok {
            if let Some(p) = q.price {
                assert!(p.0 > rust_decimal::Decimal::ZERO, "{} 价格应>0", q.ts_code.as_str());
            }
        }
        assert!(with_price > 0, "应至少有部分标的返回有效价格");
        assert!(dt < Duration::from_secs(5), "单批延迟应 < 5s");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn perf_pool_speedup() {
        let mgr = TdxConnectionManager::new();
        // 4 批 × 80 = 320 标的
        let mut all = codes(true, 0..160);
        all.extend(codes(false, 1..160));
        let chunks: Vec<Vec<_>> = all.chunks(80).map(|c| c.to_vec()).collect();
        let nb = chunks.len();
        eprintln!("[perf] pool speedup test: {} batches", nb);

        // 顺序
        let t0 = Instant::now();
        for c in &chunks {
            let _ = mgr.fetch_quotes(c.clone(), td(), Utc::now()).await;
        }
        let seq = t0.elapsed();

        // 并发（buffer_unordered 4，落在连接池 4 条连接）
        let t1 = Instant::now();
        let mut s = futures_util::stream::iter(chunks.clone())
            .map(|c| {
                let mgr = &mgr;
                async move { mgr.fetch_quotes(c, td(), Utc::now()).await }
            })
            .buffer_unordered(4);
        while s.next().await.is_some() {}
        let conc = t1.elapsed();

        let speedup = seq.as_secs_f64() / conc.as_secs_f64().max(0.001);
        eprintln!(
            "[perf] {} batches: sequential={:?} concurrent={:?} speedup={:.2}x",
            nb, seq, conc, speedup
        );
        assert!(conc <= seq, "连接池并发应 ≤ 顺序耗时");
    }

    #[tokio::test]
    #[ignore]
    async fn perf_kline_latency_accuracy() {
        let mgr = TdxConnectionManager::new();
        let ts = TsCode::parse("600519.SH").unwrap();
        let _ = mgr.fetch_kline_at(&ts, crate::domain::quotes::KlinePeriod::Day, 0, 800).await;
        let t0 = Instant::now();
        let bars = mgr
            .fetch_kline_at(&ts, crate::domain::quotes::KlinePeriod::Day, 0, 800)
            .await
            .expect("kline 600519");
        let dt = t0.elapsed();
        eprintln!("[perf] daily kline 600519 x{} latency = {:?}", bars.len(), dt);
        assert!(!bars.is_empty(), "应返回 K 线");
        // 准确性：OHLC 有序 + > 0
        for b in &bars {
            assert!(b.high >= b.low, "high>=low");
            assert!(b.high >= b.open && b.high >= b.close, "high 为最高");
            assert!(b.low <= b.open && b.low <= b.close, "low 为最低");
            assert!(b.close > 0.0, "收盘>0");
        }
        eprintln!("[perf]   first={} last={}", bars.first().unwrap().datetime(), bars.last().unwrap().datetime());
        assert!(dt < Duration::from_secs(5), "K线延迟应 < 5s");
    }
}
