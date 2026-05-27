//! 核心指数集合 + 其他模块默认 source 常量。
//!
//! Spec: docs/design/quotes-module.md §5 (核心指数集合)

use crate::domain::shared::TsCode;

/// Spec: quotes-module.md §5
///
/// 默认 headline 核心指数集合。外部调度通过 `core_indexes()` 合并 refresh scope，
/// 不内嵌指数列表。
pub fn core_indexes() -> Vec<TsCode> {
    vec![
        TsCode::parse("000001.SH").unwrap(),
        TsCode::parse("399001.SZ").unwrap(),
        TsCode::parse("399006.SZ").unwrap(),
        TsCode::parse("000300.SH").unwrap(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_indexes_are_four() {
        assert_eq!(core_indexes().len(), 4);
    }
}
