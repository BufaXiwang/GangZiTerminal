//! Wire framing: 16-byte response header + optional zlib body.
//!
//! Header layout (`<IIIHH`): 3 unused u32, then `zipsize` u16 and `unzipsize` u16.
//! When `zipsize != unzipsize` the body is zlib-compressed.

use std::io::{Read, Write};
use std::net::TcpStream;

use byteorder::{ByteOrder, LittleEndian};
use flate2::read::ZlibDecoder;

use super::super::error::{Error, Result};

pub const HEADER_LEN: usize = 0x10;

/// Send `pkg`, then read exactly one framed response and return the body.
pub fn request(sock: &mut TcpStream, pkg: &[u8]) -> Result<Vec<u8>> {
    sock.write_all(pkg)?;

    let mut header = [0u8; HEADER_LEN];
    sock.read_exact(&mut header)?;

    let zipsize = LittleEndian::read_u16(&header[12..14]) as usize;
    let unzipsize = LittleEndian::read_u16(&header[14..16]) as usize;

    let mut body = vec![0u8; zipsize];
    sock.read_exact(&mut body)?;

    if zipsize == unzipsize {
        Ok(body)
    } else {
        let mut decoder = ZlibDecoder::new(&body[..]);
        let mut out = Vec::with_capacity(unzipsize);
        decoder
            .read_to_end(&mut out)
            .map_err(|e| Error::Decompress(e.to_string()))?;
        Ok(out)
    }
}
