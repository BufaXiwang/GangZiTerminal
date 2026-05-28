//! `get_security_bars` — K-line history.
//!
//! Mirrors `pytdx/parser/get_security_bars.py`. Note the per-record state:
//! prices are delta-encoded, so we keep a running `pre_diff_base` between rows.

use byteorder::{ByteOrder, LittleEndian};

use super::super::super::error::{Error, Result};
use super::super::super::helper::{get_datetime, get_price, get_volume};
use super::super::super::types::{Bar, BarCategory};

const CMD_PRIMARY: u16 = 0x10c;
const CMD_SECONDARY: u32 = 0x01016408;
const CMD_OPCODE: u16 = 0x052d;

pub fn build(
    category: BarCategory,
    market: u8,
    code: &str,
    start: u16,
    count: u16,
) -> Result<Vec<u8>> {
    let code_bytes = code.as_bytes();
    if code_bytes.len() != 6 {
        return Err(Error::InvalidSymbol(format!(
            "expected 6-char code, got {code}"
        )));
    }
    // Layout (Python): <HIHHHH6sHHHHIIH
    //   H I H H H H 6s H H H H I I H  => total 2+4+2+2+2+2+6+2+2+2+2+4+4+2 = 38 bytes
    let mut pkg = Vec::with_capacity(38);
    pkg.extend_from_slice(&CMD_PRIMARY.to_le_bytes());
    pkg.extend_from_slice(&CMD_SECONDARY.to_le_bytes());
    pkg.extend_from_slice(&0x1c_u16.to_le_bytes());
    pkg.extend_from_slice(&0x1c_u16.to_le_bytes());
    pkg.extend_from_slice(&CMD_OPCODE.to_le_bytes());
    pkg.extend_from_slice(&(market as u16).to_le_bytes());
    pkg.extend_from_slice(code_bytes);
    pkg.extend_from_slice(&category.as_u16().to_le_bytes());
    pkg.extend_from_slice(&1u16.to_le_bytes());
    pkg.extend_from_slice(&start.to_le_bytes());
    pkg.extend_from_slice(&count.to_le_bytes());
    pkg.extend_from_slice(&0u32.to_le_bytes());
    pkg.extend_from_slice(&0u32.to_le_bytes());
    pkg.extend_from_slice(&0u16.to_le_bytes());
    Ok(pkg)
}

/// Parse a `0x052d` K-line response.
///
/// **`is_index`**：指数 K 线（pytdx `get_index_bars`）每根 bar 在 vol/amount 之后
/// 多 4 字节 (up_count u16 + down_count u16)。我们不需要这两个字段，但必须读掉
/// 否则 pos 会漂移 4N 字节导致后续 bar 解码错乱。**这是历史 K 线错位 bug 的根因**。
///
/// 个股 / ETF / 场内基金：is_index = false。
/// 指数（SH 000xxx, SZ 399xxx 等）：is_index = true。
pub fn parse(body: &[u8], category: BarCategory, is_index: bool) -> Result<Vec<Bar>> {
    if body.len() < 2 {
        return Err(Error::Protocol("security_bars: short body".into()));
    }
    let count = LittleEndian::read_u16(&body[0..2]) as usize;
    let mut pos = 2usize;
    let mut out = Vec::with_capacity(count);
    let mut pre_diff_base: i64 = 0;

    for _ in 0..count {
        let (year, month, day, hour, minute) = get_datetime(category, body, &mut pos)?;

        let open_diff = get_price(body, &mut pos)?;
        let close_diff = get_price(body, &mut pos)?;
        let high_diff = get_price(body, &mut pos)?;
        let low_diff = get_price(body, &mut pos)?;

        if body.len() < pos + 8 {
            return Err(Error::Protocol("security_bars: truncated volume".into()));
        }
        let vol_raw = LittleEndian::read_u32(&body[pos..pos + 4]);
        let vol = get_volume(vol_raw);
        pos += 4;
        let dbvol_raw = LittleEndian::read_u32(&body[pos..pos + 4]);
        let amount = get_volume(dbvol_raw);
        pos += 4;

        // 指数 bars 在此处有额外 4 字节 up_count/down_count（pytdx get_index_bars）。
        if is_index {
            if body.len() < pos + 4 {
                return Err(Error::Protocol(
                    "security_bars: truncated index up/down_count".into(),
                ));
            }
            pos += 4;
        }

        let open_v = (open_diff + pre_diff_base) as f64 / 1000.0;
        let abs_open = open_diff + pre_diff_base;
        let close_v = (abs_open + close_diff) as f64 / 1000.0;
        let high_v = (abs_open + high_diff) as f64 / 1000.0;
        let low_v = (abs_open + low_diff) as f64 / 1000.0;

        pre_diff_base = abs_open + close_diff;

        out.push(Bar {
            year,
            month,
            day,
            hour,
            minute,
            open: open_v,
            close: close_v,
            high: high_v,
            low: low_v,
            volume: vol,
            amount,
        });
    }
    Ok(out)
}

/// 协议层 helper：判断 6 位 code 是否为指数。
/// SH: `000xxx` (上证指数 / 行业指数 / ETF 指数等) / `999xxx`
/// SZ: `399xxx` (深证成指 / 创业板指 / 中证指数等)
pub fn is_index_code(code: &str, market: u8) -> bool {
    if code.len() != 6 {
        return false;
    }
    match market {
        // SH = 1 in TDX protocol (TdxMarket::SH)
        1 => code.starts_with("000") || code.starts_with("999"),
        // SZ = 0
        0 => code.starts_with("399"),
        _ => false,
    }
}
