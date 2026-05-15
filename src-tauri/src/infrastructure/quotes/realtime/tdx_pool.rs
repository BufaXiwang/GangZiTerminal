//! TDX 连接池——8 个并行连接，专用于**全市场刷新**（universe loop）。
//!
//! 与 `realtime::tdx::TdxSource`（单 client，dispatch 链用）的关系：
//! - `TdxSource`：active_set 高频小批刷新（≤80 codes，~50ms 单 client）
//! - `TdxConnectionPool`：universe 大批刷新（5000+ codes，需要并行加速）
//!
//! 各 client 在首次使用时 `connect_bestip()` 并行竞速绑定一台 HQ 服务器（16 台中选最快），
//! 之后长连复用；任一请求失败丢弃重连。
//!
//! 并行执行：`fetch_batches` 用 `buffered_unordered(POOL_SIZE)` 把 N 个 batch 分发到 8 client。

use crate::infrastructure::quotes::tdx::client::TdxHqClient;
use crate::infrastructure::quotes::tdx::types::{Market, SecurityQuote};
use futures_util::stream::{self, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const POOL_SIZE: usize = 8;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct TdxConnectionPool {
    /// 8 个独立 Client；每个 lock 独立 → 并行使用
    clients: Vec<Arc<Mutex<Option<TdxHqClient>>>>,
}

impl TdxConnectionPool {
    pub fn new() -> Self {
        Self {
            clients: (0..POOL_SIZE).map(|_| Arc::new(Mutex::new(None))).collect(),
        }
    }

    pub fn size(&self) -> usize {
        POOL_SIZE
    }

    /// 并行处理一批 batch（每个 batch ≤ 80 个 (Market, code)）。
    /// 返回所有成功 batch 的 SecurityQuote 合集；失败的 batch 静默跳过 + log。
    pub async fn fetch_batches(&self, batches: Vec<Vec<(Market, String)>>) -> Vec<SecurityQuote> {
        if batches.is_empty() {
            return Vec::new();
        }

        let clients = self.clients.clone();
        let results: Vec<Vec<SecurityQuote>> = stream::iter(batches.into_iter().enumerate())
            .map(move |(idx, batch)| {
                let client = clients[idx % POOL_SIZE].clone();
                async move {
                    match fetch_one_batch(client, batch).await {
                        Ok(qs) => qs,
                        Err(e) => {
                            tracing::warn!(err = %e, "tdx pool batch failed");
                            Vec::new()
                        }
                    }
                }
            })
            .buffer_unordered(POOL_SIZE)
            .collect()
            .await;

        results.into_iter().flatten().collect()
    }
}

async fn fetch_one_batch(
    client_arc: Arc<Mutex<Option<TdxHqClient>>>,
    batch: Vec<(Market, String)>,
) -> Result<Vec<SecurityQuote>, String> {
    tokio::task::spawn_blocking(move || -> Result<Vec<SecurityQuote>, String> {
        let mut guard = client_arc.blocking_lock();

        // 懒建连
        if guard.is_none() {
            let (c, addr) = TdxHqClient::connect_bestip(CONNECT_TIMEOUT)
                .map_err(|e| format!("connect: {e}"))?;
            tracing::debug!(peer = %addr, "tdx pool 连接建立");
            *guard = Some(c);
        }

        let client = guard.as_mut().expect("just inited");
        let pairs: Vec<(Market, &str)> = batch.iter().map(|(m, c)| (*m, c.as_str())).collect();
        match client.security_quotes(&pairs) {
            Ok(qs) => Ok(qs),
            Err(e) => {
                // 失败丢弃连接，下次重连
                *guard = None;
                Err(format!("query: {e}"))
            }
        }
    })
    .await
    .map_err(|e| format!("join: {e}"))?
}
