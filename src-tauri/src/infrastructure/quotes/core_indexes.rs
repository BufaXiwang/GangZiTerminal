//! 4 大核心指数——UI 总览 + KlineChart 默认选中，始终订阅。
//!
//! `pipeline::market::refresh` 每 tick 合并 `account::subscribed_codes()` + 本列表 → 喂 dispatch。

pub const CORE_INDEXES: &[&str] = &[
    "000001.SH", // 上证综指
    "399001.SZ", // 深证成指
    "399006.SZ", // 创业板指
    "000300.SH", // 沪深 300
];

pub fn list() -> Vec<String> {
    CORE_INDEXES.iter().map(|s| s.to_string()).collect()
}
