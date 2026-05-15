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

    pub fn parse_security_bars(body: &[u8], category: BarCategory) -> Result<Vec<Bar>> {
        super::cmd::security_bars::parse(body, category)
    }
}

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::error::{Error, Result};
use super::hosts::HQ_HOSTS;
use super::types::{Bar, BarCategory, Market, SecurityListEntry, SecurityQuote};

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
        cmd::security_bars::parse(&body, category)
    }

    /// Real-time L1 quotes for up to ~80 (market, code) pairs.
    pub fn security_quotes(&mut self, stocks: &[(Market, &str)]) -> Result<Vec<SecurityQuote>> {
        let stocks: Vec<(u8, &str)> = stocks.iter().map(|(m, c)| (m.as_u8(), *c)).collect();
        let pkg = cmd::security_quotes::build(&stocks)?;
        let body = frame::request(&mut self.sock, &pkg)?;
        cmd::security_quotes::parse(&body)
    }
}

impl Drop for TdxHqClient {
    fn drop(&mut self) {
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
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
