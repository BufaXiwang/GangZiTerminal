//! 按 proxy URL 缓存的 reqwest::Client——给 em/tencent/sina 三源共用。
//!
//! 每个 source 自带 base builder（UA / Referer / timeout 等），cache 在此之上叠加 proxy。
//! 同一个 (source, proxy_url) 对 Client 复用——避免每个 batch 都重新 TLS handshake。

use crate::domain::quotes::QuotesError;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

type BuilderFn = Box<dyn Fn() -> reqwest::ClientBuilder + Send + Sync>;

pub struct ProxyClientCache {
    base_builder: BuilderFn,
    /// `None` key = 直连 client；`Some(url)` key = 走该 proxy 的 client
    clients: RwLock<HashMap<Option<String>, Arc<reqwest::Client>>>,
}

impl ProxyClientCache {
    pub fn new<F>(base_builder: F) -> Self
    where
        F: Fn() -> reqwest::ClientBuilder + Send + Sync + 'static,
    {
        Self {
            base_builder: Box::new(base_builder),
            clients: RwLock::new(HashMap::new()),
        }
    }

    /// 拿到对应 proxy 的 client；不在缓存中则按 base_builder 现 build 一个并 cache。
    pub fn get(&self, proxy_url: Option<&str>) -> Result<Arc<reqwest::Client>, QuotesError> {
        let key = proxy_url.map(|s| s.to_string());
        if let Ok(g) = self.clients.read() {
            if let Some(c) = g.get(&key) {
                return Ok(c.clone());
            }
        }
        let mut b = (self.base_builder)();
        if let Some(p) = proxy_url {
            let proxy = reqwest::Proxy::all(p)
                .map_err(|e| QuotesError::Network(format!("代理 {p} 解析失败: {e}")))?;
            b = b.proxy(proxy);
        }
        let c = Arc::new(
            b.build()
                .map_err(|e| QuotesError::Network(format!("client 构造失败: {e}")))?,
        );
        if let Ok(mut g) = self.clients.write() {
            g.insert(key, c.clone());
        }
        Ok(c)
    }

    /// 代理列表变更后调用——丢弃所有已 build 的 client，下次 get 时按新配置 rebuild。
    #[allow(dead_code)]
    pub fn invalidate(&self) {
        if let Ok(mut g) = self.clients.write() {
            g.clear();
        }
    }
}
