//! `.lc1` / `.lc5` (or `.1`/`.5`) minute-bar file parser.
//!
//! Each record is 32 bytes, layout `<HHIIIIfII`:
//!   date(u16 packed), time(u16 minutes-since-midnight),
//!   open*100, high*100, low*100, close*100 (i32 each),
//!   amount(f32), volume(u32), reserved(u32).

use std::fs;
use std::path::Path;

use byteorder::{ByteOrder, LittleEndian};

use super::super::error::{Error, Result};
use super::super::types::Bar;

const RECORD_SIZE: usize = 32;

pub fn read_minute<P: AsRef<Path>>(path: P) -> Result<Vec<Bar>> {
    let path = path.as_ref();
    if !path.is_file() {
        return Err(Error::FileNotFound(path.display().to_string()));
    }
    let content = fs::read(path)?;
    read_minute_file(&content)
}

pub fn read_minute_file(content: &[u8]) -> Result<Vec<Bar>> {
    if content.len() % RECORD_SIZE != 0 {
        return Err(Error::InvalidRecordSize(content.len(), RECORD_SIZE));
    }
    let mut out = Vec::with_capacity(content.len() / RECORD_SIZE);
    for chunk in content.chunks_exact(RECORD_SIZE) {
        let date = LittleEndian::read_u16(&chunk[0..2]) as u32;
        let tmin = LittleEndian::read_u16(&chunk[2..4]) as u32;
        let open = LittleEndian::read_u32(&chunk[4..8]);
        let high = LittleEndian::read_u32(&chunk[8..12]);
        let low = LittleEndian::read_u32(&chunk[12..16]);
        let close = LittleEndian::read_u32(&chunk[16..20]);
        let amount = LittleEndian::read_f32(&chunk[20..24]);
        let volume = LittleEndian::read_u32(&chunk[24..28]);

        let year = (date / 2048 + 2004) as u16;
        let month = ((date % 2048) / 100) as u8;
        let day = ((date % 2048) % 100) as u8;
        let hour = (tmin / 60) as u8;
        let minute = (tmin % 60) as u8;

        out.push(Bar {
            year,
            month,
            day,
            hour,
            minute,
            open: open as f64 / 100.0,
            high: high as f64 / 100.0,
            low: low as f64 / 100.0,
            close: close as f64 / 100.0,
            amount: amount as f64,
            volume: volume as f64,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_one_synthetic_record() {
        // year=2024 month=3 day=15 → zipday = (2024-2004)*2048 + 3*100 + 15
        let zipday: u16 = ((2024 - 2004) * 2048 + 3 * 100 + 15) as u16;
        let tminutes: u16 = 9 * 60 + 35; // 09:35

        let mut buf = [0u8; 32];
        LittleEndian::write_u16(&mut buf[0..2], zipday);
        LittleEndian::write_u16(&mut buf[2..4], tminutes);
        LittleEndian::write_u32(&mut buf[4..8], 1500);
        LittleEndian::write_u32(&mut buf[8..12], 1550);
        LittleEndian::write_u32(&mut buf[12..16], 1490);
        LittleEndian::write_u32(&mut buf[16..20], 1520);
        LittleEndian::write_f32(&mut buf[20..24], 1234.5);
        LittleEndian::write_u32(&mut buf[24..28], 8_888);

        let bars = read_minute_file(&buf).unwrap();
        assert_eq!(bars.len(), 1);
        let b = &bars[0];
        assert_eq!(
            (b.year, b.month, b.day, b.hour, b.minute),
            (2024, 3, 15, 9, 35)
        );
        assert!((b.open - 15.0).abs() < 1e-9);
        assert!((b.close - 15.20).abs() < 1e-9);
        assert_eq!(b.volume, 8888.0);
    }
}
