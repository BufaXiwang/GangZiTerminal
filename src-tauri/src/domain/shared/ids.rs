//! Newtype IDs——编译期防 ID 混淆。
//!
//! 跨函数传递的 ID 都用这里的 newtype，**禁止**用 `String`。

use serde::{Deserialize, Serialize};

/// 6 位 A 股代码（含基金 / 指数）。
///
/// 构造时校验：必须 6 位纯数字。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StockCode(String);

impl StockCode {
    /// 构造——校验 6 位纯数字。
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.len() == 6 && s.chars().all(|c| c.is_ascii_digit()) {
            Ok(Self(s))
        } else {
            Err(IdError::BadStockCode(s))
        }
    }

    /// 不校验直接构造——仅供已验证场景用（如 DB row 反序列化）。
    pub(crate) fn new_unchecked(s: String) -> Self {
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 推断个股交易所前缀：sh / sz / bj。
    ///
    /// **个股语义**——000001 = 平安银行（.SZ），不是上证指数（.SH）。
    /// 指数 / 基金的 ts_code 应该直接用字符串（"000001.SH"），不要构造 StockCode 后调这个。
    ///
    /// 北交所代码段：4xxxxx / 8xxxxx（老段）+ 92xxxx（2023+ 新段）。
    pub fn market_prefix(&self) -> &'static str {
        let s = self.0.as_str();
        if s.starts_with('6') {
            "sh" // 沪市主板 + 科创板（688）
        } else if s.starts_with('4') || s.starts_with('8') || s.starts_with("92") {
            "bj" // 北交所（老段 4/8 + 新段 92）
        } else {
            "sz" // 深市主板（00）+ 创业板（30）
        }
    }

    /// 转 TuShare 风格的 ts_code（带后缀）。"601899" → "601899.SH"。
    pub fn to_ts_code(&self) -> String {
        let suffix = match self.market_prefix() {
            "sh" => "SH",
            "sz" => "SZ",
            "bj" => "BJ",
            _ => unreachable!(),
        };
        format!("{}.{}", self.0, suffix)
    }

    /// 转 Eastmoney secid。市场号 1=沪 / 0=深 / 2=北。"601899" → "1.601899"。
    pub fn to_em_secid(&self) -> String {
        let market = match self.market_prefix() {
            "sh" => "1",
            "sz" => "0",
            "bj" => "2",
            _ => unreachable!(),
        };
        format!("{}.{}", market, self.0)
    }
}

impl std::fmt::Display for StockCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// TsCode — TuShare 全限定代码（"000001.SZ" / "510300.SH" / "920469.BJ"）
// ============================================================================
//
// 区别于 `StockCode`（仅 6 位数字）：TsCode 自带市场后缀，唯一标识标的，
// **不需要任何"猜测"**——后缀直接告诉我们沪/深/北。
//
// 类型边界设计：
// - 前端 invoke / 后端 adapter / kline_cache / 各 fetch_* 接口：用 TsCode
// - StockCode 仅用于"半边数据"（来自 6 位 user input / EM ulist 返回的 f12 等）
// - 6 位 → ts_code 时**必须**借助 DB（stocks/indexes/funds 表的 market 字段），
//   不能用前缀猜（监管会发新代码段，92xxx 已踩过坑）

/// "000001.SZ" 这种带后缀的 TuShare 全限定代码。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TsCode(String);

impl TsCode {
    /// 构造——校验格式 `\d{6}\.(SH|SZ|BJ)`。
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.len() != 9 {
            return Err(IdError::BadTsCode(s));
        }
        let bytes = s.as_bytes();
        if !bytes[..6].iter().all(|b| b.is_ascii_digit()) {
            return Err(IdError::BadTsCode(s));
        }
        if bytes[6] != b'.' {
            return Err(IdError::BadTsCode(s));
        }
        let suffix = &s[7..];
        if !matches!(suffix, "SH" | "SZ" | "BJ") {
            return Err(IdError::BadTsCode(s));
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 6 位 code 部分（"000001.SZ" → "000001"）。
    pub fn code(&self) -> &str {
        &self.0[..6]
    }

    /// 市场后缀（"SH" / "SZ" / "BJ"）。
    pub fn market(&self) -> &str {
        &self.0[7..]
    }

    /// 转 EM secid。"000001.SZ" → "0.000001"；"600519.SH" → "1.600519"；"920469.BJ" → "2.920469"。
    pub fn to_em_secid(&self) -> String {
        let m = match self.market() {
            "SH" => "1",
            "SZ" => "0",
            "BJ" => "2",
            _ => unreachable!("constructor validates market"),
        };
        format!("{m}.{}", self.code())
    }
}

impl std::fmt::Display for TsCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ===== 错误 ===============================================================

#[derive(Debug, Clone, thiserror::Error)]
pub enum IdError {
    #[error("非法 A 股代码：{0}（需要 6 位数字）")]
    BadStockCode(String),
    #[error("非法 ts_code：{0}（需要 \\d{{6}}\\.(SH|SZ|BJ)）")]
    BadTsCode(String),
}

// ===== 测试 ===============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stock_code_validation() {
        assert!(StockCode::new("601899").is_ok());
        assert!(StockCode::new("000001").is_ok());
        assert!(StockCode::new("832149").is_ok());
        assert!(StockCode::new("61899").is_err()); // 5 位
        assert!(StockCode::new("60189x").is_err()); // 含字母
        assert!(StockCode::new("").is_err());
    }

    #[test]
    fn market_prefix_resolution() {
        assert_eq!(StockCode::new("601899").unwrap().market_prefix(), "sh");
        assert_eq!(StockCode::new("000002").unwrap().market_prefix(), "sz");
        assert_eq!(StockCode::new("000001").unwrap().market_prefix(), "sz"); // 个股语义=平安银行
        assert_eq!(StockCode::new("300750").unwrap().market_prefix(), "sz");
        assert_eq!(StockCode::new("688981").unwrap().market_prefix(), "sh");
        assert_eq!(StockCode::new("832149").unwrap().market_prefix(), "bj");
        assert_eq!(StockCode::new("430564").unwrap().market_prefix(), "bj");
        assert_eq!(StockCode::new("920469").unwrap().market_prefix(), "bj"); // 北交所新段
    }

    #[test]
    fn ts_code_conversion() {
        assert_eq!(StockCode::new("601899").unwrap().to_ts_code(), "601899.SH");
        assert_eq!(StockCode::new("000002").unwrap().to_ts_code(), "000002.SZ");
        assert_eq!(StockCode::new("832149").unwrap().to_ts_code(), "832149.BJ");
    }

    #[test]
    fn em_secid_conversion() {
        assert_eq!(StockCode::new("601899").unwrap().to_em_secid(), "1.601899");
        assert_eq!(StockCode::new("000002").unwrap().to_em_secid(), "0.000002");
        assert_eq!(StockCode::new("832149").unwrap().to_em_secid(), "2.832149");
    }
}
