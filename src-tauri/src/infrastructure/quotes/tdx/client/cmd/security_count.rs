use byteorder::{ByteOrder, LittleEndian};

use super::super::super::error::{Error, Result};

pub fn build(market: u16) -> Vec<u8> {
    let mut pkg = vec![
        0x0c, 0x0c, 0x18, 0x6c, 0x00, 0x01, 0x08, 0x00, 0x08, 0x00, 0x4e, 0x04,
    ];
    pkg.extend_from_slice(&market.to_le_bytes());
    pkg.extend_from_slice(&[0x75, 0xc7, 0x33, 0x01]);
    pkg
}

pub fn parse(body: &[u8]) -> Result<u16> {
    if body.len() < 2 {
        return Err(Error::Protocol("security_count: short body".into()));
    }
    Ok(LittleEndian::read_u16(&body[0..2]))
}
