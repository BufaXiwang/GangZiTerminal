//! `get_minute_time_data` — 当日分时（价格 + 成交量逐分钟序列）。
//!
//! Spec: docs/design/quotes-module.md §5 (TDX 协议层)
//!
//! Mirrors `pytdx/parser/get_minute_time_data.py`.
//!
//! Wire format:
//! - Request: 12 byte 固定 header `0c 1b 08 00 01 01 0e 00 0e 00 1d 05`
//!   + `<H6sI` market(u16) + code([u8;6]) + 0u32 padding
//! - Response body:
//!   - u16 count
//!   - 2 byte padding（pytdx `pos += 4` 后 num 已读 2 → 再跳 2）
//!   - 重复 count 次 `<price_delta_varint, reversed1_varint, vol_varint>`
//!   - `price = (prev + price_delta) / 100.0`（delta 累积到 last_price，再除 100）
//!
//! pytdx parser 本身 **不带时间戳** —— 调用方需根据 index 派生交易分钟（A 股 240 分钟）。

use byteorder::{ByteOrder, LittleEndian};

use super::super::super::error::{Error, Result};
use super::super::super::helper::get_price;
use super::super::super::types::MinuteTimePoint;

const HEADER: [u8; 12] = [
    0x0c, 0x1b, 0x08, 0x00, 0x01, 0x01, 0x0e, 0x00, 0x0e, 0x00, 0x1d, 0x05,
];

pub fn build(market: u8, code: &str) -> Result<Vec<u8>> {
    let code_bytes = code.as_bytes();
    if code_bytes.len() != 6 {
        return Err(Error::InvalidSymbol(format!(
            "expected 6-char code, got {code}"
        )));
    }
    // <H6sI: market u16 + code 6s + 0u32
    let mut pkg = Vec::with_capacity(HEADER.len() + 2 + 6 + 4);
    pkg.extend_from_slice(&HEADER);
    pkg.extend_from_slice(&(market as u16).to_le_bytes());
    pkg.extend_from_slice(code_bytes);
    pkg.extend_from_slice(&0u32.to_le_bytes());
    Ok(pkg)
}

pub fn parse(body: &[u8]) -> Result<Vec<MinuteTimePoint>> {
    if body.len() < 4 {
        return Err(Error::Protocol("minute_time: short body".into()));
    }
    let num = LittleEndian::read_u16(&body[0..2]) as usize;
    // pytdx: 读完 num（2 字节）后 `pos += 4` → 再跳 2 字节 padding
    let mut pos = 4usize;

    let mut out: Vec<MinuteTimePoint> = Vec::with_capacity(num);
    let mut last_price: i64 = 0;

    for _ in 0..num {
        let price_delta = get_price(body, &mut pos)?;
        let _reversed1 = get_price(body, &mut pos)?;
        let vol = get_price(body, &mut pos)?;
        last_price += price_delta;
        out.push(MinuteTimePoint {
            price: last_price as f64 / 100.0,
            volume: vol,
        });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_packet_layout() {
        let pkg = build(1, "600519").unwrap();
        assert_eq!(pkg.len(), 12 + 2 + 6 + 4);
        assert_eq!(&pkg[0..12], &HEADER);
        // market u16 LE
        assert_eq!(&pkg[12..14], &[0x01, 0x00]);
        assert_eq!(&pkg[14..20], b"600519");
        assert_eq!(&pkg[20..24], &[0u8; 4]);
    }

    #[test]
    fn build_rejects_bad_code() {
        assert!(build(0, "12345").is_err());
        assert!(build(0, "1234567").is_err());
    }

    #[test]
    fn parse_zero_points() {
        let mut body = vec![0u8; 0];
        body.extend_from_slice(&0u16.to_le_bytes()); // num=0
        body.extend_from_slice(&[0u8; 2]); // padding
        let pts = parse(&body).unwrap();
        assert!(pts.is_empty());
    }

    #[test]
    fn parse_two_points_delta_accumulates() {
        // 两个点：第 1 个 delta=10 vol=5；第 2 个 delta=20 vol=7
        // price_1 = 10/100 = 0.10
        // price_2 = (10+20)/100 = 0.30
        // 每个点 3 个 varint：delta, reversed1, vol
        // 用 small_positive 编码（单字节 = value）：10 = 0x0a，20 = 0x14，5 = 0x05，7 = 0x07
        let mut body = Vec::new();
        body.extend_from_slice(&2u16.to_le_bytes()); // num=2
        body.extend_from_slice(&[0u8; 2]); // padding
        // point 1
        body.push(0x0a); // delta=10
        body.push(0x00); // reversed1=0
        body.push(0x05); // vol=5
        // point 2
        body.push(0x14); // delta=20
        body.push(0x00); // reversed1=0
        body.push(0x07); // vol=7

        let pts = parse(&body).unwrap();
        assert_eq!(pts.len(), 2);
        assert!((pts[0].price - 0.10).abs() < 1e-9);
        assert_eq!(pts[0].volume, 5);
        assert!((pts[1].price - 0.30).abs() < 1e-9);
        assert_eq!(pts[1].volume, 7);
    }

    #[test]
    fn parse_negative_delta() {
        // delta=-10 (0x4a in varint)；起始 last_price=0
        // price_1 = -10/100 = -0.10（理论上不会出现负价，但 wire 上 delta 是 signed）
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_le_bytes());
        body.extend_from_slice(&[0u8; 2]);
        body.push(0x4a); // -10
        body.push(0x00);
        body.push(0x01);
        let pts = parse(&body).unwrap();
        assert_eq!(pts.len(), 1);
        assert!((pts[0].price - (-0.10)).abs() < 1e-9);
    }

    #[test]
    fn parse_truncated_returns_error() {
        let mut body = Vec::new();
        body.extend_from_slice(&5u16.to_le_bytes()); // num=5
        body.extend_from_slice(&[0u8; 2]);
        // no point data
        assert!(parse(&body).is_err());
    }
}
