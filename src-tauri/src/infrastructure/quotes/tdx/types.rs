//! Strongly-typed records returned by reader & client.

/// K-line categories used by `get_security_bars` / `get_index_bars`.
///
/// Numeric values match the Tdx protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum BarCategory {
    Minute5 = 0,
    Minute15 = 1,
    Minute30 = 2,
    Hour = 3,
    Day = 4,
    Week = 5,
    Month = 6,
    Minute = 7,
    Minute1 = 8,
    DayBfq = 9, // 日 不复权 (mootdx 默认)
    Quarter = 10,
    Year = 11,
}

impl BarCategory {
    pub fn as_u16(self) -> u16 {
        self as u16
    }
    /// True if datetime encoding uses the short 4-byte (zipday/tminutes) layout.
    pub(crate) fn is_intraday(self) -> bool {
        matches!(
            self,
            BarCategory::Minute5
                | BarCategory::Minute15
                | BarCategory::Minute30
                | BarCategory::Hour
                | BarCategory::Minute
                | BarCategory::Minute1
        )
    }
}

/// Market id for HQ requests (0 = SZ, 1 = SH).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Market {
    SZ = 0,
    SH = 1,
}

impl Market {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Guess market for an A-share-style code (mootdx logic).
    pub fn guess(code: &str) -> Self {
        // 0/3 -> SZ, 6/5/9 -> SH (rough, mirrors mootdx StdReader.find_path).
        match code.as_bytes().first() {
            Some(b'6') | Some(b'9') | Some(b'5') => Market::SH,
            _ => Market::SZ,
        }
    }
}

/// One OHLCV bar returned by online & offline APIs.
#[derive(Debug, Clone, PartialEq)]
pub struct Bar {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// 成交量。Online K线返回的是浮点解码；offline daily/minute 直接是整数。
    pub volume: f64,
    /// 成交额。
    pub amount: f64,
}

impl Bar {
    /// `YYYY-MM-DD HH:MM` for intraday or `YYYY-MM-DD` for daily-and-above.
    pub fn datetime(&self) -> String {
        if self.hour == 0 && self.minute == 0 {
            format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
        } else {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                self.year, self.month, self.day, self.hour, self.minute
            )
        }
    }
}

/// One entry of `get_security_list`.
#[derive(Debug, Clone, PartialEq)]
pub struct SecurityListEntry {
    pub code: String,
    pub volunit: u16,
    pub decimal_point: u8,
    pub name: String,
    pub pre_close: f64,
}

/// One entry of `get_security_quotes` — real-time level-1 quote with 5-level book.
#[derive(Debug, Clone, PartialEq)]
pub struct SecurityQuote {
    pub market: u8,
    pub code: String,
    pub active1: u16,
    pub price: f64,
    pub last_close: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub vol: f64,
    pub cur_vol: f64,
    pub amount: f64,
    pub s_vol: f64,
    pub b_vol: f64,
    /// (bid, ask, bid_vol, ask_vol) for levels 1..=5
    pub book: [QuoteLevel; 5],
    /// 涨速
    pub rate: f64,
    pub active2: u16,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct QuoteLevel {
    pub bid: f64,
    pub ask: f64,
    pub bid_vol: f64,
    pub ask_vol: f64,
}

/// 除权除息 / 公司行动信息分类（mirrors pytdx `XDXR_CATEGORY_MAPPING`）。
///
/// 不同 category 决定 `XdxrRecord` 里哪些字段有值——参见 `pytdx/parser/get_xdxr_info.py`。
/// 这是协议层的原样转录；上层（pipeline / domain）负责把 raw 字段翻译成 qfq / hfq 计算输入。
///
/// | id | name | semantics |
/// |---|---|---|
/// | 1  | 除权除息 (Cash dividend + bonus + rights) | 填 `fenhong / peigujia / songzhuangu / peigu` |
/// | 2  | 送配股上市 (Bonus/rights listing) | 填 share-structure 4 字段 |
/// | 3  | 非流通股上市 (Non-tradable shares listed) | 填 share-structure 4 字段 |
/// | 4  | 未知股本变动 (Unknown share change) | 填 share-structure 4 字段 |
/// | 5  | 股本变化 (Share structure change) | 填 share-structure 4 字段 |
/// | 6  | 增发新股 (New share offering) | 填 share-structure 4 字段 |
/// | 7  | 股份回购 (Share buyback) | 填 share-structure 4 字段 |
/// | 8  | 增发新股上市 (New issue listed) | 填 share-structure 4 字段 |
/// | 9  | 转配股上市 (Converted rights listing) | 填 share-structure 4 字段 |
/// | 10 | 可转债上市 (Convertible bond listing) | 填 share-structure 4 字段 |
/// | 11 | 扩缩股 (Share split/consolidation) | 填 `suogu` |
/// | 12 | 非流通股缩股 (Non-tradable consolidation) | 填 `suogu` |
/// | 13 | 送认购权证 (Call warrant) | 填 `xingquanjia / fenshu` |
/// | 14 | 送认沽权证 (Put warrant) | 填 `xingquanjia / fenshu` |
///
/// Category 1 是 qfq/hfq 复权计算最核心的输入：每个除权日的 `fenhong`（每 10 股派息）、
/// `songzhuangu`（每 10 股送转股本）、`peigu`（每 10 股配股股数）、`peigujia`（配股价）。
#[derive(Debug, Clone, PartialEq)]
pub struct XdxrRecord {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    /// 事件分类 1..=14；未知值原样保留。
    pub category: u8,

    // Category == 1（除权除息）：以下 4 个字段才有值
    /// 每 10 股派息（人民币元）
    pub fenhong: Option<f32>,
    /// 配股价（人民币元）
    pub peigujia: Option<f32>,
    /// 每 10 股送转股本（股）
    pub songzhuangu: Option<f32>,
    /// 每 10 股配股股数（股）
    pub peigu: Option<f32>,

    // Category in [11, 12]：缩股比例
    pub suogu: Option<f32>,

    // Category in [13, 14]：权证
    pub xingquanjia: Option<f32>,
    pub fenshu: Option<f32>,

    // 其他 category：股本结构变动，4 个字段都是 `get_volume` 解码后的浮点
    pub panqianliutong: Option<f64>,
    pub qianzongguben: Option<f64>,
    pub panhouliutong: Option<f64>,
    pub houzongguben: Option<f64>,
}

/// 当日分时点（`get_minute_time_data`）。
///
/// pytdx 返回 240 个点（A 股标准交易时段 9:30–11:30 / 13:00–15:00 = 240 分钟），
/// 价格是 delta-encoded：每点 `price = prev_price + delta`，再除以 100 得元。
/// 协议本身**不带时间戳**——调用方按 `index → trading minute` 派生时间。
#[derive(Debug, Clone, PartialEq)]
pub struct MinuteTimePoint {
    /// 价格（元）。
    pub price: f64,
    /// 该分钟的成交量（手或股，按 volunit 转换由上层处理）。
    pub volume: i64,
}

