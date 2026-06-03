//! Provider 配置（token / timeout / 默认参数）。
//!
//! Spec: docs/design/quotes-module.md §5（provider 策略）
//! Spec: docs/design/references/quotes/tushare.md（token 缺失时跳过）

#[derive(Debug, Clone)]
pub struct QuotesConfig {
    /// TuShare Pro token；缺失时 TuShare adapter 不参与 refresh。
    pub tushare_token: Option<String>,
}

impl Default for QuotesConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

impl QuotesConfig {
    pub fn from_env() -> Self {
        let tushare_token = std::env::var("TUSHARE_TOKEN").ok().filter(|s| !s.is_empty());
        Self { tushare_token }
    }
}
