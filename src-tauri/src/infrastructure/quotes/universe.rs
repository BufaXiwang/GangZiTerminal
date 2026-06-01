//! Universe 分类纯函数：把 6 位 code 前缀映射到 (category, board)。
//!
//! Spec: docs/design/quotes-module.md §2 / §5
//!
//! - SH 主板: 600/601/603/605；科创板: 688/689；ETF/fund: 51/56/58；
//!   index: 000/100/110/120/130/180/999
//! - SZ 主板/中小: 000/001/002/003/004；创业板: 300/301；
//!   ETF/fund: 159；index: 399

use crate::domain::shared::{InstrumentCategory, Market};

/// 单条分类结果：`(category, optional board)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UniverseClass {
    pub category: InstrumentCategory,
    pub board: Option<&'static str>,
}

/// 按 6 位 code 推 SH universe entry 的 (category, board)。
///
/// 未知前缀返回 None — 调用方按 spec 跳过或标记。
pub fn classify_sh(code6: &str) -> Option<UniverseClass> {
    if code6.len() < 3 {
        return None;
    }
    let p2 = &code6[..2];
    let p3 = &code6[..3];
    let cls = match p3 {
        "600" | "601" | "603" | "605" => UniverseClass {
            category: InstrumentCategory::Stock,
            board: Some("主板"),
        },
        "688" | "689" => UniverseClass {
            category: InstrumentCategory::Stock,
            board: Some("科创板"),
        },
        _ => match p2 {
            "51" | "56" | "58" => UniverseClass {
                category: InstrumentCategory::Fund,
                board: None,
            },
            // SH 指数只有 000xxx（综指/上证50/沪深300/中证500/科创50…）+ TDX 合成 999xxx。
            // 之前还含 100/110/120/130/180——这些其实是**债券**（100=国债、110=可转债、120=企业债、
            // 130=企债/回购），会被错当指数塞进 universe，且报价 10× 错（3 位小数被当 Index 2 位）。
            // universe 只收股票/指数/场内基金（spec §2/§5），债券一律丢弃。
            _ => match p3 {
                "000" | "999" => UniverseClass {
                    category: InstrumentCategory::Index,
                    board: None,
                },
                _ => return None,
            },
        },
    };
    Some(cls)
}

/// 按 6 位 code 推 SZ universe entry 的 (category, board)。
pub fn classify_sz(code6: &str) -> Option<UniverseClass> {
    if code6.len() < 3 {
        return None;
    }
    let p3 = &code6[..3];
    let cls = match p3 {
        "000" | "001" | "002" | "003" | "004" => UniverseClass {
            category: InstrumentCategory::Stock,
            board: Some("主板"),
        },
        "300" | "301" => UniverseClass {
            category: InstrumentCategory::Stock,
            board: Some("创业板"),
        },
        "159" => UniverseClass {
            category: InstrumentCategory::Fund,
            board: None,
        },
        "399" => UniverseClass {
            category: InstrumentCategory::Index,
            board: None,
        },
        _ => return None,
    };
    Some(cls)
}

/// 按 market 派发到对应分类函数。BJ 不通过 TDX 入口，直接 stock 主板。
pub fn classify(market: Market, code6: &str) -> Option<UniverseClass> {
    match market {
        Market::SH => classify_sh(code6),
        Market::SZ => classify_sz(code6),
        Market::BJ => Some(UniverseClass {
            category: InstrumentCategory::Stock,
            board: Some("主板"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sh_main_board_recognised() {
        let r = classify_sh("600519").unwrap();
        assert_eq!(r.category, InstrumentCategory::Stock);
        assert_eq!(r.board, Some("主板"));
        assert_eq!(classify_sh("601318").unwrap().board, Some("主板"));
        assert_eq!(classify_sh("603259").unwrap().board, Some("主板"));
        assert_eq!(classify_sh("605588").unwrap().board, Some("主板"));
    }

    #[test]
    fn sh_star_recognised() {
        let r = classify_sh("688981").unwrap();
        assert_eq!(r.category, InstrumentCategory::Stock);
        assert_eq!(r.board, Some("科创板"));
    }

    #[test]
    fn sh_fund_recognised() {
        let r = classify_sh("510300").unwrap();
        assert_eq!(r.category, InstrumentCategory::Fund);
        let r2 = classify_sh("588000").unwrap();
        assert_eq!(r2.category, InstrumentCategory::Fund);
    }

    #[test]
    fn sh_index_recognised() {
        let r = classify_sh("000001").unwrap();
        assert_eq!(r.category, InstrumentCategory::Index);
        let r2 = classify_sh("999999").unwrap();
        assert_eq!(r2.category, InstrumentCategory::Index);
    }

    #[test]
    fn sh_unknown_returns_none() {
        assert!(classify_sh("888888").is_none());
    }

    // 回归：SH 债券前缀（国债 100 / 可转债 110 / 企业债 120 / 130 / 180）不是指数，必须丢弃，
    // 不能进 universe（spec §2/§5：universe 只收股票/指数/场内基金）。
    #[test]
    fn sh_bond_prefixes_dropped_not_index() {
        for code in ["100303", "110059", "113537", "120201", "130001", "180101"] {
            assert!(
                classify_sh(code).is_none(),
                "{code} 是债券，应被丢弃而非分类为指数"
            );
        }
        // 真指数仍保留。
        assert_eq!(classify_sh("000001").unwrap().category, InstrumentCategory::Index);
        assert_eq!(classify_sh("000300").unwrap().category, InstrumentCategory::Index);
        assert_eq!(classify_sh("999999").unwrap().category, InstrumentCategory::Index);
    }

    #[test]
    fn sz_main_board_recognised() {
        let r = classify_sz("000001").unwrap();
        assert_eq!(r.category, InstrumentCategory::Stock);
        assert_eq!(r.board, Some("主板"));
        assert_eq!(classify_sz("002230").unwrap().board, Some("主板"));
    }

    #[test]
    fn sz_chinext_recognised() {
        let r = classify_sz("300750").unwrap();
        assert_eq!(r.category, InstrumentCategory::Stock);
        assert_eq!(r.board, Some("创业板"));
        assert_eq!(classify_sz("301000").unwrap().board, Some("创业板"));
    }

    #[test]
    fn sz_etf_recognised() {
        let r = classify_sz("159915").unwrap();
        assert_eq!(r.category, InstrumentCategory::Fund);
    }

    #[test]
    fn sz_index_recognised() {
        let r = classify_sz("399006").unwrap();
        assert_eq!(r.category, InstrumentCategory::Index);
    }

    #[test]
    fn sz_unknown_returns_none() {
        assert!(classify_sz("888888").is_none());
    }

    #[test]
    fn bj_always_stock_main() {
        let r = classify(Market::BJ, "430047").unwrap();
        assert_eq!(r.category, InstrumentCategory::Stock);
        assert_eq!(r.board, Some("主板"));
    }
}
