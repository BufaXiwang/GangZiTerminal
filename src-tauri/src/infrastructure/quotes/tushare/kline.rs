//! TuShare K 线 + 复权因子 + daily_basic。
//!
//! Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/tushare.md

use super::client::{pick_f64, pick_str, TushareClient, TushareError};
use crate::domain::quotes::{DailyBasic, KlinePeriod, KlinePoint};
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

    // 注：TuShare 的 `adj_factor` / `apply_adjust` 已移除——复权统一走本地 TDX xdxr
    // （`domain::quotes::apply_adjust`，service.rs）；K 线也不再由 TuShare 补段（2026-06-02 决策）。
    // TuShare 仅保留 enrich 角色（universe / daily_basic / 公司事件 / 交易日历）+ `fetch_kline`
    // 作准确性测试 oracle。

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

