//! Eastmoney push2 / qt 行情 HTTP client。
//!
//! Spec: docs/design/quotes-module.md §5；docs/design/references/quotes/eastmoney.md
//!
//! 设计：单 reqwest::Client 复用；timeout 5s（quote）/ 8s（kline）；retry 1 次。
//! 失败返回 `EmError`；调用方按 spec 把它翻译成 `provider_partial_failure` / item warning。

use crate::domain::quotes::{KlinePoint, MinuteKlinePeriod, MinuteKlinePoint};
use crate::domain::shared::{Amount, Price, TimestampMs, TradeDate, TsCode, Volume};
use chrono::{NaiveDate, TimeZone, Utc};
use chrono_tz::Asia::Shanghai;
use reqwest::Client;
use rust_decimal::{prelude::FromPrimitive, Decimal};
use serde::Deserialize;
use std::time::Duration;
use thiserror::Error;

const KLINE_TIMEOUT: Duration = Duration::from_secs(8);
const USER_AGENT: &str = "Mozilla/5.0 GangZi/0.1";

#[derive(Debug, Error)]
pub enum EmError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("provider returned no data")]
    Empty,
}

#[derive(Clone)]
pub struct EastmoneyProvider {
    client: Client,
}

impl EastmoneyProvider {
    pub fn new() -> reqwest::Result<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(KLINE_TIMEOUT)
            .build()?;
        Ok(Self { client })
    }

    /// 拉分钟 K（push2 kline）。
    pub async fn fetch_minute_kline(
        &self,
        ts_code: &TsCode,
        period: MinuteKlinePeriod,
        count: u32,
    ) -> Result<Vec<MinuteKlinePoint>, EmError> {
        let secid = secid_of(ts_code);
        let klt = match period {
            MinuteKlinePeriod::M1 => 1,
            MinuteKlinePeriod::M5 => 5,
            MinuteKlinePeriod::M15 => 15,
            MinuteKlinePeriod::M30 => 30,
            MinuteKlinePeriod::M60 => 60,
        };
        let url = format!(
            "https://push2his.eastmoney.com/api/qt/stock/kline/get?secid={}&fields1=f1,f2,f3,f4,f5,f6&fields2=f51,f52,f53,f54,f55,f56,f57,f58&klt={}&fqt=0&lmt={}",
            secid, klt, count
        );
        #[derive(Deserialize)]
        struct Resp {
            data: Option<KlineData>,
        }
        #[derive(Deserialize)]
        struct KlineData {
            #[serde(default)]
            klines: Vec<String>,
        }
        let resp: Resp = self
            .client
            .get(&url)
            .timeout(KLINE_TIMEOUT)
            .send()
            .await?
            .json()
            .await
            .map_err(|e| EmError::Parse(e.to_string()))?;
        let data = resp.data.ok_or(EmError::Empty)?;
        let mut out = Vec::with_capacity(data.klines.len());
        for line in data.klines.iter() {
            // 格式： "YYYY-MM-DD HH:MM,open,close,high,low,volume,amount,..."
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() < 7 {
                continue;
            }
            let dt = parts[0];
            let nd = if dt.len() >= 16 {
                let date_part = &dt[..10];
                let time_part = &dt[11..16];
                let nd = NaiveDate::parse_from_str(date_part, "%Y-%m-%d").ok();
                let (hh, mm) = time_part
                    .split_once(':')
                    .and_then(|(h, m)| Some((h.parse::<u32>().ok()?, m.parse::<u32>().ok()?)))
                    .unwrap_or((0, 0));
                nd.and_then(|d| d.and_hms_opt(hh, mm, 0))
            } else {
                continue;
            };
            let Some(naive) = nd else { continue };
            let Some(utc) = Shanghai.from_local_datetime(&naive).single().map(|t| t.with_timezone(&Utc)) else {
                continue;
            };
            let timestamp_ms: TimestampMs = utc.timestamp_millis();
            let parse_p = |s: &str| -> Option<Price> {
                s.parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .and_then(|v| Decimal::from_f64(v).map(|d| Price(d.round_dp(4))))
            };
            let Some(open) = parse_p(parts[1]) else { continue };
            let Some(close) = parse_p(parts[2]) else { continue };
            let Some(high) = parse_p(parts[3]) else { continue };
            let Some(low) = parse_p(parts[4]) else { continue };
            // EM minute volume 单位是手；× 100 转股。
            let volume = parts[5]
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
                .map(|v| Volume((v * 100.0) as i64))
                .unwrap_or(Volume(0));
            let amount = parts[6]
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
                .and_then(|v| Decimal::from_f64(v).map(|d| Amount(d.round_dp(4))))
                .unwrap_or(Amount(Decimal::ZERO));
            out.push(MinuteKlinePoint {
                timestamp_ms,
                open,
                close,
                high,
                low,
                volume,
                amount,
            });
        }
        Ok(out)
    }

    /// 当日分时（trends2 接口）。
    pub async fn fetch_intraday(
        &self,
        ts_code: &TsCode,
        trade_date: TradeDate,
    ) -> Result<Vec<(String, Price, Option<Volume>, Option<Amount>)>, EmError> {
        let secid = secid_of(ts_code);
        let url = format!(
            "https://push2his.eastmoney.com/api/qt/stock/trends2/get?secid={}&fields1=f1,f2,f3,f4,f5&fields2=f51,f53,f56,f58&iscr=0&ndays=1",
            secid
        );
        #[derive(Deserialize)]
        struct Resp {
            data: Option<TrendsData>,
        }
        #[derive(Deserialize)]
        struct TrendsData {
            #[serde(default)]
            trends: Vec<String>,
        }
        let resp: Resp = self
            .client
            .get(&url)
            .timeout(KLINE_TIMEOUT)
            .send()
            .await?
            .json()
            .await
            .map_err(|e| EmError::Parse(e.to_string()))?;
        let data = resp.data.ok_or(EmError::Empty)?;
        let _ = trade_date;
        let mut out = Vec::with_capacity(data.trends.len());
        for line in data.trends.iter() {
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() < 4 {
                continue;
            }
            // "YYYY-MM-DD HH:MM,price,volume,amount"
            let time_str = if parts[0].len() >= 16 {
                parts[0][11..16].to_string()
            } else {
                parts[0].to_string()
            };
            let Some(price) = parts[1]
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v > 0.0)
                .and_then(|v| Decimal::from_f64(v).map(|d| Price(d.round_dp(4))))
            else {
                continue;
            };
            let volume = parts[2]
                .parse::<f64>()
                .ok()
                .filter(|v| *v > 0.0)
                .map(|v| Volume((v * 100.0) as i64));
            let amount = parts[3]
                .parse::<f64>()
                .ok()
                .filter(|v| *v > 0.0)
                .and_then(|v| Decimal::from_f64(v).map(|d| Amount(d.round_dp(4))));
            out.push((time_str, price, volume, amount));
        }
        Ok(out)
    }

    /// 日线 K（用作 fallback；TuShare 不可用时）。
    pub async fn fetch_daily_kline(
        &self,
        ts_code: &TsCode,
        count: u32,
    ) -> Result<Vec<KlinePoint>, EmError> {
        let secid = secid_of(ts_code);
        let url = format!(
            "https://push2his.eastmoney.com/api/qt/stock/kline/get?secid={}&fields1=f1,f2,f3,f4,f5,f6&fields2=f51,f52,f53,f54,f55,f56,f57,f58&klt=101&fqt=0&lmt={}",
            secid, count
        );
        #[derive(Deserialize)]
        struct Resp {
            data: Option<KlineData>,
        }
        #[derive(Deserialize)]
        struct KlineData {
            #[serde(default)]
            klines: Vec<String>,
        }
        let resp: Resp = self
            .client
            .get(&url)
            .timeout(KLINE_TIMEOUT)
            .send()
            .await?
            .json()
            .await
            .map_err(|e| EmError::Parse(e.to_string()))?;
        let data = resp.data.ok_or(EmError::Empty)?;
        let mut out = Vec::with_capacity(data.klines.len());
        for line in data.klines.iter() {
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() < 7 {
                continue;
            }
            let date = match NaiveDate::parse_from_str(parts[0], "%Y-%m-%d") {
                Ok(d) => TradeDate::from_naive(d),
                Err(_) => continue,
            };
            let parse_p = |s: &str| -> Option<Price> {
                s.parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite() && *v > 0.0)
                    .and_then(|v| Decimal::from_f64(v).map(|d| Price(d.round_dp(4))))
            };
            let Some(open) = parse_p(parts[1]) else { continue };
            let Some(close) = parse_p(parts[2]) else { continue };
            let Some(high) = parse_p(parts[3]) else { continue };
            let Some(low) = parse_p(parts[4]) else { continue };
            let volume = parts[5]
                .parse::<f64>()
                .ok()
                .filter(|v| *v > 0.0)
                .map(|v| Volume((v * 100.0) as i64));
            let amount = parts[6]
                .parse::<f64>()
                .ok()
                .filter(|v| *v > 0.0)
                .and_then(|v| Decimal::from_f64(v).map(|d| Amount(d.round_dp(4))));
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
        Ok(out)
    }

    /// 拉取 BJ universe（北交所主板）。
    ///
    /// Spec: quotes-module.md §5 line 729 — Eastmoney 补 BJ。
    /// 使用 `qt/clist/get` 的 `m:0+t:81+s:2048` 筛选；返回 (code6, name)。
    pub async fn fetch_bj_universe(&self) -> Result<Vec<(String, String)>, EmError> {
        // 单页足够 (BJ 标的总数 ~250)；pz=500 保险。
        let url = "https://82.push2.eastmoney.com/api/qt/clist/get?pn=1&pz=500\
            &fid=f12&fs=m:0+t:81+s:2048&fields=f12,f14";
        let body = self
            .client
            .get(url)
            .timeout(KLINE_TIMEOUT)
            .send()
            .await?
            .text()
            .await?;
        parse_bj_universe(&body)
    }
}

#[derive(Deserialize)]
struct BjUniverseResp {
    data: Option<BjUniverseData>,
}

#[derive(Deserialize)]
struct BjUniverseData {
    // EM clist/get 的 `diff` 在不同 host / 版本下可能是数组 `[{...}]` 或以数字字符串为键的
    // 对象 `{"0":{...},"1":{...}}`（live 2026-05-30 观测到对象形态）。兼容两种形态。
    #[serde(default, deserialize_with = "de_diff")]
    diff: Vec<BjRow>,
}

#[derive(Deserialize)]
struct BjRow {
    #[serde(default)]
    f12: Option<String>,
    #[serde(default)]
    f14: Option<String>,
}

/// 接受 `diff` 为数组或对象（数字键 map）→ 统一成 `Vec<BjRow>`。
/// 对象形态按数字键升序排序，保持与数组形态一致的稳定顺序。
fn de_diff<'de, D>(deserializer: D) -> Result<Vec<BjRow>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum DiffShape {
        Array(Vec<BjRow>),
        Map(std::collections::HashMap<String, BjRow>),
        Null,
    }
    Ok(match DiffShape::deserialize(deserializer)? {
        DiffShape::Array(v) => v,
        DiffShape::Map(m) => {
            let mut entries: Vec<(i64, BjRow)> = m
                .into_iter()
                .map(|(k, row)| (k.parse::<i64>().unwrap_or(i64::MAX), row))
                .collect();
            entries.sort_by_key(|(k, _)| *k);
            entries.into_iter().map(|(_, row)| row).collect()
        }
        DiffShape::Null => Vec::new(),
    })
}

/// 纯函数：解析 EM clist/get 响应体 → (code6, name) 列表（无 I/O，可单测）。
fn parse_bj_universe(body: &str) -> Result<Vec<(String, String)>, EmError> {
    let resp: BjUniverseResp =
        serde_json::from_str(body).map_err(|e| EmError::Parse(e.to_string()))?;
    let data = resp.data.ok_or(EmError::Empty)?;
    let mut out = Vec::with_capacity(data.diff.len());
    for r in data.diff.into_iter() {
        if let (Some(code), Some(name)) = (r.f12, r.f14) {
            if code.len() == 6 && code.chars().all(|c| c.is_ascii_digit()) {
                out.push((code, name));
            }
        }
    }
    Ok(out)
}

/// EM secid 映射。
///
/// 市场前缀：SH=1，SZ=0，BJ=0。
///
/// BJ 前缀证据（Spec finding #2）：EM push2 对北交所使用 market=0 前缀（与 SZ 同），
/// `2.<code>` 实测无数据。审计在可用环境（2026-05-30）下用 `0.<code>` 拿到了 payload。
/// 故保留 BJ → `0.`。注意：此前缀无法在当前沙箱内复测（EM /api/qt/* 出口被屏蔽，
/// HTTP 层无响应），如未来 EM 改规则需重新 live 验证 920 系新代码。
/// EM 已退出实时报价路径（spec §5），secid 仅用于 K 线 / 分时 / 日线 / universe。
pub(crate) fn secid_of(ts_code: &TsCode) -> String {
    let market = match ts_code.market() {
        crate::domain::shared::Market::SH => "1",
        crate::domain::shared::Market::SZ => "0",
        crate::domain::shared::Market::BJ => "0",
    };
    format!("{}.{}", market, &ts_code.as_str()[..6])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn em_secid_sh_maps_to_1_prefix() {
        let c = TsCode::parse("600519.SH").unwrap();
        assert_eq!(secid_of(&c), "1.600519");
    }

    #[test]
    fn em_secid_sz_maps_to_0_prefix() {
        let c = TsCode::parse("000001.SZ").unwrap();
        assert_eq!(secid_of(&c), "0.000001");
    }

    #[test]
    fn em_secid_bj_maps_to_0_prefix() {
        let c = TsCode::parse("430047.BJ").unwrap();
        assert_eq!(secid_of(&c), "0.430047");
    }

    // FIX B: diff 为对象（数字键 map）形态应被解析。
    #[test]
    fn em_bj_universe_parses_object_diff() {
        let json = r#"{"data":{"diff":{
            "0":{"f12":"810011","f14":"优机定转"},
            "1":{"f12":"810013","f14":"万通定转"}
        }}}"#;
        let out = parse_bj_universe(json).unwrap();
        assert_eq!(
            out,
            vec![
                ("810011".to_string(), "优机定转".to_string()),
                ("810013".to_string(), "万通定转".to_string()),
            ]
        );
    }

    // FIX B: diff 为数组形态仍应被解析（向后兼容）。
    #[test]
    fn em_bj_universe_parses_array_diff() {
        let json = r#"{"data":{"diff":[
            {"f12":"430047","f14":"诺思兰德"},
            {"f12":"920019","f14":"某新代码"}
        ]}}"#;
        let out = parse_bj_universe(json).unwrap();
        assert_eq!(
            out,
            vec![
                ("430047".to_string(), "诺思兰德".to_string()),
                ("920019".to_string(), "某新代码".to_string()),
            ]
        );
    }

    // FIX B: diff 缺失 / null 时返回空列表，不报错。
    #[test]
    fn em_bj_universe_tolerates_missing_diff() {
        assert!(parse_bj_universe(r#"{"data":{}}"#).unwrap().is_empty());
        assert!(parse_bj_universe(r#"{"data":{"diff":null}}"#)
            .unwrap()
            .is_empty());
    }
}
