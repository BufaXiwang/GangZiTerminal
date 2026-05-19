//! K 线图渲染——为 `analyze_chart` / `get_kline(mode=chart)` 工具生成 PNG 字节
//! 给 LLM vision 看。
//!
//! 设计目标：
//! - 纯函数：吃 `&[KlinePoint]` + 渲染参数，吐 `Vec<u8>` (PNG)
//! - 不依赖 Tauri / 网络——可单测
//! - 输出尺寸固定 800×500，控制 LLM 看图 cost；够展示价 + 量 + MA20
//!
//! 图层（自上而下）：
//! 1. 主图：蜡烛 + MA20 折线（A 股惯例红涨绿跌）
//! 2. 副图：成交量柱（按当日涨跌着色）

use crate::domain::quotes::types::KlinePoint;
use crate::domain::shared::{Lots, TradeDate, Yuan};
use crate::infrastructure::quotes::cache::kline_cache::KlineRow;
use plotters::prelude::*;

/// `KlineRow`（DB 行 / cache）→ `KlinePoint`（domain 类型）的统一转换器。
/// 老调用方分散在 visual.rs / quotes.rs 各自维护一份；现在共享这一份。
pub fn klinerow_to_point(r: &KlineRow) -> Option<KlinePoint> {
    let date = TradeDate::from_compact(&r.date).ok()?;
    Some(KlinePoint {
        date,
        open: Yuan::from_unchecked(r.open),
        close: Yuan::from_unchecked(r.close),
        high: Yuan::from_unchecked(r.high),
        low: Yuan::from_unchecked(r.low),
        volume: Lots::from_unchecked(r.volume.unwrap_or(0.0) as i64),
        amount: Yuan::from_unchecked(r.amount.unwrap_or(0.0)),
    })
}

pub struct ChartRenderOptions {
    pub width: u32,
    pub height: u32,
    pub title: String,
    /// 是否叠加 20 日 MA
    pub show_ma20: bool,
}

impl Default for ChartRenderOptions {
    fn default() -> Self {
        Self {
            width: 800,
            height: 500,
            title: "K-line".into(),
            show_ma20: true,
        }
    }
}

/// 渲染 K 线 PNG。`klines` 升序排列；调用方负责裁剪窗口（一般 60-120 根）。
pub fn render_kline_png(
    klines: &[KlinePoint],
    opt: &ChartRenderOptions,
) -> Result<Vec<u8>, String> {
    if klines.is_empty() {
        return Err("klines 为空，无法渲染".into());
    }

    let mut buf = vec![0u8; (opt.width * opt.height * 3) as usize];
    {
        let root = BitMapBackend::with_buffer(&mut buf, (opt.width, opt.height))
            .into_drawing_area();
        root.fill(&WHITE).map_err(|e| e.to_string())?;
        let (price_area, vol_area) = root.split_vertically((opt.height as f64 * 0.72) as u32);

        // ---- 主图：蜡烛 ---------------------------------------------------
        let lo = klines.iter().map(|k| k.low.value()).fold(f64::INFINITY, f64::min);
        let hi = klines.iter().map(|k| k.high.value()).fold(f64::NEG_INFINITY, f64::max);
        let pad = (hi - lo) * 0.05;
        let mut chart = ChartBuilder::on(&price_area)
            .margin(10)
            .build_cartesian_2d(0..klines.len() as i32, (lo - pad)..(hi + pad))
            .map_err(|e| e.to_string())?;
        chart
            .configure_mesh()
            .disable_x_mesh()
            .disable_y_mesh()
            .disable_x_axis()
            .disable_y_axis()
            .draw()
            .map_err(|e| e.to_string())?;

        // A 股惯例：红涨绿跌
        chart
            .draw_series(klines.iter().enumerate().map(|(i, k)| {
                let up = k.close.value() >= k.open.value();
                let color = if up { RED } else { GREEN };
                CandleStick::new(
                    i as i32,
                    k.open.value(),
                    k.high.value(),
                    k.low.value(),
                    k.close.value(),
                    color.filled(),
                    color.filled(),
                    4,
                )
            }))
            .map_err(|e| e.to_string())?;

        if opt.show_ma20 && klines.len() >= 20 {
            let ma: Vec<(i32, f64)> = (19..klines.len())
                .map(|end| {
                    let sum: f64 = klines[end - 19..=end]
                        .iter()
                        .map(|k| k.close.value())
                        .sum();
                    (end as i32, sum / 20.0)
                })
                .collect();
            chart
                .draw_series(LineSeries::new(ma, &BLUE))
                .map_err(|e| e.to_string())?;
        }

        // ---- 副图：成交量 -------------------------------------------------
        let max_vol = klines
            .iter()
            .map(|k| k.volume.value() as f64)
            .fold(0.0_f64, f64::max);
        let mut vchart = ChartBuilder::on(&vol_area)
            .margin(10)
            .build_cartesian_2d(0..klines.len() as i32, 0.0..(max_vol * 1.05))
            .map_err(|e| e.to_string())?;
        vchart
            .configure_mesh()
            .disable_x_mesh()
            .disable_y_mesh()
            .disable_x_axis()
            .disable_y_axis()
            .draw()
            .map_err(|e| e.to_string())?;
        vchart
            .draw_series(klines.iter().enumerate().map(|(i, k)| {
                let up = k.close.value() >= k.open.value();
                let color = if up { RED } else { GREEN };
                Rectangle::new(
                    [(i as i32, 0.0), (i as i32 + 1, k.volume.value() as f64)],
                    color.filled(),
                )
            }))
            .map_err(|e| e.to_string())?;

        root.present().map_err(|e| e.to_string())?;
    }

    // 将 RGB 原始字节编码为 PNG
    encode_png(&buf, opt.width, opt.height)
}

fn encode_png(rgb: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    use std::io::Cursor;
    let mut out = Cursor::new(Vec::new());
    {
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(rgb).map_err(|e| e.to_string())?;
    }
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shared::{Lots, TradeDate, Yuan};

    fn syn(close: f64, vol: i64, date: i32) -> KlinePoint {
        KlinePoint {
            date: TradeDate::from_unchecked(date),
            open: Yuan::new(close * 0.99).unwrap(),
            close: Yuan::new(close).unwrap(),
            high: Yuan::new(close * 1.02).unwrap(),
            low: Yuan::new(close * 0.98).unwrap(),
            volume: Lots::from_unchecked(vol),
            amount: Yuan::from_unchecked(vol as f64 * close),
        }
    }

    #[test]
    fn renders_non_empty_png() {
        let klines: Vec<KlinePoint> = (0..30)
            .map(|i| syn(10.0 + i as f64 * 0.1, 100_000, 20260100 + i))
            .collect();
        let png = render_kline_png(&klines, &ChartRenderOptions::default()).unwrap();
        // PNG signature
        assert_eq!(&png[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
        assert!(png.len() > 500);
    }

    #[test]
    fn empty_klines_errors() {
        assert!(render_kline_png(&[], &ChartRenderOptions::default()).is_err());
    }
}
