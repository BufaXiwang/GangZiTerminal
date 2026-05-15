use byteorder::{ByteOrder, LittleEndian};
use encoding_rs::GBK;

use super::super::super::error::{Error, Result};
use super::super::super::helper::get_volume;
use super::super::super::types::SecurityListEntry;

pub fn build(market: u16, start: u16) -> Vec<u8> {
    let mut pkg = vec![
        0x0c, 0x01, 0x18, 0x64, 0x01, 0x01, 0x06, 0x00, 0x06, 0x00, 0x50, 0x04,
    ];
    pkg.extend_from_slice(&market.to_le_bytes());
    pkg.extend_from_slice(&start.to_le_bytes());
    pkg
}

pub fn parse(body: &[u8]) -> Result<Vec<SecurityListEntry>> {
    if body.len() < 2 {
        return Err(Error::Protocol("security_list: short body".into()));
    }
    let num = LittleEndian::read_u16(&body[0..2]) as usize;
    let mut pos = 2usize;
    let mut out = Vec::with_capacity(num);

    for _ in 0..num {
        if body.len() < pos + 29 {
            return Err(Error::Protocol("security_list: truncated entry".into()));
        }
        let row = &body[pos..pos + 29];

        let code = std::str::from_utf8(&row[0..6])
            .map_err(|_| Error::Protocol("security_list: non-utf8 code".into()))?
            .trim_end_matches('\0')
            .to_string();
        let volunit = LittleEndian::read_u16(&row[6..8]);
        let name_bytes = &row[8..16];
        let (name_cow, _, _) = GBK.decode(name_bytes);
        let name = name_cow.trim_end_matches('\0').to_string();
        let decimal_point = row[20];
        let pre_close_raw = LittleEndian::read_u32(&row[21..25]);
        let pre_close = get_volume(pre_close_raw);

        out.push(SecurityListEntry {
            code,
            volunit,
            decimal_point,
            name,
            pre_close,
        });

        pos += 29;
    }
    Ok(out)
}
