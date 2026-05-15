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
