//! TuShare K 线 + 复权因子 + daily_basic。
//!
//! Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/tushare.md

use super::client::{pick_f64, pick_str, TushareClient, TushareError};
use crate::domain::quotes::{Adjust, DailyBasic, KlinePeriod, KlinePoint};
use crate::domain::shared::{Amount, Money, Percent, Price, TradeDate, TsCode, Volume};
use chrono::Utc;
use rust_decimal::{prelude::FromPrimitive, Decimal};
use serde_json::json;

impl TushareClient {
    /// 日 / 周 / 月 K（不复权）。返回按 trade_date 升序。
    pub async fn fetch_kline(
        &self,
        ts_code: &TsCode,
        period: KlinePeriod,
        start_date: &str,
        end_date: &str,
    ) -> Result<Vec<KlinePoint>, TushareError> {
        let api = match period {
            KlinePeriod::Day => "daily",
            KlinePeriod::Week => "weekly",
            KlinePeriod::Month => "monthly",
        };
        let params = json!({
            "ts_code": ts_code.as_str(),
            "start_date": start_date,
            "end_date": end_date,
        });
        let data = self
            .call(
                api,
                params,
                "ts_code,trade_date,open,high,low,close,vol,amount",
            )
            .await?;
        let mut out = Vec::with_capacity(data.items.len());
        for row in data.items.iter() {
            let Some(td) = pick_str(&data.fields, row, "trade_date") else {
                continue;
            };
            let Ok(date) = TradeDate::parse(&td) else {
                continue;
            };
            let parse_p = |k: &str| -> Option<Price> {
                pick_f64(&data.fields, row, k)
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .and_then(|v| Decimal::from_f64(v).map(|d| Price(d.round_dp(4))))
            };
            let Some(open) = parse_p("open") else { continue };
            let Some(close) = parse_p("close") else { continue };
            let Some(high) = parse_p("high") else { continue };
            let Some(low) = parse_p("low") else { continue };
            let volume = pick_f64(&data.fields, row, "vol")
                .filter(|v| *v > 0.0)
                // TuShare daily vol 单位是手；× 100 转股
                .map(|v| Volume((v * 100.0) as i64));
            let amount = pick_f64(&data.fields, row, "amount")
                .filter(|v| *v > 0.0)
                // TuShare amount 单位是千元；× 1000 转元
                .and_then(|v| Decimal::from_f64(v * 1000.0).map(|d| Amount(d.round_dp(4))));
            out.push(KlinePoint {
                date,
                open,
                close,
                high,
                low,
                volume,
                amount,
            });
        }
        // TuShare 默认按 trade_date desc 返回；这里翻转为升序。
        out.reverse();
        Ok(out)
    }

    /// 复权因子（与日 K 同 ts_code 对齐）。返回 `(trade_date, adj_factor)` 升序。
    pub async fn fetch_adj_factor(
        &self,
        ts_code: &TsCode,
        start_date: &str,
        end_date: &str,
    ) -> Result<Vec<(TradeDate, f64)>, TushareError> {
        let params = json!({
            "ts_code": ts_code.as_str(),
            "start_date": start_date,
            "end_date": end_date,
        });
        let data = self
            .call("adj_factor", params, "ts_code,trade_date,adj_factor")
            .await?;
        let mut out = Vec::with_capacity(data.items.len());
        for row in data.items.iter() {
            let Some(td) = pick_str(&data.fields, row, "trade_date") else {
                continue;
            };
            let Ok(date) = TradeDate::parse(&td) else {
                continue;
            };
            let Some(adj) = pick_f64(&data.fields, row, "adj_factor") else {
                continue;
            };
            out.push((date, adj));
        }
        out.reverse();
        Ok(out)
    }

    /// 把不复权日 K + 复权因子转为指定 adjust 模式的 K。
    /// 公式：qfq = close * adj / latest_adj；hfq = close * adj。
    pub fn apply_adjust(
        bars: &[KlinePoint],
        factors: &[(TradeDate, f64)],
        adjust: Adjust,
    ) -> Vec<KlinePoint> {
        if matches!(adjust, Adjust::None) || factors.is_empty() {
            return bars.to_vec();
        }
        use std::collections::HashMap;
        let map: HashMap<TradeDate, f64> = factors.iter().copied().collect();
        let latest_adj = factors.last().map(|t| t.1).unwrap_or(1.0);
        bars.iter()
            .map(|p| {
                let Some(adj) = map.get(&p.date).copied() else {
                    return p.clone();
                };
                let factor = match adjust {
                    Adjust::Qfq => adj / latest_adj,
                    Adjust::Hfq => adj,
                    Adjust::None => 1.0,
                };
                let scale = |x: Price| -> Price {
                    let v = Decimal::from_f64(factor).unwrap_or(Decimal::ONE);
                    Price((x.0 * v).round_dp(4))
                };
                KlinePoint {
                    date: p.date,
                    open: scale(p.open),
                    close: scale(p.close),
                    high: scale(p.high),
                    low: scale(p.low),
                    volume: p.volume,
                    amount: p.amount,
                }
            })
            .collect()
    }

    /// `daily_basic` — 每日基础指标。
    pub async fn fetch_daily_basic(
        &self,
        ts_code: Option<&TsCode>,
        trade_date: Option<&str>,
    ) -> Result<Vec<DailyBasic>, TushareError> {
        let mut params = json!({});
        if let Some(c) = ts_code {
            params["ts_code"] = json!(c.as_str());
        }
        if let Some(d) = trade_date {
            params["trade_date"] = json!(d);
        }
        let data = self
            .call(
                "daily_basic",
                params,
                "ts_code,trade_date,pe,pe_ttm,pb,ps,ps_ttm,turnover_rate,turnover_rate_f,volume_ratio,total_mv,circ_mv",
            )
            .await?;
        let now = Utc::now();
        let mut out = Vec::with_capacity(data.items.len());
        for row in data.items.iter() {
            let Some(ts) = pick_str(&data.fields, row, "ts_code") else {
                continue;
            };
            let Ok(ts_code) = TsCode::parse(&ts) else {
                continue;
            };
            let Some(td) = pick_str(&data.fields, row, "trade_date") else {
                continue;
            };
            let Ok(date) = TradeDate::parse(&td) else { continue };
            let to_money = |v: f64| {
                // total_mv / circ_mv 单位是万元 → × 10000。
                Decimal::from_f64(v * 10000.0).map(|d| Money(d.round_dp(4)))
            };
            let total_mv = pick_f64(&data.fields, row, "total_mv").and_then(to_money);
            let circ_mv = pick_f64(&data.fields, row, "circ_mv").and_then(to_money);
            out.push(DailyBasic {
                ts_code,
                trade_date: date,
                pe: pick_f64(&data.fields, row, "pe"),
                pe_ttm: pick_f64(&data.fields, row, "pe_ttm"),
                pb: pick_f64(&data.fields, row, "pb"),
                ps: pick_f64(&data.fields, row, "ps"),
                ps_ttm: pick_f64(&data.fields, row, "ps_ttm"),
                turnover_rate: pick_f64(&data.fields, row, "turnover_rate").map(|v| v as Percent),
                turnover_rate_float: pick_f64(&data.fields, row, "turnover_rate_f")
                    .map(|v| v as Percent),
                volume_ratio: pick_f64(&data.fields, row, "volume_ratio"),
                total_mv,
                circ_mv,
                source: "tushare".to_string(),
                fetched_at: now,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::prelude::FromPrimitive;

    fn pt(date: &str, close: f64) -> KlinePoint {
        KlinePoint {
            date: TradeDate::parse(date).unwrap(),
            open: Price(Decimal::from_f64(close).unwrap()),
            close: Price(Decimal::from_f64(close).unwrap()),
            high: Price(Decimal::from_f64(close).unwrap()),
            low: Price(Decimal::from_f64(close).unwrap()),
            volume: Some(Volume(100)),
            amount: None,
        }
    }

    #[test]
    fn apply_adjust_none_returns_clone() {
        let bars = vec![pt("20260520", 100.0), pt("20260521", 110.0)];
        let factors = vec![(TradeDate::parse("20260521").unwrap(), 1.0)];
        let out = TushareClient::apply_adjust(&bars, &factors, Adjust::None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].close.0, Decimal::from_f64(100.0).unwrap());
    }

    fn dec(x: f64) -> Decimal {
        Decimal::from_f64(x).unwrap()
    }

    #[test]
    fn apply_adjust_qfq_scales_history_by_factor_ratio() {
        // factor 0.5 -> latest_adj 1.0 → qfq scale = 0.5；history close 100 → 50。
        let bars = vec![pt("20260520", 100.0), pt("20260521", 110.0)];
        let factors = vec![
            (TradeDate::parse("20260520").unwrap(), 0.5),
            (TradeDate::parse("20260521").unwrap(), 1.0),
        ];
        let out = TushareClient::apply_adjust(&bars, &factors, Adjust::Qfq);
        assert_eq!(out[0].close.0, dec(50.0));
        assert_eq!(out[1].close.0, dec(110.0));
    }

    #[test]
    fn apply_adjust_hfq_uses_factor_directly() {
        let bars = vec![pt("20260520", 100.0)];
        let factors = vec![(TradeDate::parse("20260520").unwrap(), 2.0)];
        let out = TushareClient::apply_adjust(&bars, &factors, Adjust::Hfq);
        assert_eq!(out[0].close.0, dec(200.0));
    }

    #[test]
    fn apply_adjust_missing_factor_keeps_bar_unchanged() {
        let bars = vec![pt("20260520", 100.0)];
        let factors = vec![(TradeDate::parse("20260521").unwrap(), 0.5)];
        let out = TushareClient::apply_adjust(&bars, &factors, Adjust::Qfq);
        assert_eq!(out[0].close.0, dec(100.0));
    }
}
