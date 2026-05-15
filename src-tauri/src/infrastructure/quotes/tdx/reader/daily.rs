//! `.day` file parser.
//!
//! Each record is 32 bytes, little-endian, layout `<IIIIIfII`:
//!   date(YYYYMMDD u32), open*100, high*100, low*100, close*100,
//!   amount(f32), volume(u32), reserved(u32).
//!
//! Per `daily_bar_reader.py`, price/volume scaling depends on instrument type
//! (A-stock, B-stock, index, fund, bond) which is inferred from the filename.

use std::fs;
use std::path::Path;

use byteorder::{ByteOrder, LittleEndian};

use super::super::error::{Error, Result};
use super::super::types::Bar;

const RECORD_SIZE: usize = 32;

/// Read & parse a `.day` file, applying price/volume scaling based on filename.
pub fn read_daily<P: AsRef<Path>>(path: P) -> Result<Vec<Bar>> {
    let path = path.as_ref();
    if !path.is_file() {
        return Err(Error::FileNotFound(path.display().to_string()));
    }
    let (price_coef, volume_coef) = coefficients_from_filename(path)?;
    let content = fs::read(path)?;
    parse_daily(&content, price_coef, volume_coef)
}

/// Lower-level entry point: parse `.day` bytes directly with explicit scaling.
pub fn read_daily_file(bytes: &[u8], price_coef: f64, volume_coef: f64) -> Result<Vec<Bar>> {
    parse_daily(bytes, price_coef, volume_coef)
}

fn parse_daily(content: &[u8], price_coef: f64, volume_coef: f64) -> Result<Vec<Bar>> {
    if content.len() % RECORD_SIZE != 0 {
        return Err(Error::InvalidRecordSize(content.len(), RECORD_SIZE));
    }
    let mut out = Vec::with_capacity(content.len() / RECORD_SIZE);
    for chunk in content.chunks_exact(RECORD_SIZE) {
        let date = LittleEndian::read_u32(&chunk[0..4]);
        let open = LittleEndian::read_u32(&chunk[4..8]);
        let high = LittleEndian::read_u32(&chunk[8..12]);
        let low = LittleEndian::read_u32(&chunk[12..16]);
        let close = LittleEndian::read_u32(&chunk[16..20]);
        let amount = LittleEndian::read_f32(&chunk[20..24]);
        let volume = LittleEndian::read_u32(&chunk[24..28]);
        // chunk[28..32] reserved

        let year = (date / 10000) as u16;
        let month = ((date % 10000) / 100) as u8;
        let day = (date % 100) as u8;

        out.push(Bar {
            year,
            month,
            day,
            hour: 0,
            minute: 0,
            open: open as f64 * price_coef,
            high: high as f64 * price_coef,
            low: low as f64 * price_coef,
            close: close as f64 * price_coef,
            amount: amount as f64,
            volume: volume as f64 * volume_coef,
        });
    }
    Ok(out)
}

/// Map filename → (price_coefficient, volume_coefficient). Mirrors
/// `TdxDailyBarReader.SECURITY_COEFFICIENT`. Falls back to A-stock scaling.
fn coefficients_from_filename(path: &Path) -> Result<(f64, f64)> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::UnknownExchange(path.display().to_string()))?;
    // expected `sh600000.day` / `sz000001.day`
    if name.len() < 9 {
        return Err(Error::UnknownExchange(name.into()));
    }
    let exchange = &name[..2].to_lowercase();
    let head = &name[2..4];

    Ok(match (exchange.as_str(), head) {
        ("sz", "00") | ("sz", "30") => (0.01, 0.01), // SZ_A_STOCK
        ("sz", "20") => (0.01, 0.01),                // SZ_B_STOCK
        ("sz", "39") => (0.01, 1.0),                 // SZ_INDEX
        ("sz", "15") | ("sz", "16") => (0.001, 0.01), // SZ_FUND
        ("sz", "10") | ("sz", "11") | ("sz", "12") | ("sz", "13") | ("sz", "14") => (0.001, 0.01), // SZ_BOND
        ("sh", "60") | ("sh", "68") => (0.01, 0.01), // SH_A_STOCK (incl. 688 STAR)
        ("sh", "90") => (0.001, 0.01),               // SH_B_STOCK
        ("sh", "00") | ("sh", "88") | ("sh", "99") => (0.01, 1.0), // SH_INDEX
        ("sh", "50") | ("sh", "51") => (0.001, 1.0), // SH_FUND
        ("sh", "01")
        | ("sh", "10")
        | ("sh", "11")
        | ("sh", "12")
        | ("sh", "13")
        | ("sh", "14")
        | ("sh", "20") => (0.001, 1.0), // SH_BOND
        _ => (0.01, 0.01),                           // sensible default
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_one_synthetic_record() {
        let mut buf = [0u8; 32];
        LittleEndian::write_u32(&mut buf[0..4], 20240115);
        LittleEndian::write_u32(&mut buf[4..8], 1234);
        LittleEndian::write_u32(&mut buf[8..12], 1300);
        LittleEndian::write_u32(&mut buf[12..16], 1200);
        LittleEndian::write_u32(&mut buf[16..20], 1280);
        LittleEndian::write_f32(&mut buf[20..24], 999.5);
        LittleEndian::write_u32(&mut buf[24..28], 10_000);

        let bars = parse_daily(&buf, 0.01, 0.01).unwrap();
        assert_eq!(bars.len(), 1);
        let b = &bars[0];
        assert_eq!((b.year, b.month, b.day), (2024, 1, 15));
        assert!((b.open - 12.34).abs() < 1e-9);
        assert!((b.high - 13.0).abs() < 1e-9);
        assert!((b.close - 12.80).abs() < 1e-9);
        assert!((b.amount - 999.5).abs() < 1e-3);
        assert!((b.volume - 100.0).abs() < 1e-9);
    }

    #[test]
    fn reject_partial_record() {
        let buf = [0u8; 31];
        assert!(parse_daily(&buf, 0.01, 0.01).is_err());
    }
}
