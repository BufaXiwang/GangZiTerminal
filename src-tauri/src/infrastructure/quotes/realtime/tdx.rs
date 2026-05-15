//! 通达信 (TDX) 实时报价 source —— 包装内部 [`crate::infrastructure::quotes::tdx`] 模块。
//!
//! 参考：<https://github.com/mootdx/mootdx>（pytdx wire 兼容）
//!
//! ## 工作方式
//!
//! - 维护一个共享 [`TdxHqClient`]（同步 TCP），首次调用时 `connect_bestip()` 竞速建连
//! - 每次 `fetch` 用 `tokio::task::spawn_blocking` 包装同步调用，不阻塞 tokio runtime
//! - 任一请求失败 → 丢弃 client → 下次重连
//!
//! ## 限制
//!
//! - **不支持北交所**（mootdx `Market` 只有 SZ/SH）：任一 BJ 代码出现 → fail fast →
//!   让 dispatch fallback 整个 batch 给 EM / 腾讯 / 新浪
//! - SecurityQuote 不含 name → 返 `name=""`；caller 按需查 stocks 表补名

use super::{split_ts_code, RealtimeQuoteSource};
use crate::domain::quotes::{OrderBookLevel, QuotesError, StockQuote};
use crate::domain::shared::{Lots, OccurredAt, StockCode, Yuan};
use crate::infrastructure::quotes::tdx::client::TdxHqClient;
use crate::infrastructure::quotes::tdx::types::{Market, SecurityQuote};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct TdxSource {
    /// 共享一个 TCP 客户端——TDX 协议每个连接顺序请求即可；首失败丢弃重连
    client: Arc<Mutex<Option<TdxHqClient>>>,
}

impl TdxSource {
    pub fn new() -> Self {
        Self {
            client: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait]
impl RealtimeQuoteSource for TdxSource {
    fn name(&self) -> &'static str {
        "tdx"
    }

    fn batch_limit(&self) -> usize {
        80
    }

    async fn fetch(&self, ts_codes: &[String]) -> Result<Vec<(String, StockQuote)>, QuotesError> {
        if ts_codes.is_empty() {
            return Ok(Vec::new());
        }

        // 1. 解析 ts_code → (Market, code, original_ts)
        //    BJ **静默跳过**（dispatch 会用下一源补 BJ 的缺失），不让 BJ 拖累 SH/SZ
        let mut stocks: Vec<(Market, String, String)> = Vec::with_capacity(ts_codes.len());
        for ts in ts_codes {
            let (prefix, code) = match split_ts_code(ts) {
                Some(parts) => parts,
                None => continue, // 非法 ts_code 跳过
            };
            let market = match prefix {
                "sh" => Market::SH,
                "sz" => Market::SZ,
                "bj" => continue, // BJ 不支持，跳过让 dispatch 用 EM 补
                _ => continue,
            };
            stocks.push((market, code.to_string(), ts.clone()));
        }
        if stocks.is_empty() {
            // 整个 batch 都是 BJ / 非法——返回空让 dispatch fallback
            return Ok(Vec::new());
        }

        // 2. spawn_blocking 调同步 TCP client
        let client_arc = self.client.clone();
        let stocks_for_call = stocks.clone();
        let raw_quotes =
            tokio::task::spawn_blocking(move || -> Result<Vec<SecurityQuote>, String> {
                let mut guard = client_arc.blocking_lock();

                // 确保连接
                if guard.is_none() {
                    let (client, addr) = TdxHqClient::connect_bestip(CONNECT_TIMEOUT)
                        .map_err(|e| format!("tdx 建连失败: {e}"))?;
                    tracing::info!(peer = %addr, "tdx HQ 连接建立");
                    *guard = Some(client);
                }

                let client = guard.as_mut().expect("just inited above");
                let pairs: Vec<(Market, &str)> = stocks_for_call
                    .iter()
                    .map(|(m, c, _)| (*m, c.as_str()))
                    .collect();

                match client.security_quotes(&pairs) {
                    Ok(qs) => Ok(qs),
                    Err(e) => {
                        // 失败：丢弃 client，下次重连
                        *guard = None;
                        Err(format!("security_quotes 失败: {e}"))
                    }
                }
            })
            .await
            .map_err(|e| QuotesError::Network(format!("tdx blocking task join: {e}")))?
            .map_err(QuotesError::Network)?;

        // 3. SecurityQuote → StockQuote 映射
        //    SecurityQuote 没 name 字段；返空让上层（service / valuation）按需查 stocks 表
        let mut ts_lookup: HashMap<(u8, String), String> = HashMap::with_capacity(stocks.len());
        for (m, c, ts) in &stocks {
            ts_lookup.insert((m.as_u8(), c.clone()), ts.clone());
        }

        let mut result: Vec<(String, StockQuote)> = Vec::with_capacity(raw_quotes.len());
        for q in raw_quotes {
            let key = (q.market, q.code.clone());
            let ts_code = match ts_lookup.get(&key) {
                Some(ts) => ts.clone(),
                None => continue, // 返了未知 market/code，跳过
            };
            let code = match StockCode::new(&q.code) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // change / change_percent 从 last_close 派生
            let (change, change_percent) = if q.last_close > 0.0 {
                let c = q.price - q.last_close;
                let pct = c / q.last_close * 100.0;
                (Some(Yuan::from_unchecked(c)), Some(pct))
            } else {
                (None, None)
            };

            // 价/昨收都 ≤ 0 通常是停牌或无效数据 → 跳过
            if q.price <= 0.0 && q.last_close <= 0.0 {
                continue;
            }

            result.push((
                ts_code,
                StockQuote {
                    code,
                    name: String::new(), // TDX 不返 name
                    price: if q.price > 0.0 {
                        Some(Yuan::from_unchecked(q.price))
                    } else {
                        None
                    },
                    change_percent,
                    change,
                    open: if q.open > 0.0 {
                        Some(Yuan::from_unchecked(q.open))
                    } else {
                        None
                    },
                    high: if q.high > 0.0 {
                        Some(Yuan::from_unchecked(q.high))
                    } else {
                        None
                    },
                    low: if q.low > 0.0 {
                        Some(Yuan::from_unchecked(q.low))
                    } else {
                        None
                    },
                    previous_close: if q.last_close > 0.0 {
                        Some(Yuan::from_unchecked(q.last_close))
                    } else {
                        None
                    },
                    // TDX vol 单位是手（与我们的 Lots 一致）
                    day_volume: Some(Lots::from_unchecked(q.vol as i64)),
                    day_amount: Some(Yuan::from_unchecked(q.amount)),
                    captured_at: OccurredAt::now(),
                    bid_levels: q
                        .book
                        .iter()
                        .map(|l| OrderBookLevel {
                            price: positive_yuan(l.bid),
                            volume: positive_lots(l.bid_vol),
                        })
                        .collect(),
                    ask_levels: q
                        .book
                        .iter()
                        .map(|l| OrderBookLevel {
                            price: positive_yuan(l.ask),
                            volume: positive_lots(l.ask_vol),
                        })
                        .collect(),
                    buy_volume: positive_lots(q.b_vol),
                    sell_volume: positive_lots(q.s_vol),
                    order_imbalance: order_imbalance(&q),
                },
            ));
        }

        Ok(result)
    }
}

fn positive_yuan(v: f64) -> Option<Yuan> {
    (v.is_finite() && v > 0.0).then(|| Yuan::from_unchecked(v))
}

fn positive_lots(v: f64) -> Option<Lots> {
    (v.is_finite() && v > 0.0).then(|| Lots::from_unchecked(v as i64))
}

fn order_imbalance(q: &SecurityQuote) -> Option<f64> {
    let bid: f64 = q.book.iter().map(|l| l.bid_vol.max(0.0)).sum();
    let ask: f64 = q.book.iter().map(|l| l.ask_vol.max(0.0)).sum();
    let denom = bid + ask;
    (denom > 0.0).then(|| (bid - ask) / denom * 100.0)
}
