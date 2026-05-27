//! Provider 配置（token / timeout / 默认参数）。
//!
//! Spec: docs/design/quotes-module.md §5（provider 策略）
//! Spec: docs/design/references/quotes/tushare.md（token 缺失时跳过）

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct QuotesConfig {
    /// TuShare Pro token；缺失时 TuShare adapter 不参与 refresh。
    pub tushare_token: Option<String>,
    /// 全市场 quote refresh tick 间隔（spec §5：universe 60s 默认）。
    pub universe_refresh_interval: Duration,
    /// 关注 + 核心指数 quote refresh tick 间隔（15s）。
    pub subscribed_refresh_interval: Duration,
    /// 启动 universe bootstrap 是否立即执行（默认 true；测试可关）。
    pub bootstrap_on_start: bool,
}

impl Default for QuotesConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

impl QuotesConfig {
    pub fn from_env() -> Self {
        let tushare_token = std::env::var("TUSHARE_TOKEN").ok().filter(|s| !s.is_empty());
        Self {
            tushare_token,
            universe_refresh_interval: Duration::from_secs(60),
            subscribed_refresh_interval: Duration::from_secs(15),
            bootstrap_on_start: true,
        }
    }
}
