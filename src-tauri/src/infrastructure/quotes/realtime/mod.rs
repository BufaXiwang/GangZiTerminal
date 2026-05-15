//! 实时报价多源调度——**递增填充** + 健康度排序。
//!
//! 设计：
//!
//! ```
//! ┌─────────────────────────────────────────────┐
//! │ DispatchSource                              │
//! │  fetch(ts_codes):                           │
//! │    order = sort_by_health([TDX, EM, 腾讯, 新浪]) │
//! │    pending = ts_codes                       │
//! │    for src in order:                        │
//! │      ask src.fetch(pending) → 收到的从 pending 移除 │
//! │      pending 空 → 返回                       │
//! │    返回累计结果                              │
//! ├─────────────────────────────────────────────┤
//! │ trait RealtimeQuoteSource                   │
//! ├─────────────────────────────────────────────┤
//! │ TdxSource / EmSource / TencentSource / SinaSource │
//! └─────────────────────────────────────────────┘
//! ```
//!
//! - **TDX 主路径**：抗 IP 风控最强；处理 SH/SZ
//! - **EM 补 BJ 缺失**：北交所 + TDX 临时故障兜底
//! - **腾讯 / 新浪**：极端兜底
//!
//! 健康度：
//! - 每次拿到 ≥1 条 → EMA 加权 1
//! - 全没拿到且报错 → EMA 加权 0
//! - α = 0.3，连续失败被排到列表末尾
//!
//! 上层只用 `dispatch()` 全局单例，单一入口。

use crate::domain::quotes::{QuotesError, StockQuote};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};

pub mod client_cache;
pub mod em;
pub mod proxy_pool;
pub mod sina;
pub mod tdx;
pub mod tdx_pool;
pub mod tencent;

// ============================================================================
// trait + 调度
// ============================================================================

#[async_trait]
pub trait RealtimeQuoteSource: Send + Sync {
    /// 源名——"em" / "tencent" / "sina"。用于健康度跟踪 + 日志。
    fn name(&self) -> &'static str;

    /// 单次请求批量上限——超过会被 DispatchSource 拆批。
    fn batch_limit(&self) -> usize;

    /// 拉取实时报价。返回 `(ts_code, StockQuote)`，ts_code 形如 "000001.SZ"。
    async fn fetch(&self, ts_codes: &[String]) -> Result<Vec<(String, StockQuote)>, QuotesError>;
}

pub struct DispatchSource {
    sources: Vec<Arc<dyn RealtimeQuoteSource>>,
    /// 名字 → EMA 成功率 (0.0-1.0)，初始 1.0
    health: RwLock<HashMap<&'static str, f64>>,
}

const HEALTH_ALPHA: f64 = 0.3;

impl DispatchSource {
    /// 标准配置：TDX > EM > 腾讯 > 新浪。
    ///
    /// 期望分工：
    /// - **TDX 主路径**：16 公共 HQ 服务器分散 + 私有协议，抗 IP 风控最强；处理所有 SH/SZ
    /// - **EM fallback**：北交所标的（TDX 不支持）+ TDX 临时挂掉时补
    /// - **腾讯 / 新浪**：极端场景兜底
    pub fn standard() -> Self {
        Self {
            sources: vec![
                Arc::new(tdx::TdxSource::new()),
                Arc::new(em::EmSource::new()),
                Arc::new(tencent::TencentSource::new()),
                Arc::new(sina::SinaSource::new()),
            ],
            health: RwLock::new(HashMap::new()),
        }
    }

    /// 拉取——**递增填充**模式：每源只拿"还没补到"的代码，下一源补缺失。
    ///
    /// 优势：
    /// - TDX 返回 SH/SZ 子集 → BJ 自动落到 EM，不丢任何标的
    /// - 任一源临时失败 → 下一源继续补，不阻塞整个 batch
    /// - 健康度只惩罚"既没拿到任何数据又抛错"的源
    pub async fn fetch(
        &self,
        ts_codes: &[String],
    ) -> Result<Vec<(String, StockQuote)>, QuotesError> {
        if ts_codes.is_empty() {
            return Ok(Vec::new());
        }
        let order = self.order_by_health();
        let mut all_results: Vec<(String, StockQuote)> = Vec::with_capacity(ts_codes.len());
        let mut filled: HashSet<String> = HashSet::new();
        let mut last_err: Option<QuotesError> = None;

        for src in order {
            // 只问还没补到的代码
            let pending: Vec<String> = ts_codes
                .iter()
                .filter(|c| !filled.contains(c.as_str()))
                .cloned()
                .collect();
            if pending.is_empty() {
                break;
            }

            let limit = src.batch_limit();
            let mut src_got_any = false;
            let mut src_failed_any = false;

            for chunk in pending.chunks(limit) {
                match src.fetch(chunk).await {
                    Ok(items) => {
                        for (ts, q) in items {
                            if filled.insert(ts.clone()) {
                                all_results.push((ts, q));
                                src_got_any = true;
                            }
                        }
                    }
                    Err(e) => {
                        src_failed_any = true;
                        last_err = Some(e);
                    }
                }
            }

            // 健康度更新策略：拿到一条都算成功，全没拿到才算失败
            if src_got_any {
                self.record(src.name(), true);
                tracing::debug!(
                    source = src.name(),
                    filled = filled.len(),
                    pending_after = ts_codes.len().saturating_sub(filled.len()),
                    "实时报价补充"
                );
            } else if src_failed_any {
                self.record(src.name(), false);
                tracing::warn!(source = src.name(), "实时报价失败，尝试下一源");
            }
        }

        if all_results.is_empty() {
            return Err(last_err
                .unwrap_or_else(|| QuotesError::Network("所有实时报价源都未补到任何数据".into())));
        }
        if filled.len() < ts_codes.len() {
            tracing::debug!(
                requested = ts_codes.len(),
                got = filled.len(),
                "部分 ts_code 所有源都没拿到"
            );
        }
        Ok(all_results)
    }

    fn record(&self, name: &'static str, ok: bool) {
        if let Ok(mut g) = self.health.write() {
            let cur = g.get(name).copied().unwrap_or(1.0);
            let sample = if ok { 1.0 } else { 0.0 };
            let new_val = HEALTH_ALPHA * sample + (1.0 - HEALTH_ALPHA) * cur;
            g.insert(name, new_val);
        }
    }

    /// 当前各源健康度——观测 / Settings UI 展示用。
    pub fn health_snapshot(&self) -> Vec<(&'static str, f64)> {
        let g = match self.health.read() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        self.sources
            .iter()
            .map(|s| {
                let name = s.name();
                let score = g.get(name).copied().unwrap_or(1.0);
                (name, score)
            })
            .collect()
    }

    /// 按健康度降序——同分按原顺序（即默认 EM > 腾讯 > 新浪）。
    fn order_by_health(&self) -> Vec<Arc<dyn RealtimeQuoteSource>> {
        let h = self.health.read().ok();
        let mut indexed: Vec<(usize, f64)> = self
            .sources
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let score = h
                    .as_ref()
                    .and_then(|m| m.get(s.name()))
                    .copied()
                    .unwrap_or(1.0);
                (i, score)
            })
            .collect();
        indexed.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        indexed
            .into_iter()
            .map(|(i, _)| self.sources[i].clone())
            .collect()
    }
}

// ============================================================================
// 全局单例
// ============================================================================

static DISPATCH: OnceLock<DispatchSource> = OnceLock::new();

pub fn dispatch() -> &'static DispatchSource {
    DISPATCH.get_or_init(DispatchSource::standard)
}

// ============================================================================
// 共享工具——ts_code 形态转换
// ============================================================================

/// "000001.SZ" → ("sz", "000001")；非法返 None。
pub(crate) fn split_ts_code(ts_code: &str) -> Option<(&'static str, &str)> {
    if ts_code.len() != 9 || ts_code.as_bytes()[6] != b'.' {
        return None;
    }
    let code = &ts_code[..6];
    let prefix = match &ts_code[7..] {
        "SH" => "sh",
        "SZ" => "sz",
        "BJ" => "bj",
        _ => return None,
    };
    Some((prefix, code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_ts_code_works() {
        assert_eq!(split_ts_code("000001.SZ"), Some(("sz", "000001")));
        assert_eq!(split_ts_code("600519.SH"), Some(("sh", "600519")));
        assert_eq!(split_ts_code("920469.BJ"), Some(("bj", "920469")));
        assert_eq!(split_ts_code("000001"), None);
        assert_eq!(split_ts_code("000001.XX"), None);
    }
}
