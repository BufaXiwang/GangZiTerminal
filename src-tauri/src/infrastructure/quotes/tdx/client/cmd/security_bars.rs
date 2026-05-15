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

pub fn parse(body: &[u8], category: BarCategory) -> Result<Vec<Bar>> {
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
