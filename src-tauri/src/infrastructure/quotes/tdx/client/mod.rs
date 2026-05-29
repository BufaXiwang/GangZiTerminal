//! Synchronous TCP client for the Tdx HQ (行情) protocol.
//!
//! See [`TdxHqClient`] for the high-level entry point.

mod cmd;
pub(crate) mod frame;
mod handshake;

/// Internal command builders/parsers exposed for parity tests. Not part of the
/// stable API — names start with `cmd_test_hooks::` to discourage external use.
#[doc(hidden)]
pub mod cmd_test_hooks {
    use super::super::error::Result;
    use super::super::types::{Bar, BarCategory};

    pub fn build_security_bars(
        category: BarCategory,
        market: u8,
        code: &str,
        start: u16,
        count: u16,
    ) -> Result<Vec<u8>> {
        super::cmd::security_bars::build(category, market, code, start, count)
    }

    pub fn parse_security_bars(
        body: &[u8],
        category: BarCategory,
        is_index: bool,
    ) -> Result<Vec<Bar>> {
        super::cmd::security_bars::parse(body, category, is_index)
    }
}

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::error::{Error, Result};
use super::hosts::HQ_HOSTS;
use super::types::{
    Bar, BarCategory, Market, MinuteTimePoint, SecurityListEntry, SecurityQuote, XdxrRecord,
};

/// Blocking Tdx HQ client. Open with [`connect`](Self::connect) (or
/// [`connect_default`](Self::connect_default)) and call methods directly.
///
/// Mirrors the high-level surface of `mootdx.quotes.StdQuotes` /
/// `pytdx.hq.TdxHq_API` — only the commands the user asked for are wired up.
pub struct TdxHqClient {
    sock: TcpStream,
}

impl TdxHqClient {
    /// Connect to an explicit `addr:port`, run the handshake, and return a ready client.
    pub fn connect<A: ToSocketAddrs>(addr: A, timeout: Duration) -> Result<Self> {
        let socket_addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| Error::Protocol("no resolved address".into()))?;
        let sock = TcpStream::connect_timeout(&socket_addr, timeout)?;
        sock.set_read_timeout(Some(timeout))?;
        sock.set_write_timeout(Some(timeout))?;
        sock.set_nodelay(true)?;

        let mut client = TdxHqClient { sock };
        handshake::run(&mut client.sock)?;
        Ok(client)
    }

    /// Try built-in servers sequentially; return the first that connects.
    ///
    /// Simple but slow when the first hosts are dead — prefer [`connect_bestip`]
    /// which races them in parallel.
    pub fn connect_default(timeout: Duration) -> Result<Self> {
        let mut last_err: Option<Error> = None;
        for (_name, host, port) in HQ_HOSTS {
            match Self::connect((*host, *port), timeout) {
                Ok(c) => return Ok(c),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| Error::Protocol("no servers configured".into())))
    }

    /// Race all built-in servers in parallel; return the first successful handshake.
    ///
    /// Spawns one OS thread per host. Each thread runs the full TCP connect +
    /// 3-step handshake; the first to finish wins. Losing threads are detached
    /// and their sockets dropped — they may continue connecting in the
    /// background briefly but will not block the caller.
    ///
    /// Returns `Ok((client, "ip:port"))` on success.
    pub fn connect_bestip(timeout: Duration) -> Result<(Self, String)> {
        let (tx, rx) = mpsc::channel::<std::result::Result<(TcpStream, SocketAddr), Error>>();
        let started = Instant::now();

        for (_name, host, port) in HQ_HOSTS {
            let tx = tx.clone();
            let host = *host;
            let port = *port;
            thread::spawn(move || {
                let result = race_one(host, port, timeout);
                // Send may fail if the receiver was dropped after a winner — that's fine.
                let _ = tx.send(result);
            });
        }
        // Drop the original sender so rx.recv() returns Err once all workers finish.
        drop(tx);

        let deadline = started + timeout + Duration::from_secs(1);
        let mut last_err: Option<Error> = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok(Ok((sock, addr))) => {
                    return Ok((Self { sock }, addr.to_string()));
                }
                Ok(Err(e)) => last_err = Some(e),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        Err(last_err.unwrap_or_else(|| Error::Protocol("no servers reachable".into())))
    }

    /// Peer address the client is connected to (useful after `connect_bestip`).
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.sock.peer_addr().ok()
    }

    /// Number of securities listed on the given market (0 = SZ, 1 = SH).
    pub fn security_count(&mut self, market: Market) -> Result<u16> {
        let pkg = cmd::security_count::build(market.as_u8() as u16);
        let body = frame::request(&mut self.sock, &pkg)?;
        cmd::security_count::parse(&body)
    }

    /// Paged security list (256 entries per page).
    pub fn security_list(&mut self, market: Market, start: u16) -> Result<Vec<SecurityListEntry>> {
        let pkg = cmd::security_list::build(market.as_u8() as u16, start);
        let body = frame::request(&mut self.sock, &pkg)?;
        cmd::security_list::parse(&body)
    }

    /// K-line bars. `count` is capped server-side around 800.
    ///
    /// 指数（SH 000xxx / 999xxx, SZ 399xxx）每根 bar 比股票多 4 字节
    /// (up_count/down_count)。caller 必须按 code/market 正确传 `is_index`，
    /// 否则解码会逐 bar 累计 4 字节漂移导致价格全部错位。
    pub fn security_bars(
        &mut self,
        category: BarCategory,
        market: Market,
        code: &str,
        start: u16,
        count: u16,
    ) -> Result<Vec<Bar>> {
        let pkg = cmd::security_bars::build(category, market.as_u8(), code, start, count)?;
        let body = frame::request(&mut self.sock, &pkg)?;
        let is_index = cmd::security_bars::is_index_code(code, market.as_u8());
        cmd::security_bars::parse(&body, category, is_index)
    }

    /// Real-time L1 quotes for up to ~80 (market, code) pairs.
    pub fn security_quotes(&mut self, stocks: &[(Market, &str)]) -> Result<Vec<SecurityQuote>> {
        let stocks: Vec<(u8, &str)> = stocks.iter().map(|(m, c)| (m.as_u8(), *c)).collect();
        let pkg = cmd::security_quotes::build(&stocks)?;
        let body = frame::request(&mut self.sock, &pkg)?;
        cmd::security_quotes::parse(&body)
    }

    /// 除权除息 / 公司行动历史。返回 `XdxrRecord` 序列；上层（pipeline）负责
    /// 翻译 category 1 等记录为 qfq / hfq 计算输入。
    pub fn security_xdxr(&mut self, market: Market, code: &str) -> Result<Vec<XdxrRecord>> {
        let pkg = cmd::security_xdxr::build(market.as_u8(), code)?;
        let body = frame::request(&mut self.sock, &pkg)?;
        cmd::security_xdxr::parse(&body)
    }

    /// 当日分时（240 个交易分钟价格 + 成交量序列）。
    /// 返回的 `MinuteTimePoint` 不含时间戳——index 对应交易时段第 N 分钟。
    pub fn security_minute_time(
        &mut self,
        market: Market,
        code: &str,
    ) -> Result<Vec<MinuteTimePoint>> {
        let pkg = cmd::security_minute::build(market.as_u8(), code)?;
        let body = frame::request(&mut self.sock, &pkg)?;
        cmd::security_minute::parse(&body)
    }
}

impl Drop for TdxHqClient {
    fn drop(&mut self) {
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod minute_diag {
    use super::*;
    use byteorder::ByteOrder;

    /// 联网诊断（分时下线调查留档）：dump minute_time 原始 body + 逐点解码。
    /// 结论：当前 TDX 服务器 minute 响应非标准（body 回显 code + per-point 结构
    /// 与 pytdx/mootdx 不一致），分时已 descoped。详见 quotes-module.md §5。
    /// 运行：cargo test --lib tdx::client::minute_diag -- --ignored --nocapture
    #[test]
    #[ignore]
    fn dump_minute_raw() {
        let (mut cli, ip) =
            TdxHqClient::connect_bestip(Duration::from_secs(5)).expect("connect");
        eprintln!("connected via {ip}");
        // 600519 贵州茅台 (SH)，收盘价 ~1500+，分时第一个点应接近开盘价。
        let pkg = cmd::security_minute::build(Market::SH.as_u8(), "600519").unwrap();
        let body = frame::request(&mut cli.sock, &pkg).expect("request");
        eprintln!("body.len = {}", body.len());
        let hex: Vec<String> = body.iter().take(48).map(|b| format!("{b:02x}")).collect();
        eprintln!("first 48 bytes: {}", hex.join(" "));
        let num = byteorder::LittleEndian::read_u16(&body[0..2]);
        eprintln!("num(u16@0) = {num}");

        // 详细 dump：从 offset 11(code 后) 起，逐点打印 3 个 varint 的原始值 + 消耗字节数。
        for start in [11usize, 13usize] {
            eprintln!("=== detailed decode from offset {start} (3 varints/point) ===");
            let mut pos = start;
            let mut last = 0i64;
            for i in 0..10 {
                let p0 = pos;
                let Ok(d) = super::super::helper::get_price(&body, &mut pos) else { break };
                let p1 = pos;
                let Ok(r) = super::super::helper::get_price(&body, &mut pos) else { break };
                let p2 = pos;
                let Ok(v) = super::super::helper::get_price(&body, &mut pos) else { break };
                last += d;
                eprintln!(
                    "  pt{i}: d={d}({}B) r={r}({}B) v={v}({}B) | accum={:.2}",
                    p1 - p0, p2 - p1, pos - p2, last as f64 / 100.0
                );
            }
        }
    }
}

/// Connect + handshake against one host. Used by [`TdxHqClient::connect_bestip`].
fn race_one(host: &str, port: u16, timeout: Duration) -> Result<(TcpStream, SocketAddr)> {
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::Protocol(format!("no resolved address for {host}:{port}")))?;
    let mut sock = TcpStream::connect_timeout(&addr, timeout)?;
    sock.set_read_timeout(Some(timeout))?;
    sock.set_write_timeout(Some(timeout))?;
    sock.set_nodelay(true)?;
    handshake::run(&mut sock)?;
    Ok((sock, addr))
}
