//! Offline parser for local Tdx data files (`.day`, `.lc1`, `.lc5`).
//!
//! Mirrors `pytdx/reader/daily_bar_reader.py` and `min_bar_reader.py`.

mod daily;
mod minute;

pub use daily::{read_daily, read_daily_file};
pub use minute::{read_minute, read_minute_file};

use std::path::{Path, PathBuf};

use super::error::{Error, Result};
use super::types::Bar;

/// High-level reader that mirrors `mootdx.reader.StdReader`.
///
/// Resolves files under `{tdxdir}/vipdoc/{market}/{subdir}/{prefix}{code}.{ext}`.
pub struct Reader {
    tdxdir: PathBuf,
}

impl Reader {
    pub fn new<P: AsRef<Path>>(tdxdir: P) -> Result<Self> {
        let dir = tdxdir.as_ref().to_path_buf();
        if !dir.is_dir() {
            return Err(Error::FileNotFound(dir.display().to_string()));
        }
        Ok(Self { tdxdir: dir })
    }

    /// Daily bars for a stock symbol (e.g. `"600036"` or `"000001"`).
    pub fn daily(&self, symbol: &str) -> Result<Vec<Bar>> {
        let path = self.find(symbol, "lday", &["day"])?;
        read_daily(&path)
    }

    /// 1-minute bars (`.lc1` or `.1`).
    pub fn minute1(&self, symbol: &str) -> Result<Vec<Bar>> {
        let path = self.find(symbol, "minline", &["lc1", "1"])?;
        read_minute(&path)
    }

    /// 5-minute bars (`.lc5` or `.5`).
    pub fn minute5(&self, symbol: &str) -> Result<Vec<Bar>> {
        let path = self.find(symbol, "fzline", &["lc5", "5"])?;
        read_minute(&path)
    }

    fn find(&self, symbol: &str, subdir: &str, suffixes: &[&str]) -> Result<PathBuf> {
        let market = guess_market_prefix(symbol)?;
        let stem = if symbol.starts_with(market) {
            symbol.to_string()
        } else {
            format!("{}{}", market, symbol)
        };

        for ext in suffixes {
            let p = self
                .tdxdir
                .join("vipdoc")
                .join(market)
                .join(subdir)
                .join(format!("{}.{}", stem, ext));
            if p.is_file() {
                return Ok(p);
            }
        }
        Err(Error::FileNotFound(format!(
            "{}/{}/{}.{:?}",
            market, subdir, stem, suffixes
        )))
    }
}

fn guess_market_prefix(symbol: &str) -> Result<&'static str> {
    // mirrors mootdx.utils.get_stock_market + the 88* special case
    let s = if symbol.len() == 8 && (symbol.starts_with("sh") || symbol.starts_with("sz")) {
        &symbol[2..]
    } else {
        symbol
    };
    if s.starts_with("88") {
        return Ok("sh");
    }
    let head = s
        .as_bytes()
        .first()
        .copied()
        .ok_or_else(|| Error::InvalidSymbol(format!("symbol too short: {symbol}")))?;
    match head {
        b'6' | b'5' | b'9' => Ok("sh"),
        _ => Ok("sz"),
    }
}
