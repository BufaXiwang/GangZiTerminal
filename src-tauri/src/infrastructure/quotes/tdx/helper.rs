//! Low-level decoders mirroring `pytdx/helper.py`.
//!
//! These are quirky: prices use a varint-like signed encoding, and volumes use
//! a non-standard mantissa/exponent float decoded byte-by-byte.

use byteorder::{ByteOrder, LittleEndian};

use super::error::{Error, Result};
use super::types::BarCategory;

/// Decode a varint-like signed price; advances `pos` past the consumed bytes.
///
/// Layout: first byte's low 6 bits = magnitude bits 0..5, bit 6 = sign,
/// bit 7 = continuation. Each continuation byte uses low 7 bits, MSB = continue.
pub fn get_price(data: &[u8], pos: &mut usize) -> Result<i64> {
    let mut p = *pos;
    if p >= data.len() {
        return Err(Error::Protocol("get_price: out of bounds".into()));
    }
    let mut byte = data[p];
    let mut value: i64 = (byte & 0x3f) as i64;
    let sign = byte & 0x40 != 0;
    let mut shift = 6;

    if byte & 0x80 != 0 {
        loop {
            p += 1;
            if p >= data.len() {
                return Err(Error::Protocol("get_price: truncated varint".into()));
            }
            byte = data[p];
            value += ((byte & 0x7f) as i64) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
    }

    p += 1;
    *pos = p;
    Ok(if sign { -value } else { value })
}

/// Decode the Tdx "ivol" pseudo-float (`get_volume` in pytdx).
///
/// Direct port of the byte-by-byte mantissa/exponent reconstruction.
pub fn get_volume(ivol: u32) -> f64 {
    let logpoint = (ivol >> 24) as i32;
    let hleax = ((ivol >> 16) & 0xff) as i32;
    let lheax = ((ivol >> 8) & 0xff) as i32;
    let lleax = (ivol & 0xff) as i32;

    let dw_ecx = logpoint * 2 - 0x7f;
    let dw_edx = logpoint * 2 - 0x86;
    let dw_esi = logpoint * 2 - 0x8e;
    let dw_eax = logpoint * 2 - 0x96;

    let abs_ecx = dw_ecx.abs();
    let mut dbl_xmm6 = 2.0_f64.powi(abs_ecx);
    if dw_ecx < 0 {
        dbl_xmm6 = 1.0 / dbl_xmm6;
    }

    let dbl_xmm4 = if hleax > 0x80 {
        let dwtmpeax = dw_edx + 1;
        let tmp = 2.0_f64.powi(dwtmpeax);
        2.0_f64.powi(dw_edx) * 128.0 + ((hleax & 0x7f) as f64) * tmp
    } else if dw_edx >= 0 {
        2.0_f64.powi(dw_edx) * (hleax as f64)
    } else {
        (1.0 / 2.0_f64.powi(dw_edx)) * (hleax as f64)
    };

    let mut dbl_xmm3 = 2.0_f64.powi(dw_esi) * (lheax as f64);
    let mut dbl_xmm1 = 2.0_f64.powi(dw_eax) * (lleax as f64);
    if hleax & 0x80 != 0 {
        dbl_xmm3 *= 2.0;
        dbl_xmm1 *= 2.0;
    }

    dbl_xmm6 + dbl_xmm4 + dbl_xmm3 + dbl_xmm1
}

/// Decode the K-line datetime field; returns (year, month, day, hour, minute).
///
/// Intraday categories use `<HH` (zipday + tminutes-since-midnight).
/// Daily+ categories use `<I` (raw `YYYYMMDD` integer) and a fixed 15:00 close.
pub fn get_datetime(
    category: BarCategory,
    data: &[u8],
    pos: &mut usize,
) -> Result<(u16, u8, u8, u8, u8)> {
    if data.len() < *pos + 4 {
        return Err(Error::Protocol("get_datetime: truncated".into()));
    }
    let buf = &data[*pos..*pos + 4];
    *pos += 4;

    let (year, month, day, hour, minute) = if category.is_intraday() {
        let zipday = LittleEndian::read_u16(&buf[0..2]) as u32;
        let tminutes = LittleEndian::read_u16(&buf[2..4]) as u32;
        let year = (zipday >> 11) + 2004;
        let month = (zipday % 2048) / 100;
        let day = (zipday % 2048) % 100;
        let hour = tminutes / 60;
        let minute = tminutes % 60;
        (
            year as u16,
            month as u8,
            day as u8,
            hour as u8,
            minute as u8,
        )
    } else {
        let zipday = LittleEndian::read_u32(buf);
        let year = zipday / 10000;
        let month = (zipday % 10000) / 100;
        let day = zipday % 100;
        (year as u16, month as u8, day as u8, 15, 0)
    };

    Ok((year, month, day, hour, minute))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_zero() {
        let buf = [0u8];
        let mut pos = 0;
        assert_eq!(get_price(&buf, &mut pos).unwrap(), 0);
        assert_eq!(pos, 1);
    }

    #[test]
    fn price_small_positive() {
        let buf = [0x0a];
        let mut pos = 0;
        assert_eq!(get_price(&buf, &mut pos).unwrap(), 10);
        assert_eq!(pos, 1);
    }

    #[test]
    fn price_small_negative() {
        let buf = [0x4a];
        let mut pos = 0;
        assert_eq!(get_price(&buf, &mut pos).unwrap(), -10);
    }

    #[test]
    fn price_multibyte() {
        let buf = [0x80, 0x01];
        let mut pos = 0;
        assert_eq!(get_price(&buf, &mut pos).unwrap(), 64);
        assert_eq!(pos, 2);
    }
}
