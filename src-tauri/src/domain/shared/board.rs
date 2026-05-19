//! A 股板块识别 + 涨跌停规则。
//!
//! A 股不同板块的涨跌停幅度差异巨大，统一阈值 9.9% 会把创业板 / 科创板 / 北交所的
//! 正常波动错判为涨跌停。本模块按 `StockCode` 前缀 + 股票名称（识别 ST）推断板块，
//! 给 signal_detector 提供正确的 limit_up / limit_down 触发阈值。
//!
//! 规则来源（截至 2024）：
//! - 主板（沪 600/601/603/605/688 之外、深 000/001/002）：±10%
//! - 创业板（深 300/301）：±20%（2020-08-24 改革后）
//! - 科创板（沪 688/689）：±20%
//! - 北交所（4xxxxx / 8xxxxx / 92xxxx）：±30%
//! - ST / *ST（任何板块；名称含 "ST"）：±5%（北交所 ST 也是 5%）
//!
//! 不处理"新股上市前 5 交易日无限制"——需要 listing_date，目前数据流没接进来。
//! 这是已知简化，对训练目的影响微弱。

use crate::domain::shared::StockCode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Board {
    /// 主板（沪 600/601/603/605；深 000/001/002）
    MainBoard,
    /// 创业板（300/301）
    ChiNext,
    /// 科创板（688/689）
    Star,
    /// 北交所（4/8/92*）
    Beijing,
    /// ST / *ST（任意板块，名字含 ST）
    St,
}

impl Board {
    /// 涨跌停百分点（绝对值，10.0 = ±10%）。
    pub fn price_limit_pct(self) -> f64 {
        match self {
            Self::MainBoard => 10.0,
            Self::ChiNext | Self::Star => 20.0,
            Self::Beijing => 30.0,
            Self::St => 5.0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MainBoard => "main_board",
            Self::ChiNext => "chinext",
            Self::Star => "star",
            Self::Beijing => "beijing",
            Self::St => "st",
        }
    }
}

/// 根据 6 位 code + 股票名推断板块。名称为空时退化为只看 code 前缀。
///
/// ST 判定严格化：去空格后名称必须以 "ST"、"*ST" 或 "SST"（"S*ST"去 * 后）开头，
/// 防止 "STAR XX"/"FIRST XX" 这类含 ST 子串的非 ST 股被误判。
pub fn classify(code: &StockCode, name: &str) -> Board {
    if is_st_name(name) {
        return Board::St;
    }
    let s = code.as_str();
    if s.starts_with("688") || s.starts_with("689") {
        Board::Star
    } else if s.starts_with("300") || s.starts_with("301") {
        Board::ChiNext
    } else if s.starts_with('4') || s.starts_with('8') || s.starts_with("92") {
        Board::Beijing
    } else {
        // 沪 600/601/603/605 / 深 000/001/002
        Board::MainBoard
    }
}

fn is_st_name(name: &str) -> bool {
    // 去掉空白 + 大写——A 股 ST 形态：ST/*ST/SST/S*ST/N*ST（XR/XD 不是 ST）
    let stripped: String = name
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase();
    // 找 ST 标记长度（按更长前缀优先匹配，避免 S*ST 被先匹配成 S）
    let marker_len: Option<usize> = if stripped.starts_with("S*ST") {
        Some(4)
    } else if stripped.starts_with("*ST") {
        Some(3)
    } else if stripped.starts_with("SST") {
        Some(3)
    } else if stripped.starts_with("ST") {
        Some(2)
    } else {
        None
    };
    let Some(idx) = marker_len else { return false };
    // ST 标记后必须是字符串结束或非字母数字字符（中文/标点/空字符）
    // —— 防止 "STAR XX" / "FIRST XX" 被误判
    let rest = &stripped[idx..];
    match rest.chars().next() {
        None => true,
        Some(c) => !c.is_ascii_alphanumeric(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(s: &str) -> StockCode {
        StockCode::new(s).unwrap()
    }

    #[test]
    fn main_board_codes() {
        assert_eq!(classify(&code("600519"), "贵州茅台"), Board::MainBoard);
        assert_eq!(classify(&code("000001"), "平安银行"), Board::MainBoard);
        assert_eq!(classify(&code("002594"), "比亚迪"), Board::MainBoard);
    }

    #[test]
    fn chinext_codes() {
        assert_eq!(classify(&code("300750"), "宁德时代"), Board::ChiNext);
        assert_eq!(classify(&code("301308"), "江波龙"), Board::ChiNext);
    }

    #[test]
    fn star_codes() {
        assert_eq!(classify(&code("688981"), "中芯国际"), Board::Star);
    }

    #[test]
    fn beijing_codes() {
        assert_eq!(classify(&code("832149"), "翰博高新"), Board::Beijing);
        assert_eq!(classify(&code("430564"), "天能股份"), Board::Beijing);
        assert_eq!(classify(&code("920469"), "新北交"), Board::Beijing);
    }

    #[test]
    fn st_overrides_board() {
        // ST 优先于板块前缀——ST 主板股、ST 创业板股都按 ±5%
        assert_eq!(classify(&code("600519"), "*ST 茅台"), Board::St);
        assert_eq!(classify(&code("300750"), "ST 宁德"), Board::St);
        assert_eq!(classify(&code("688981"), "ST 中芯"), Board::St);
        assert_eq!(classify(&code("600519"), "SST 老股"), Board::St);
        assert_eq!(classify(&code("600519"), "S*ST 老股"), Board::St);
    }

    #[test]
    fn st_substring_does_not_trigger() {
        // 子串含 "ST" 但非 ST 股不能误判
        assert_eq!(classify(&code("688981"), "STAR XX"), Board::Star);
        assert_eq!(classify(&code("600519"), "FIRST 银行"), Board::MainBoard);
        assert_eq!(classify(&code("000001"), "POST 银行"), Board::MainBoard);
    }

    #[test]
    fn limits() {
        assert_eq!(Board::MainBoard.price_limit_pct(), 10.0);
        assert_eq!(Board::ChiNext.price_limit_pct(), 20.0);
        assert_eq!(Board::Star.price_limit_pct(), 20.0);
        assert_eq!(Board::Beijing.price_limit_pct(), 30.0);
        assert_eq!(Board::St.price_limit_pct(), 5.0);
    }
}
