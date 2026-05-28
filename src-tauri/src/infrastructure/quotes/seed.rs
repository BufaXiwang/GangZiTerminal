//! Builtin universe seed — cold-start 内置标的种子，启动时 upsert 进 `quote_instruments`，
//! 让 UI 第一帧就能看到一个非空市场列表（TDX 全市场拉取 ~10s 期间的覆盖）。
//!
//! Spec: docs/design/quotes-module.md §5 universe (cold-start seed —
//! spec drift: seed concept not explicitly covered, see PR notes)
//!
//! 设计约束：
//! - 仅 hardcode (code6, market, name)；(category, board) 由 `universe::classify` 推。
//! - 不写 `list_date` / `industry` / `is_st` 等，留给后续 TDX/TuShare refresh enrich。
//! - 不确定的 code 不要瞎填——错的 code 会污染 universe 持久层。
//! - `InstrumentSource::Builtin`（spec §5 step 0：仅作 cold-start diagnostic；后续 TDX/EM/TuShare
//!   refresh upsert 时 `source` 会被覆盖为对应 provider）。
//!
//! 依赖方向：本文件只 use `domain` + 同层 `universe`，不引 pipeline / adapters。

use crate::domain::quotes::{InstrumentSource, MarketInstrument};
use crate::domain::shared::{InstrumentStatus, Market, TsCode};
use crate::infrastructure::quotes::repository::QuotesRepository;
use crate::infrastructure::quotes::universe::classify;
use chrono::Utc;

/// `(code6, market, name)`。Market 显式给——code6 无法唯一推 market（例如 `000001`
/// 在 SH 是上证综指、在 SZ 是平安银行）。
pub const BUILTIN_INSTRUMENTS: &[(&str, Market, &str)] = &[
    // ---- 主流指数 ----
    ("000001", Market::SH, "上证综指"),
    ("000016", Market::SH, "上证50"),
    ("000300", Market::SH, "沪深300"),
    ("000688", Market::SH, "科创50"),
    ("000905", Market::SH, "中证500"),
    ("000852", Market::SH, "中证1000"),
    ("000015", Market::SH, "上证红利"),
    ("000010", Market::SH, "上证180"),
    ("000009", Market::SH, "上证380"),
    ("399001", Market::SZ, "深证成指"),
    ("399006", Market::SZ, "创业板指"),
    ("399005", Market::SZ, "中小板指"),
    // ---- 沪市蓝筹 ----
    ("600519", Market::SH, "贵州茅台"),
    ("600036", Market::SH, "招商银行"),
    ("601398", Market::SH, "工商银行"),
    ("601318", Market::SH, "中国平安"),
    ("600900", Market::SH, "长江电力"),
    ("601012", Market::SH, "隆基绿能"),
    ("600309", Market::SH, "万华化学"),
    ("601088", Market::SH, "中国神华"),
    ("600031", Market::SH, "三一重工"),
    ("600276", Market::SH, "恒瑞医药"),
    ("600436", Market::SH, "片仔癀"),
    ("600585", Market::SH, "海螺水泥"),
    ("600887", Market::SH, "伊利股份"),
    ("603259", Market::SH, "药明康德"),
    ("603288", Market::SH, "海天味业"),
    ("603501", Market::SH, "韦尔股份"),
    ("601888", Market::SH, "中国中免"),
    ("601166", Market::SH, "兴业银行"),
    ("601628", Market::SH, "中国人寿"),
    ("600028", Market::SH, "中国石化"),
    ("601857", Market::SH, "中国石油"),
    ("601288", Market::SH, "农业银行"),
    ("601988", Market::SH, "中国银行"),
    ("600000", Market::SH, "浦发银行"),
    ("600030", Market::SH, "中信证券"),
    // ---- 深市蓝筹 ----
    ("000858", Market::SZ, "五粮液"),
    ("300750", Market::SZ, "宁德时代"),
    ("002594", Market::SZ, "比亚迪"),
    ("000333", Market::SZ, "美的集团"),
    ("002415", Market::SZ, "海康威视"),
    ("002475", Market::SZ, "立讯精密"),
    ("000001", Market::SZ, "平安银行"),
    ("000002", Market::SZ, "万科A"),
    ("000725", Market::SZ, "京东方A"),
    ("000651", Market::SZ, "格力电器"),
    ("002714", Market::SZ, "牧原股份"),
    ("300059", Market::SZ, "东方财富"),
    ("300760", Market::SZ, "迈瑞医疗"),
    ("000063", Market::SZ, "中兴通讯"),
    ("000568", Market::SZ, "泸州老窖"),
    ("002230", Market::SZ, "科大讯飞"),
    ("002304", Market::SZ, "洋河股份"),
    // ---- 沪市 ETF ----
    ("510050", Market::SH, "上证50ETF"),
    ("510300", Market::SH, "沪深300ETF"),
    ("510500", Market::SH, "中证500ETF"),
    ("510880", Market::SH, "红利ETF"),
    ("512170", Market::SH, "医疗ETF"),
    ("515030", Market::SH, "新能源车ETF"),
    ("515050", Market::SH, "5G通信ETF"),
    ("515790", Market::SH, "光伏ETF"),
    ("588000", Market::SH, "科创50ETF"),
    ("518880", Market::SH, "黄金ETF"),
    ("512880", Market::SH, "证券ETF"),
    ("512690", Market::SH, "酒ETF"),
    ("512660", Market::SH, "军工ETF"),
    // ---- 深市 ETF ----
    ("159915", Market::SZ, "创业板ETF"),
    ("159919", Market::SZ, "沪深300ETF"),
    ("159928", Market::SZ, "消费ETF"),
    ("159995", Market::SZ, "半导体ETF"),
    ("159949", Market::SZ, "创业板50ETF"),
    ("159992", Market::SZ, "创新药ETF"),
];

/// 把 `BUILTIN_INSTRUMENTS` 翻成 `MarketInstrument`，未识别的 code 跳过并日志告警。
fn build_instruments() -> Vec<MarketInstrument> {
    let now = Utc::now();
    let mut out = Vec::with_capacity(BUILTIN_INSTRUMENTS.len());
    for (code6, market, name) in BUILTIN_INSTRUMENTS {
        let cls = match classify(*market, code6) {
            Some(c) => c,
            None => {
                tracing::warn!(
                    target: "quotes.seed",
                    code = code6,
                    market = ?market,
                    "builtin seed code not recognised by universe::classify; skipped"
                );
                continue;
            }
        };
        let suffix = match market {
            Market::SH => "SH",
            Market::SZ => "SZ",
            Market::BJ => "BJ",
        };
        let ts_raw = format!("{}.{}", code6, suffix);
        let ts_code = match TsCode::parse(&ts_raw) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: "quotes.seed",
                    code = ts_raw,
                    error = %e,
                    "builtin seed ts_code parse failed; skipped"
                );
                continue;
            }
        };
        out.push(MarketInstrument {
            ts_code,
            name: (*name).to_string(),
            category: cls.category,
            market: *market,
            board: cls.board.map(|s| s.to_string()),
            sector: None,
            // 默认 Listed；后续 TuShare enrich 时会按 list_status 覆盖。
            status: Some(InstrumentStatus::Listed),
            is_st: None,
            publisher: None,
            index_category: None,
            fund_type: None,
            management: None,
            list_date: None,
            source: InstrumentSource::Builtin,
            updated_at: now,
        });
    }
    out
}

/// Upsert 内置 seed 到 `quote_instruments`。返回成功 upsert 的条数。
///
/// 同步、阻塞、幂等。调用方应在 `refresh_market_instruments` 异步任务启动**之前**完成。
pub fn seed_builtin_instruments(repo: &QuotesRepository<'_>) -> rusqlite::Result<usize> {
    let items = build_instruments();
    let count = items.len();
    repo.upsert_instruments(&items)?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shared::InstrumentCategory;
    use crate::infrastructure::db::{run_migrations, AppDb};
    use crate::infrastructure::quotes::migrations as quotes_migrations;
    use std::collections::HashSet;

    fn make_db() -> AppDb {
        let db = AppDb::open_in_memory().unwrap();
        db.with(|conn| run_migrations(conn, quotes_migrations()).unwrap());
        db
    }

    #[test]
    fn builtin_list_is_non_trivial_size() {
        // 防止意外删空。下限 60 表示种子至少要覆盖核心指数 + 蓝筹 + ETF 主要部分。
        assert!(
            BUILTIN_INSTRUMENTS.len() >= 60,
            "seed size dropped below 60: {}",
            BUILTIN_INSTRUMENTS.len()
        );
    }

    #[test]
    fn builtin_codes_are_unique_within_market() {
        let mut seen: HashSet<(Market, &str)> = HashSet::new();
        for (code6, market, _) in BUILTIN_INSTRUMENTS {
            assert!(
                seen.insert((*market, *code6)),
                "duplicate (market, code) in BUILTIN_INSTRUMENTS: {:?} {}",
                market,
                code6
            );
        }
    }

    #[test]
    fn every_builtin_code_classifies_successfully() {
        // 任何 code 走 universe::classify 失败 => 说明 hardcode 写错了
        for (code6, market, name) in BUILTIN_INSTRUMENTS {
            assert!(
                classify(*market, code6).is_some(),
                "builtin seed unrecognised: {} {:?} ({})",
                code6,
                market,
                name
            );
        }
    }

    #[test]
    fn build_instruments_matches_const_size() {
        // 编译期 const 不该有 unrecognised entries（上一个测试已保证）
        assert_eq!(build_instruments().len(), BUILTIN_INSTRUMENTS.len());
    }

    #[test]
    fn category_distribution_makes_sense() {
        let items = build_instruments();
        let n_idx = items
            .iter()
            .filter(|i| i.category == InstrumentCategory::Index)
            .count();
        let n_stock = items
            .iter()
            .filter(|i| i.category == InstrumentCategory::Stock)
            .count();
        let n_fund = items
            .iter()
            .filter(|i| i.category == InstrumentCategory::Fund)
            .count();
        assert!(n_idx >= 8, "expect >=8 indexes, got {}", n_idx);
        assert!(n_stock >= 25, "expect >=25 stocks, got {}", n_stock);
        assert!(n_fund >= 10, "expect >=10 funds, got {}", n_fund);
    }

    #[test]
    fn seed_upsert_writes_expected_count() {
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        let n = seed_builtin_instruments(&repo).unwrap();
        assert_eq!(n, BUILTIN_INSTRUMENTS.len());
        // 抽样验证：上证综指 + 贵州茅台 + 沪深300ETF 必须能取到
        let maotai = TsCode::parse("600519.SH").unwrap();
        assert!(repo.get_instrument(&maotai).unwrap().is_some());
        let sh_index = TsCode::parse("000001.SH").unwrap();
        let got = repo.get_instrument(&sh_index).unwrap().unwrap();
        assert_eq!(got.category, InstrumentCategory::Index);
        let etf = TsCode::parse("510300.SH").unwrap();
        let got = repo.get_instrument(&etf).unwrap().unwrap();
        assert_eq!(got.category, InstrumentCategory::Fund);
    }

    #[test]
    fn seed_upsert_is_idempotent() {
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        seed_builtin_instruments(&repo).unwrap();
        seed_builtin_instruments(&repo).unwrap();
        // 第二次 upsert 不应导致重复行：count 仍等于 seed size
        let (_, total) = repo.list_instruments(None, None, 1000, 0).unwrap();
        assert_eq!(total as usize, BUILTIN_INSTRUMENTS.len());
    }

    #[test]
    fn seed_rows_have_builtin_source() {
        // Spec §5 step 0：seed 行写入时 source = "builtin"，diagnostic only。
        // 真实 provider refresh 后会被覆盖；测试这里只验证 seed 阶段。
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        seed_builtin_instruments(&repo).unwrap();
        let maotai = TsCode::parse("600519.SH").unwrap();
        let got = repo.get_instrument(&maotai).unwrap().unwrap();
        assert_eq!(
            got.source,
            InstrumentSource::Builtin,
            "seed row must carry InstrumentSource::Builtin"
        );
    }

    #[test]
    fn ping_an_bank_and_sh_index_disambiguated_by_market() {
        // code6 = "000001" 在 SH 是上证综指，在 SZ 是平安银行——验证 market 区分有效
        let db = make_db();
        let repo = QuotesRepository::new(&db);
        seed_builtin_instruments(&repo).unwrap();
        let sh = repo
            .get_instrument(&TsCode::parse("000001.SH").unwrap())
            .unwrap()
            .unwrap();
        let sz = repo
            .get_instrument(&TsCode::parse("000001.SZ").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(sh.category, InstrumentCategory::Index);
        assert_eq!(sz.category, InstrumentCategory::Stock);
        assert_eq!(sh.name, "上证综指");
        assert_eq!(sz.name, "平安银行");
    }
}
