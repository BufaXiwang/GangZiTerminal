//! Quotes 子域的 I/O 实现层。
//!
//! 包含 quotes domain 所有的外部 I/O 适配：
//! - `tushare/`     —— TuShare HTTP client + 接口（stock / index / fund / flow / events / concept / calendar / probe）
//! - `eastmoney/`   —— Eastmoney HTTP client + 实时报价 + 分钟/分时 K
//! - `cache/`       —— SQLite 持久化（kline_cache 等）
//! - `snapshot/`    —— in-memory market snapshot（watchlist 属于 account，quotes 只消费订阅集合）
//!
//! 未来扩展（其它 bounded context）：
//! - `infrastructure/news/`     —— 资讯子域 I/O
//! - `infrastructure/account/`  —— 模拟账户子域 I/O

pub mod cache;
pub mod core_indexes;
pub mod eastmoney;
pub mod realtime;
pub mod repository;
pub mod scanner;
pub mod snapshot;
pub mod tdx;
pub mod tushare;
