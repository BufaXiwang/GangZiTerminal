//! 代理 IP 池——参考 adata `proxy()` 注入点，桌面单用户应用版。
//!
//! 设计：
//! - 用户在 Settings 配代理 URL 列表（每行一个，支持 HTTP / HTTPS / SOCKS5）
//! - 直连作为最后兜底（list 空 → 全走直连）
//! - 每个 proxy 独立维护 EMA 健康度 + 拉黑窗口
//! - **本模块只管配置 + 健康度**——具体 Client 由各 source 通过 `ProxyClientCache` 借出
//!
//! 设计借鉴：
//! - adata：`proxy(is_proxy=True, ip='...')` 全局注入；失效后用户手动重设
//! - 我们改进：失败自动降权，恢复期后自动复活；UI 实时展示健康度
//!
//! 与 P2 多源 dispatch 的关系：
//! - dispatch 在 **source** 维度做 fallback（EM > 腾讯 > 新浪）
//! - proxy_pool 在 **IP** 维度做 fallback（同一 source 内多个 proxy + 直连）

use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

const HEALTH_ALPHA: f64 = 0.3;
const BLOCK_THRESHOLD: f64 = 0.2; // EMA 低于此值 → 拉黑
const BLOCK_DURATION: Duration = Duration::from_secs(5 * 60);
const MAX_ATTEMPTS_PER_SOURCE: usize = 3;

#[derive(Debug, Clone)]
pub struct ProxyEntry {
    /// `None` = 直连；`Some(url)` 形如 "socks5://127.0.0.1:7890" / "http://1.2.3.4:8080"
    pub url: Option<String>,
    pub health: f64,
    /// 拉黑到这个时间点之前
    blocked_until: Option<Instant>,
}

impl ProxyEntry {
    fn new(url: Option<String>) -> Self {
        Self {
            url,
            health: 1.0,
            blocked_until: None,
        }
    }

    pub fn is_blocked(&self, now: Instant) -> bool {
        match self.blocked_until {
            Some(t) => now < t,
            None => false,
        }
    }

    /// 给 UI 看的友好标识。
    pub fn label(&self) -> String {
        self.url.clone().unwrap_or_else(|| "(direct)".to_string())
    }
}

pub struct ProxyPool {
    entries: RwLock<Vec<ProxyEntry>>,
}

impl ProxyPool {
    fn new() -> Self {
        // 初始只有直连——用户配了 proxy 后再注入
        Self {
            entries: RwLock::new(vec![ProxyEntry::new(None)]),
        }
    }

    /// 全量替换代理列表。直连始终保留（在最后位置兜底）。
    pub fn set_urls(&self, urls: Vec<String>) {
        let mut entries: Vec<ProxyEntry> = urls
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| ProxyEntry::new(Some(s.trim().to_string())))
            .collect();
        entries.push(ProxyEntry::new(None)); // 直连兜底
        if let Ok(mut g) = self.entries.write() {
            *g = entries;
        }
    }

    /// 当前配置的代理 URL 列表（不含直连）——前端 Settings 回显用。
    pub fn list_urls(&self) -> Vec<String> {
        self.entries
            .read()
            .map(|g| g.iter().filter_map(|e| e.url.clone()).collect())
            .unwrap_or_default()
    }

    /// 选一组待尝试的 entry（按 health 降序，跳过拉黑的）。
    /// 返回 (url-option, idx) 列表，idx 是 entries 数组的下标——`report()` 用。
    pub fn ordered_attempts(&self) -> Vec<(Option<String>, usize)> {
        let g = match self.entries.read() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let now = Instant::now();
        let mut active: Vec<(usize, &ProxyEntry)> = g
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.is_blocked(now))
            .collect();
        active.sort_by(|a, b| {
            b.1.health
                .partial_cmp(&a.1.health)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        active
            .into_iter()
            .take(MAX_ATTEMPTS_PER_SOURCE)
            .map(|(i, e)| (e.url.clone(), i))
            .collect()
    }

    /// 回报一次结果——更新 EMA + 触发拉黑。
    pub fn report(&self, idx: usize, ok: bool) {
        let mut g = match self.entries.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(entry) = g.get_mut(idx) {
            let sample = if ok { 1.0 } else { 0.0 };
            entry.health = HEALTH_ALPHA * sample + (1.0 - HEALTH_ALPHA) * entry.health;
            if entry.health < BLOCK_THRESHOLD && entry.url.is_some() {
                // 直连不拉黑（最后兜底）
                entry.blocked_until = Some(Instant::now() + BLOCK_DURATION);
                tracing::warn!(
                    proxy = ?entry.url,
                    health = entry.health,
                    "代理拉黑 5min"
                );
            }
            if ok && entry.blocked_until.is_some() {
                entry.blocked_until = None; // 提前恢复
            }
        }
    }

    /// 健康度快照——前端 UI 展示用。
    pub fn snapshot(&self) -> Vec<ProxyHealthSnapshot> {
        let g = match self.entries.read() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let now = Instant::now();
        g.iter()
            .map(|e| ProxyHealthSnapshot {
                label: e.label(),
                health: e.health,
                blocked: e.is_blocked(now),
                blocked_remaining_secs: e
                    .blocked_until
                    .map(|t| t.saturating_duration_since(now).as_secs())
                    .unwrap_or(0),
            })
            .collect()
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyHealthSnapshot {
    pub label: String,
    pub health: f64,
    pub blocked: bool,
    pub blocked_remaining_secs: u64,
}

// ============================================================================
// 全局单例
// ============================================================================

static POOL: OnceLock<ProxyPool> = OnceLock::new();

pub fn pool() -> &'static ProxyPool {
    POOL.get_or_init(ProxyPool::new)
}

// ============================================================================
// 持久化（KV）
// ============================================================================
//
// 用户在 Settings 配的 proxy list 存到 app_state[KEY_PROXY_LIST]，
// 进程启动时 `hydrate` 灌回 pool。

pub const KEY_PROXY_LIST: &str = "gangzi-terminal.realtime-proxies";

pub fn hydrate(app: &tauri::AppHandle) {
    if let Ok(Some(val)) = crate::db::load_app_state_value(app, KEY_PROXY_LIST) {
        if let Some(arr) = val.as_array() {
            let urls: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            pool().set_urls(urls);
            tracing::info!(count = pool().list_urls().len(), "代理池 hydrate 完成");
        }
    }
}

pub fn persist(app: &tauri::AppHandle, urls: &[String]) -> Result<(), String> {
    let val = serde_json::Value::Array(
        urls.iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect(),
    );
    crate::db::save_app_state_value(app, KEY_PROXY_LIST, &val)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_has_direct_only() {
        let p = ProxyPool::new();
        let attempts = p.ordered_attempts();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].0, None);
    }

    #[test]
    fn set_urls_keeps_direct_fallback() {
        let p = ProxyPool::new();
        p.set_urls(vec![
            "http://1.2.3.4:8080".into(),
            "socks5://127.0.0.1:7890".into(),
        ]);
        let attempts = p.ordered_attempts();
        assert_eq!(attempts.len(), 3);
        // 直连在 entries 末尾，但 ordered 按 health 排，初始全 1.0 时按 idx 排，所以直连最后
        assert_eq!(attempts[2].0, None);
    }

    #[test]
    fn report_failure_drops_health() {
        let p = ProxyPool::new();
        p.set_urls(vec!["http://1.2.3.4:8080".into()]);
        // 模拟连续失败 10 次
        for _ in 0..10 {
            p.report(0, false);
        }
        let snap = p.snapshot();
        assert!(snap[0].health < 0.2);
        assert!(snap[0].blocked);
    }

    #[test]
    fn direct_is_never_blocked() {
        let p = ProxyPool::new();
        // 直连只有一个 entry，idx 0
        for _ in 0..100 {
            p.report(0, false);
        }
        let snap = p.snapshot();
        assert!(!snap[0].blocked);
    }

    #[test]
    fn success_unblocks_quickly() {
        let p = ProxyPool::new();
        p.set_urls(vec!["http://1.2.3.4:8080".into()]);
        for _ in 0..10 {
            p.report(0, false);
        }
        assert!(p.snapshot()[0].blocked);
        p.report(0, true);
        assert!(!p.snapshot()[0].blocked);
    }
}
