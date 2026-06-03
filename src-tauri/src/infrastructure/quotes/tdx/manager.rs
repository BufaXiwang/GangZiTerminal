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
use crate::domain::quotes::{HostProbe, MinuteKlinePoint, QuoteSource, StockQuote, TradeStatus};
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
/// 连接槽 Vec 物理上限（spec §5「TDX 连接池与并发」：池大小动态、由低延时台数决定，
/// 但 slot Vec 不超过该值，有效并发 = `active_count` ≤ MAX_POOL）。
const MAX_POOL: usize = 12;
/// 动态并发下界：可达台数 ≥ 该值时至少开这么多连接（不足则有几台用几台，≥1）。
const MIN_POOL: usize = 2;
/// 低延时子集带宽（spec §5）：可达台里只选 `latency ≤ 最快台 + LATENCY_BAND` 的进池，
/// 自然排除「可达但慢」的站点。
const LATENCY_BAND: Duration = Duration::from_millis(250);
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

/// 一次性探测后定下的「动态池」：低延时子集 host 列表 + 有效并发数。
///
/// `active_hosts` = 选中的低延时台（每台 1 连接、各 slot pin 一台不同的），长度即 `active_count`。
/// slot Vec 物理上预分配 `MAX_POOL` 条，但只有前 `active_count` 条参与 round-robin。
struct Pool {
    /// 被选进池的低延时 host（已按延迟升序），槽 i 默认 pin 到 `active_hosts[i]`。
    active_hosts: Arc<Vec<(String, u16)>>,
    /// 有效并发数 = 选中台数 = `active_hosts.len()`（≥1）。
    active_count: usize,
}

#[derive(Clone)]
pub struct TdxConnectionManager {
    /// 最多 `MAX_POOL` 条独立连接槽（物理上限）；只有前 `active_count` 条参与 round-robin。
    slots: Arc<Vec<Arc<Mutex<State>>>>,
    next: Arc<AtomicUsize>,
    /// 动态池：首次需要连接时**并行**探测全部 `HQ_HOSTS`、选低延时子集，算一次并缓存
    /// （spec §5：并发数 N = 低延时台数 clamp[2,12]，每连接 pin 一台不同 host）。
    pool: Arc<Mutex<Option<Arc<Pool>>>>,
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

/// 把槽下标映射到选中低延时子集的初始下标：`slot_idx % active_hosts.len()`。
///
/// 动态池下 active_count = 低延时台数 → 槽 i (i<active_count) pin 到选中第 i 台不同 host，每台 ≤1 连接。
/// 纯函数，无 I/O，便于单测。`ranked_len` 为 0 时返回 0（调用方需另行保证非空）。
fn assign_host(slot_idx: usize, ranked_len: usize) -> usize {
    if ranked_len == 0 {
        0
    } else {
        slot_idx % ranked_len
    }
}

/// 单台探测结果（内部用）：延迟 + 可达 + 站点元信息。`latency` 仅在 `ok` 时有意义。
#[derive(Clone)]
struct Probe {
    name: &'static str,
    host: String,
    port: u16,
    latency: Duration,
    ok: bool,
}

/// **并行**探测全部 `HQ_HOSTS`（每台一线程，`PROBE_TIMEOUT` 上限），返回按
/// 「成功优先 → 延迟升序」排序的结果列表。
///
/// 复用 [`TdxHqClient::connect`] 的「connect + handshake」语义做一次性 probe：每台单独计时，
/// 连不上 / 握手失败标 `ok=false` 排到队尾。**会联网**。串行探测 ~sum(timeout)：部分 host
/// 慢/死时每台 3s 累加 → 首次建连阻塞数十秒。并行后墙钟 ≈ 最慢一台 ~PROBE_TIMEOUT。
fn probe_all_latency() -> Vec<Probe> {
    let handles: Vec<_> = HQ_HOSTS
        .iter()
        .map(|(name, host, port)| {
            let name = *name;
            let host = host.to_string();
            let port = *port;
            std::thread::spawn(move || {
                let t0 = Instant::now();
                let ok = TdxHqClient::connect((host.as_str(), port), PROBE_TIMEOUT).is_ok();
                Probe {
                    name,
                    host,
                    port,
                    latency: t0.elapsed(),
                    ok,
                }
            })
        })
        .collect();
    let mut probes: Vec<Probe> = handles.into_iter().filter_map(|h| h.join().ok()).collect();
    // 成功优先，其次按延迟升序。
    probes.sort_by(|a, b| match (a.ok, b.ok) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.latency.cmp(&b.latency),
    });
    probes
}

/// 从「已按成功优先 + 延迟升序排好的」候选中选低延时子集做并发（spec §5）。**纯函数**，便于单测。
///
/// 规则：
/// 1. 只看可达（`ok`）台；按延迟取 `latency ≤ 最快台 + band` 的那些（自然排除「可达但慢」）。
/// 2. 并发数 N = 该子集大小，clamp 到 `[min, max]`：
///    - 子集 > max → 截到 max（取最快的 max 台）。
///    - 子集 < min → 用全部可达台兜底（不足 min 则有几台用几台，≥1）。
/// 3. 返回选中台的 `(host, port)` 列表（即各 slot pin 的 host）。
///
/// 输入 `ranked` 必须已排序（`probe_all_latency` 的输出）。可达台数为 0 时返回空 Vec
/// （调用方再回退到全量 `HQ_HOSTS`）。
fn select_low_latency(
    ranked: &[Probe],
    band: Duration,
    min: usize,
    max: usize,
) -> Vec<(String, u16)> {
    let reachable: Vec<&Probe> = ranked.iter().filter(|p| p.ok).collect();
    if reachable.is_empty() {
        return Vec::new();
    }
    let fastest = reachable[0].latency;
    let cutoff = fastest.saturating_add(band);
    // 带宽内的低延时子集（reachable 已按延迟升序，故为前缀）。
    let in_band: Vec<&Probe> = reachable
        .iter()
        .copied()
        .filter(|p| p.latency <= cutoff)
        .collect();
    let chosen: &[&Probe] = if in_band.len() >= min {
        // 子集够大：用子集，并 clamp 到 max。
        let n = in_band.len().min(max);
        &in_band[..n]
    } else {
        // 子集不足下界：用全部可达兜底，clamp 到 max（仍 ≥1）。
        let n = reachable.len().min(max);
        &reachable[..n]
    };
    chosen.iter().map(|p| (p.host.clone(), p.port)).collect()
}

impl Default for TdxConnectionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TdxConnectionManager {
    pub fn new() -> Self {
        // 物理上预分配 MAX_POOL 条槽；首次探测后只有前 active_count 条参与 round-robin
        // （active_count = 低延时台数 clamp[MIN_POOL, MAX_POOL]，spec §5）。
        let slots = (0..MAX_POOL)
            .map(|i| {
                Arc::new(Mutex::new(State {
                    client: None,
                    last_call: None,
                    // 槽 i 默认 pin 到选中低延时子集第 i 台 host（首次连接时按 active_hosts 解析）。
                    host_idx: i,
                }))
            })
            .collect();
        Self {
            slots: Arc::new(slots),
            next: Arc::new(AtomicUsize::new(0)),
            pool: Arc::new(Mutex::new(None)),
        }
    }

    /// round-robin 取一条 **active** 连接槽（携带其下标，用于 pin host）。
    ///
    /// 只在前 `active_count` 条物理槽里轮转（`next % active_count`），其余 `MAX_POOL - active_count`
    /// 条不参与（对应的低延时台不足时不开连接）。并发调用各拿不同槽 → 真并行。
    fn slot(&self) -> (usize, Arc<Mutex<State>>) {
        let active = self.pool().active_count.max(1).min(self.slots.len());
        let i = self.next.fetch_add(1, Ordering::Relaxed) % active;
        (i, Arc::clone(&self.slots[i]))
    }

    /// 有效并发数（active 连接数）。供 service 层把 universe burst / refresh_quote_batch 的
    /// `buffer_unordered` 并发度接到实际池大小，而非硬编码（spec §5）。
    /// 首次调用会触发一次性探测。
    pub fn active_connections(&self) -> usize {
        self.pool().active_count
    }

    /// 启动预热：触发一次性 host 探测（动态池选定，~3s 并行）+ 预建立所有 active 连接，
    /// 让**首笔用户请求免去 ~3s 冷探测 + 建连延迟**（spec §连接池「启动时探测」）。
    /// best-effort：连接失败静默（connect_slot 内部已换台重试），下次正常路径再连。
    /// **阻塞**（probe + connect 走同步 TCP）——调用方须放 `spawn_blocking` / 独立线程，勿在 async 上下文直接调。
    pub fn warm(&self) {
        let pool = self.pool(); // 触发探测 + 缓存（~3s 一次性，Mutex 守护只算一次）
        let ranked = pool.active_hosts.clone();
        let active = pool.active_count.min(self.slots.len());
        for i in 0..active {
            if let Ok(mut guard) = self.slots[i].lock() {
                if guard.client.is_none() {
                    let _ = connect_slot(&mut guard, ranked.as_slice(), CONNECT_TIMEOUT);
                }
            }
        }
    }

    /// 取（必要时**并行**探测并缓存）动态池：低延时子集 host 列表 + active_count。
    ///
    /// 首次调用会联网 probe 全部 `HQ_HOSTS`、选低延时子集（spec §5），之后复用缓存。
    /// 极端：全部探测失败 → 兜底回退到全量 `HQ_HOSTS`（仍 clamp 到 MAX_POOL）。
    fn pool(&self) -> Arc<Pool> {
        let mut guard = self.pool.lock().expect("pool poisoned");
        if let Some(p) = guard.as_ref() {
            return Arc::clone(p);
        }
        let probes = probe_all_latency();
        let mut selected = select_low_latency(&probes, LATENCY_BAND, MIN_POOL, MAX_POOL);
        if selected.is_empty() {
            // 全部探测失败：兜底用全量 HQ_HOSTS（clamp 到 MAX_POOL），让 connect_slot 自己换台重试。
            selected = HQ_HOSTS
                .iter()
                .take(MAX_POOL)
                .map(|(_, h, p)| (h.to_string(), *p))
                .collect();
        }
        let active_count = selected.len().max(1);
        let pool = Arc::new(Pool {
            active_hosts: Arc::new(selected),
            active_count,
        });
        *guard = Some(Arc::clone(&pool));
        pool
    }

    /// 并行探测**全部** `HQ_HOSTS`，返回每台 `HostProbe`（给前端延时 popup）。
    ///
    /// Spec: docs/design/quotes-module.md §TDX 连接池与并发。
    ///
    /// `inPool` = 该台是否被选进当前 active 池（低延时子集）。会先确保动态池已选定
    /// （`self.pool()` 首次触发一次性探测、之后复用缓存），再按当前 active 池的 host 集合标记。
    /// 本方法自身的探测是**实时**的（不读 pool 缓存的延迟），故 popup 的延迟反映当下网络。
    /// 结果按「成功优先 → 延迟升序」排序（同 `probe_all_latency`）。
    pub fn probe_all_hosts(&self) -> Vec<HostProbe> {
        // 当前 active 池的 host 集合（host:port）。确保已探测选定。
        let pool = self.pool();
        let in_pool: std::collections::HashSet<(String, u16)> = pool
            .active_hosts
            .iter()
            .map(|(h, p)| (h.clone(), *p))
            .collect();
        probe_all_latency()
            .into_iter()
            .map(|p| HostProbe {
                name: p.name.to_string(),
                latency_ms: if p.ok {
                    Some(p.latency.as_millis().min(u32::MAX as u128) as u32)
                } else {
                    None
                },
                ok: p.ok,
                in_pool: in_pool.contains(&(p.host.clone(), p.port)),
                host: p.host,
                port: p.port,
            })
            .collect()
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
        let ranked = self.pool().active_hosts.clone();
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
            let ranked = self.pool().active_hosts.clone();
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
        let ranked = self.pool().active_hosts.clone();
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
        let ranked = self.pool().active_hosts.clone();
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
        let ranked = self.pool().active_hosts.clone();
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
        let ranked = self.pool().active_hosts.clone();
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
        let ranked = self.pool().active_hosts.clone();
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
        let ranked = self.pool().active_hosts.clone();
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
        let ranked = self.pool().active_hosts.clone();
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

    /// 测试辅助：构造 N 台延迟（毫秒）+ ok 的 Probe 列表（已按延迟升序），免联网。
    fn probes(specs: &[(u64, bool)]) -> Vec<Probe> {
        specs
            .iter()
            .enumerate()
            .map(|(i, (ms, ok))| Probe {
                name: "t",
                host: format!("10.0.0.{i}"),
                port: 7709,
                latency: Duration::from_millis(*ms),
                ok: *ok,
            })
            .collect()
    }

    /// 测试辅助：直接把 active_hosts 注入 pool 缓存，免触发网络探测。
    fn seed_pool(mgr: &TdxConnectionManager, hosts: Vec<(String, u16)>) {
        let active_count = hosts.len().max(1);
        let pool = Arc::new(Pool {
            active_hosts: Arc::new(hosts),
            active_count,
        });
        *mgr.pool.lock().unwrap() = Some(pool);
    }

    #[test]
    fn new_builds_max_pool_slots() {
        let mgr = TdxConnectionManager::new();
        // 物理槽 = MAX_POOL（动态池只激活前 active_count 条）；next 从 0 起。
        assert_eq!(mgr.slots.len(), MAX_POOL);
        assert_eq!(mgr.next.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn slot_round_robins_only_through_active_subset() {
        // active_count = 3（注入 3 台低延时 host）→ slot() 只在 slots[0..3] 轮转。
        let mgr = TdxConnectionManager::new();
        seed_pool(
            &mgr,
            vec![
                ("a".into(), 7709),
                ("b".into(), 7709),
                ("c".into(), 7709),
            ],
        );
        assert_eq!(mgr.active_connections(), 3);
        let picked: Vec<(usize, Arc<Mutex<State>>)> = (0..3).map(|_| mgr.slot()).collect();
        for i in 0..3 {
            assert_eq!(picked[i].0, i, "pick {i} index");
            assert!(Arc::ptr_eq(&picked[i].1, &mgr.slots[i]), "pick {i} → slots[{i}]");
        }
        // 第 4 次回绕到 slots[0]（不会用到 slots[3..MAX_POOL]）。
        let wrapped = mgr.slot();
        assert_eq!(wrapped.0, 0, "active_count=3 → 第 4 次 wrap 回 0");
        assert!(Arc::ptr_eq(&wrapped.1, &mgr.slots[0]));
    }

    #[test]
    fn select_low_latency_picks_in_band_subset() {
        // 最快 30ms，band 250ms → cutoff 280ms：选 30/50/200（≤280），排除 400/可达但慢。
        let ranked = probes(&[(30, true), (50, true), (200, true), (400, true)]);
        let sel = select_low_latency(&ranked, Duration::from_millis(250), 2, 12);
        assert_eq!(sel.len(), 3, "带宽内应选 3 台（30/50/200）");
        assert_eq!(sel[0].0, "10.0.0.0");
        assert_eq!(sel[2].0, "10.0.0.2");
    }

    #[test]
    fn select_low_latency_clamps_to_max() {
        // 15 台全在带宽内 → clamp 到 max=12（取最快的 12 台）。
        let specs: Vec<(u64, bool)> = (0..15).map(|i| (10 + i, true)).collect();
        let ranked = probes(&specs);
        let sel = select_low_latency(&ranked, Duration::from_millis(250), 2, 12);
        assert_eq!(sel.len(), 12, "应 clamp 到 max=12");
    }

    #[test]
    fn select_low_latency_falls_back_to_all_reachable_when_band_too_narrow() {
        // 最快 30ms，band 5ms → cutoff 35ms：带宽内只有 1 台 < min=2 →
        // 兜底用全部可达（4 台，clamp max=12）。
        let ranked = probes(&[(30, true), (100, true), (300, true), (900, true)]);
        let sel = select_low_latency(&ranked, Duration::from_millis(5), 2, 12);
        assert_eq!(sel.len(), 4, "带宽内不足 min → 兜底全部可达");
    }

    #[test]
    fn select_low_latency_uses_only_reachable_and_handles_one() {
        // 仅 1 台可达（其余 ok=false）→ 兜底用那 1 台（< min 也 ≥1）。
        let ranked = probes(&[(40, true), (50, false), (60, false)]);
        let sel = select_low_latency(&ranked, Duration::from_millis(250), 2, 12);
        assert_eq!(sel.len(), 1, "仅 1 台可达 → 用 1 台");
        assert_eq!(sel[0].0, "10.0.0.0");
        // 全不可达 → 空（调用方回退全量 HQ_HOSTS）。
        let none = probes(&[(40, false), (50, false)]);
        assert!(select_low_latency(&none, Duration::from_millis(250), 2, 12).is_empty());
    }

    #[test]
    fn active_count_equals_selected_hosts_len() {
        // active_count = 选中台数（注入 5 台 → 5）。
        let mgr = TdxConnectionManager::new();
        let hosts: Vec<(String, u16)> = (0..5).map(|i| (format!("h{i}"), 7709u16)).collect();
        seed_pool(&mgr, hosts);
        assert_eq!(mgr.active_connections(), 5);
    }

    #[test]
    fn assign_host_disperses_slots_to_distinct_hosts() {
        // active_count 个槽 → 每槽分到不同 host（slot_idx % len）。8 槽 / 12 台 → 前 8 个互不相同。
        let ranked_len = 12;
        let assigned: Vec<usize> = (0..8).map(|i| assign_host(i, ranked_len)).collect();
        assert_eq!(assigned, vec![0, 1, 2, 3, 4, 5, 6, 7]);
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

    // change% 数学一致性：change_percent == (price - prevClose) / prevClose × 100。
    // sample_raw: price=1500, last_close=1480 → change=20, change%=20/1480×100≈1.3514%。
    // Spec §2 StockQuote：changePercent 用百分点；不能用 prevClose 伪造现价。
    #[test]
    fn map_security_quote_change_percent_matches_price_over_prev_close() {
        let raw = sample_raw();
        let code = TsCode::parse("600519.SH").unwrap();
        let td = TradeDate::parse("20260526").unwrap();
        let q = map_security_quote(&raw, code, InstrumentCategory::Stock, None, td, Utc::now());
        let price = q.price.unwrap().0;
        let prev = q.previous_close.unwrap().0;
        let change = q.change.unwrap().0;
        // change == price - prevClose（Decimal 精确）。
        assert_eq!(change, price - prev, "change 应等于 price - prevClose");
        // change% == (price - prevClose) / prevClose × 100（容差到小数点后 4 位）。
        use rust_decimal::prelude::ToPrimitive;
        let expected_pct =
            ((price - prev) / prev * rust_decimal::Decimal::from(100)).to_f64().unwrap();
        let got_pct = q.change_percent.unwrap();
        assert!(
            (got_pct - expected_pct).abs() < 1e-4,
            "change% 数学不一致 got={got_pct} expected={expected_pct}"
        );
    }

    // 五档盘口买卖价合理性：bid[0] ≤ ask[0]（买一价 ≤ 卖一价），且各档非负。
    // sample_raw: bid0=1499 ask0=1501 → 合理。Spec §2 盘口：bid/ask 按离成交价排序。
    #[test]
    fn map_security_quote_bid_le_ask_and_non_negative() {
        let raw = sample_raw();
        let code = TsCode::parse("600519.SH").unwrap();
        let td = TradeDate::parse("20260526").unwrap();
        let q = map_security_quote(&raw, code, InstrumentCategory::Stock, None, td, Utc::now());
        let bid0 = q.bid[0].price.unwrap().0;
        let ask0 = q.ask[0].price.unwrap().0;
        assert!(bid0 <= ask0, "买一价 {bid0} 应 ≤ 卖一价 {ask0}");
        // 有价的档位价格 / 量都应非负。
        for lvl in q.bid.iter().chain(q.ask.iter()) {
            if let Some(p) = lvl.price {
                assert!(p.0 >= rust_decimal::Decimal::ZERO, "盘口价应非负");
            }
            if let Some(v) = lvl.volume {
                assert!(v.0 >= 0, "盘口量应非负");
            }
        }
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
