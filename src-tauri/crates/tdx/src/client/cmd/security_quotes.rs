//! `get_security_quotes` — real-time L1 quote with 5-level book.

use byteorder::{ByteOrder, LittleEndian};

use super::super::super::error::{Error, Result};
use super::super::super::helper::{get_price, get_volume};
use super::super::super::types::{QuoteLevel, SecurityQuote};

pub fn build(stocks: &[(u8, &str)]) -> Result<Vec<u8>> {
    if stocks.is_empty() {
        return Err(Error::Protocol("security_quotes: no stocks".into()));
    }
    for (_, code) in stocks {
        if code.len() != 6 {
            return Err(Error::InvalidSymbol((*code).to_string()));
        }
    }
    let stock_len = stocks.len() as u16;
    let pkgdatalen: u16 = stock_len * 7 + 12;

    // Header layout (<HIHHIIHH): primary u16 + secondary u32 + pkglen u16 + pkglen u16
    //   + cmd u32 + 0u32 + 0u16 + stock_len u16 = 2+4+2+2+4+4+2+2 = 22 bytes
    let mut pkg = Vec::with_capacity(22 + stocks.len() * 7);
    pkg.extend_from_slice(&0x10c_u16.to_le_bytes());
    pkg.extend_from_slice(&0x02006320_u32.to_le_bytes());
    pkg.extend_from_slice(&pkgdatalen.to_le_bytes());
    pkg.extend_from_slice(&pkgdatalen.to_le_bytes());
    pkg.extend_from_slice(&0x5053e_u32.to_le_bytes());
    pkg.extend_from_slice(&0u32.to_le_bytes());
    pkg.extend_from_slice(&0u16.to_le_bytes());
    pkg.extend_from_slice(&stock_len.to_le_bytes());

    for (market, code) in stocks {
        pkg.push(*market);
        pkg.extend_from_slice(code.as_bytes());
    }
    Ok(pkg)
}

pub fn parse(body: &[u8]) -> Result<Vec<SecurityQuote>> {
    if body.len() < 4 {
        return Err(Error::Protocol("security_quotes: short body".into()));
    }
    let mut pos = 2usize; // skip 2 bytes
    let num = LittleEndian::read_u16(&body[pos..pos + 2]) as usize;
    pos += 2;

    let mut out = Vec::with_capacity(num);

    let cal_price = |base_p: i64, diff: i64| (base_p + diff) as f64 / 100.0;

    for _ in 0..num {
        if body.len() < pos + 9 {
            return Err(Error::Protocol("security_quotes: truncated header".into()));
        }
        let market = body[pos];
        let code = std::str::from_utf8(&body[pos + 1..pos + 7])
            .map_err(|_| Error::Protocol("security_quotes: non-utf8 code".into()))?
            .to_string();
        let active1 = LittleEndian::read_u16(&body[pos + 7..pos + 9]);
        pos += 9;

        let price = get_price(body, &mut pos)?;
        let last_close_diff = get_price(body, &mut pos)?;
        let open_diff = get_price(body, &mut pos)?;
        let high_diff = get_price(body, &mut pos)?;
        let low_diff = get_price(body, &mut pos)?;
        let _reversed0 = get_price(body, &mut pos)?;
        let _reversed1 = get_price(body, &mut pos)?;
        let vol = get_price(body, &mut pos)? as f64;
        let cur_vol = get_price(body, &mut pos)? as f64;

        if body.len() < pos + 4 {
            return Err(Error::Protocol("security_quotes: truncated amount".into()));
        }
        let amount_raw = LittleEndian::read_u32(&body[pos..pos + 4]);
        let amount = get_volume(amount_raw);
        pos += 4;

        let s_vol = get_price(body, &mut pos)? as f64;
        let b_vol = get_price(body, &mut pos)? as f64;
        let _r2 = get_price(body, &mut pos)?;
        let _r3 = get_price(body, &mut pos)?;

        let mut book: [QuoteLevel; 5] = Default::default();
        for level in &mut book {
            let bid = get_price(body, &mut pos)?;
            let ask = get_price(body, &mut pos)?;
            let bid_vol = get_price(body, &mut pos)?;
            let ask_vol = get_price(body, &mut pos)?;
            *level = QuoteLevel {
                bid: cal_price(price, bid),
                ask: cal_price(price, ask),
                bid_vol: bid_vol as f64,
                ask_vol: ask_vol as f64,
            };
        }

        if body.len() < pos + 2 {
            return Err(Error::Protocol("security_quotes: truncated tail".into()));
        }
        pos += 2; // reversed_bytes4 u16
        let _r5 = get_price(body, &mut pos)?;
        let _r6 = get_price(body, &mut pos)?;
        let _r7 = get_price(body, &mut pos)?;
        let _r8 = get_price(body, &mut pos)?;
        if body.len() < pos + 4 {
            return Err(Error::Protocol("security_quotes: truncated rate".into()));
        }
        let r9 = LittleEndian::read_i16(&body[pos..pos + 2]);
        let active2 = LittleEndian::read_u16(&body[pos + 2..pos + 4]);
        pos += 4;

        out.push(SecurityQuote {
            market,
            code,
            active1,
            price: cal_price(price, 0),
            last_close: cal_price(price, last_close_diff),
            open: cal_price(price, open_diff),
            high: cal_price(price, high_diff),
            low: cal_price(price, low_diff),
            vol,
            cur_vol,
            amount,
            s_vol,
            b_vol,
            book,
            rate: r9 as f64 / 100.0,
            active2,
        });
    }
    Ok(out)
}
