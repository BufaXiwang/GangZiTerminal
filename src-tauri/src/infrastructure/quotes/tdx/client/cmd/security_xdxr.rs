//! `get_xdxr_info` — 除权除息 / 公司行动历史。
//!
//! Spec: docs/design/quotes-module.md §5 (TDX 协议层)
//!
//! Mirrors `pytdx/parser/get_xdxr_info.py`. Each record is 29 bytes on the wire:
//! - 7 bytes: market(u8) + code([u8;6])  (重复每条 record 的标识，与请求一致)
//! - 1 byte:  unused / reserved
//! - 4 bytes: zipped date (YYYYMMDD as u32; xdxr 用 daily 日期编码，hour 固定 15:00)
//! - 1 byte:  category (1..=14)
//! - 16 bytes: category-dependent payload
//!     - cat == 1: `<ffff` = fenhong / peigujia / songzhuangu / peigu
//!     - cat in [11, 12]: `<IIfI` 第 3 个 f32 = suogu
//!     - cat in [13, 14]: `<fIfI` = xingquanjia / _ / fenshu / _
//!     - 其他: `<IIII` = panqianliutong / qianzongguben / panhouliutong / houzongguben
//!         （`u32` 通过 `get_volume` 解码为 f64）

use byteorder::{ByteOrder, LittleEndian};

use super::super::super::error::{Error, Result};
use super::super::super::helper::{get_datetime, get_volume};
use super::super::super::types::{BarCategory, XdxrRecord};

// 14 字节固定 header，由 pytdx `setParams` 抓包得到。
// 0c 1f 18 76 00 01 0b 00 0b 00 0f 00 01 00
const HEADER: [u8; 14] = [
    0x0c, 0x1f, 0x18, 0x76, 0x00, 0x01, 0x0b, 0x00, 0x0b, 0x00, 0x0f, 0x00, 0x01, 0x00,
];

pub fn build(market: u8, code: &str) -> Result<Vec<u8>> {
    let code_bytes = code.as_bytes();
    if code_bytes.len() != 6 {
        return Err(Error::InvalidSymbol(format!(
            "expected 6-char code, got {code}"
        )));
    }
    let mut pkg = Vec::with_capacity(HEADER.len() + 1 + 6);
    pkg.extend_from_slice(&HEADER);
    pkg.push(market);
    pkg.extend_from_slice(code_bytes);
    Ok(pkg)
}

pub fn parse(body: &[u8]) -> Result<Vec<XdxrRecord>> {
    // 协议在 num 之前先有 9 字节其他字段 (mirrors pytdx `pos += 9`)
    if body.len() < 11 {
        // 短 body：返回空列表（pytdx 行为）
        return Ok(Vec::new());
    }
    let mut pos = 9usize;
    let num = LittleEndian::read_u16(&body[pos..pos + 2]) as usize;
    pos += 2;

    let mut out: Vec<XdxrRecord> = Vec::with_capacity(num);

    for _ in 0..num {
        // 7 bytes market + code（每条 record 都重复一份；我们只用日期 + category + payload）
        if body.len() < pos + 7 + 1 + 4 + 1 + 16 {
            return Err(Error::Protocol("xdxr: truncated record".into()));
        }
        pos += 7;
        pos += 1; // 1 byte unused
        // xdxr 用 daily 日期编码（category 9 in pytdx → 落 else 分支 = YYYYMMDD）。
        // 我们 BarCategory::Day 走同一分支。
        let (year, month, day, _hour, _minute) = get_datetime(BarCategory::Day, body, &mut pos)?;
        let category = body[pos];
        pos += 1;

        let payload = &body[pos..pos + 16];
        pos += 16;

        let mut rec = XdxrRecord {
            year,
            month,
            day,
            category,
            fenhong: None,
            peigujia: None,
            songzhuangu: None,
            peigu: None,
            suogu: None,
            xingquanjia: None,
            fenshu: None,
            panqianliutong: None,
            qianzongguben: None,
            panhouliutong: None,
            houzongguben: None,
        };

        match category {
            1 => {
                rec.fenhong = Some(LittleEndian::read_f32(&payload[0..4]));
                rec.peigujia = Some(LittleEndian::read_f32(&payload[4..8]));
                rec.songzhuangu = Some(LittleEndian::read_f32(&payload[8..12]));
                rec.peigu = Some(LittleEndian::read_f32(&payload[12..16]));
            }
            11 | 12 => {
                // <IIfI: 取第 3 个 f32 作为 suogu
                rec.suogu = Some(LittleEndian::read_f32(&payload[8..12]));
            }
            13 | 14 => {
                // <fIfI: xingquanjia, _, fenshu, _
                rec.xingquanjia = Some(LittleEndian::read_f32(&payload[0..4]));
                rec.fenshu = Some(LittleEndian::read_f32(&payload[8..12]));
            }
            _ => {
                // <IIII，每个 u32 走 get_volume 翻成 f64；0 保留 0（pytdx `_get_v`）
                let get_v = |raw: u32| -> f64 {
                    if raw == 0 {
                        0.0
                    } else {
                        get_volume(raw)
                    }
                };
                rec.panqianliutong = Some(get_v(LittleEndian::read_u32(&payload[0..4])));
                rec.qianzongguben = Some(get_v(LittleEndian::read_u32(&payload[4..8])));
                rec.panhouliutong = Some(get_v(LittleEndian::read_u32(&payload[8..12])));
                rec.houzongguben = Some(get_v(LittleEndian::read_u32(&payload[12..16])));
            }
        }

        out.push(rec);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_packet_layout() {
        // 600000.SH market=1
        let pkg = build(1, "600000").unwrap();
        assert_eq!(pkg.len(), 14 + 1 + 6);
        assert_eq!(&pkg[0..14], &HEADER);
        assert_eq!(pkg[14], 1);
        assert_eq!(&pkg[15..21], b"600000");
    }

    #[test]
    fn build_rejects_bad_code() {
        assert!(build(1, "60000").is_err());
        assert!(build(1, "6000001").is_err());
    }

    #[test]
    fn parse_empty_short_body() {
        // pytdx: < 11 bytes => []
        let body = vec![0u8; 5];
        assert!(parse(&body).unwrap().is_empty());
    }

    #[test]
    fn parse_zero_records() {
        // 9 bytes skipped + num=0
        let mut body = vec![0u8; 9];
        body.extend_from_slice(&0u16.to_le_bytes());
        let recs = parse(&body).unwrap();
        assert!(recs.is_empty());
    }

    #[test]
    fn parse_single_category_1() {
        // 构造 1 条 cat=1 记录：
        // 9 skip + num=1
        // + 7 (market+code) + 1 unused + 4 date(20240611) + 1 cat=1
        // + 16 payload: fenhong=2.5 peigujia=0.0 songzhuangu=5.0 peigu=0.0
        let mut body = vec![0u8; 9];
        body.extend_from_slice(&1u16.to_le_bytes());
        body.push(1); // market
        body.extend_from_slice(b"600519"); // code
        body.push(0); // unused
        body.extend_from_slice(&20240611u32.to_le_bytes()); // date
        body.push(1); // category
        body.extend_from_slice(&2.5f32.to_le_bytes());
        body.extend_from_slice(&0.0f32.to_le_bytes());
        body.extend_from_slice(&5.0f32.to_le_bytes());
        body.extend_from_slice(&0.0f32.to_le_bytes());

        let recs = parse(&body).unwrap();
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!((r.year, r.month, r.day), (2024, 6, 11));
        assert_eq!(r.category, 1);
        assert_eq!(r.fenhong, Some(2.5));
        assert_eq!(r.peigujia, Some(0.0));
        assert_eq!(r.songzhuangu, Some(5.0));
        assert_eq!(r.peigu, Some(0.0));
        assert!(r.suogu.is_none());
    }

    #[test]
    fn parse_category_11_suogu() {
        let mut body = vec![0u8; 9];
        body.extend_from_slice(&1u16.to_le_bytes());
        body.push(0); // market
        body.extend_from_slice(b"000001"); // code
        body.push(0); // unused
        body.extend_from_slice(&20200101u32.to_le_bytes());
        body.push(11); // category
        // <IIfI: u32 + u32 + f32 + u32 => suogu at offset 8..12
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&0.8f32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        let r = &parse(&body).unwrap()[0];
        assert_eq!(r.category, 11);
        assert_eq!(r.suogu, Some(0.8));
    }

    #[test]
    fn parse_category_13_warrant() {
        let mut body = vec![0u8; 9];
        body.extend_from_slice(&1u16.to_le_bytes());
        body.push(1);
        body.extend_from_slice(b"600008");
        body.push(0);
        body.extend_from_slice(&20060419u32.to_le_bytes());
        body.push(13);
        body.extend_from_slice(&3.14f32.to_le_bytes()); // xingquanjia
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&100.0f32.to_le_bytes()); // fenshu
        body.extend_from_slice(&0u32.to_le_bytes());
        let r = &parse(&body).unwrap()[0];
        assert_eq!(r.category, 13);
        assert_eq!(r.xingquanjia, Some(3.14));
        assert_eq!(r.fenshu, Some(100.0));
    }

    #[test]
    fn parse_other_category_share_structure() {
        // category=5 走 else 分支：4 个 u32 经 get_volume 解码
        let mut body = vec![0u8; 9];
        body.extend_from_slice(&1u16.to_le_bytes());
        body.push(0);
        body.extend_from_slice(b"000656");
        body.push(0);
        body.extend_from_slice(&20170630u32.to_le_bytes());
        body.push(5);
        // 全 0：经 _get_v 应该全为 0.0
        body.extend_from_slice(&[0u8; 16]);
        let r = &parse(&body).unwrap()[0];
        assert_eq!(r.category, 5);
        assert_eq!(r.panqianliutong, Some(0.0));
        assert_eq!(r.panhouliutong, Some(0.0));
    }

    #[test]
    fn parse_truncated_returns_error() {
        let mut body = vec![0u8; 9];
        body.extend_from_slice(&1u16.to_le_bytes()); // num=1 但后面没字节
        assert!(parse(&body).is_err());
    }
}
