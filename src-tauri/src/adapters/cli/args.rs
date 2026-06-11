//! `gangzi` CLI 参数解析（纯函数，零依赖，hermetic 可测）。
//!
//! Spec: docs/design/cli-module.md §3 命令集（全部只读）
//!
//! 子命令 → 端点路由 + 请求 JSON 的映射在这里完成；bin 只做「解析 → HTTP → 渲染」三段。

use serde_json::{json, Value as JsonValue};

/// 输出格式（spec §3：默认 JSON，`--format table` 给人看）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Json,
    Table,
}

/// 解析结果：发给端点的请求 + 渲染方式。
#[derive(Debug, Clone, PartialEq)]
pub struct CliRequest {
    /// `/v1/<route>`。
    pub route: &'static str,
    /// 子命令名（table 渲染按它分流）。
    pub command: &'static str,
    pub body: JsonValue,
    pub format: OutputFormat,
}

pub const USAGE: &str = "\
gangzi — GangZi 终端的只读查询 CLI（瘦客户端，需 app 正在运行）

用法:
  gangzi quote <ts_code>... [--basic]            实时报价快照（--basic 附每日指标）
  gangzi kline <ts_code> [--period day] [--limit 120]
                                                 K 线序列（day/week/month）
  gangzi scan [--sort change_pct_desc] [--limit 20] [--raw '<ScanMarketRequest JSON>']
                                                 市场扫描（--raw 透传完整请求）
  gangzi news [--query 词] [--source 名]... [--from ISO] [--to ISO] [--limit 20] [--article]
                                                 资讯检索（FTS / 来源 / 时间窗）
  gangzi account [--positions] [--orders] [--watchlist] [--events] [--triggers]
                                                 模拟账户快照（+各只读分区）

通用:
  --format json|table   输出格式（默认 json）
  --help                本帮助

退出码: 0 成功 · 1 app 侧业务错误 · 2 参数非法 · 3 app 未运行(app_not_running)
端点发现: $GANGZI_CLI_PORT 或 <appData>/cli.port（app 启动时写入）
";

/// 解析 argv（不含程序名）。`Err(message)` = 参数非法（exit 2）。
pub fn parse(argv: &[String]) -> Result<CliRequest, String> {
    if argv.is_empty() || argv[0] == "--help" || argv[0] == "-h" || argv[0] == "help" {
        return Err(USAGE.to_string());
    }
    let cmd = argv[0].as_str();
    let rest = &argv[1..];
    let (format, rest) = take_format(rest)?;

    match cmd {
        "quote" => {
            let (flags, codes) = split_flags(&rest);
            if codes.is_empty() {
                return Err("quote 需要至少一个 ts_code（如 600519.SH）".into());
            }
            for c in &codes {
                validate_ts_code(c)?;
            }
            let basic = has_flag(&flags, "--basic")?;
            Ok(CliRequest {
                route: "quotes",
                command: "quote",
                body: json!({
                    "tsCodes": codes,
                    "include": { "quote": true, "dailyBasic": basic }
                }),
                format,
            })
        }
        "kline" => {
            let (flags, codes) = split_flags(&rest);
            if codes.len() != 1 {
                return Err("kline 需要恰好一个 ts_code".into());
            }
            validate_ts_code(&codes[0])?;
            let period = flag_value(&flags, "--period")?.unwrap_or_else(|| "day".into());
            if !["day", "week", "month"].contains(&period.as_str()) {
                return Err(format!("非法 --period {period}（可选 day/week/month）"));
            }
            let limit: u32 = parse_u32(flag_value(&flags, "--limit")?.as_deref(), 120, "--limit")?;
            Ok(CliRequest {
                route: "quotes",
                command: "kline",
                body: json!({
                    "tsCodes": [codes[0]],
                    "include": { "klines": [period] },
                    "limit": { "kline": limit }
                }),
                format,
            })
        }
        "scan" => {
            let (flags, extra) = split_flags(&rest);
            if !extra.is_empty() {
                return Err(format!("scan 不接受位置参数: {extra:?}"));
            }
            if let Some(raw) = flag_value(&flags, "--raw")? {
                let v: JsonValue = serde_json::from_str(&raw)
                    .map_err(|e| format!("--raw 不是合法 JSON: {e}"))?;
                return Ok(CliRequest { route: "quotes", command: "scan", body: json!({ "scan": v }), format });
            }
            let sort = flag_value(&flags, "--sort")?.unwrap_or_else(|| "change_pct_desc".into());
            let limit: u32 = parse_u32(flag_value(&flags, "--limit")?.as_deref(), 20, "--limit")?;
            Ok(CliRequest {
                route: "quotes",
                command: "scan",
                body: json!({ "scan": { "sortBy": sort, "limit": limit } }),
                format,
            })
        }
        "news" => {
            let (flags, extra) = split_flags(&rest);
            if !extra.is_empty() {
                return Err(format!("news 不接受位置参数: {extra:?}（关键词用 --query）"));
            }
            let mut body = serde_json::Map::new();
            if let Some(q) = flag_value(&flags, "--query")? {
                body.insert("query".into(), json!(q));
            }
            let sources = flag_values(&flags, "--source");
            if !sources.is_empty() {
                body.insert("sources".into(), json!(sources));
            }
            if let Some(f) = flag_value(&flags, "--from")? {
                body.insert("publishedFrom".into(), json!(f));
            }
            if let Some(t) = flag_value(&flags, "--to")? {
                body.insert("publishedTo".into(), json!(t));
            }
            if has_flag(&flags, "--article")? {
                body.insert("includeArticle".into(), json!(true));
            }
            let limit: u32 = parse_u32(flag_value(&flags, "--limit")?.as_deref(), 20, "--limit")?;
            body.insert("limit".into(), json!(limit));
            Ok(CliRequest { route: "news", command: "news", body: JsonValue::Object(body), format })
        }
        "account" => {
            let (flags, extra) = split_flags(&rest);
            if !extra.is_empty() {
                return Err(format!("account 不接受位置参数: {extra:?}"));
            }
            let mut include = serde_json::Map::new();
            include.insert("snapshot".into(), json!(true));
            for (flag, key) in [
                ("--positions", "positions"),
                ("--orders", "orders"),
                ("--watchlist", "watchlist"),
                ("--events", "events"),
                ("--triggers", "triggers"),
            ] {
                if has_flag(&flags, flag)? {
                    include.insert(key.into(), json!(true));
                }
            }
            Ok(CliRequest {
                route: "account",
                command: "account",
                body: json!({ "include": include }),
                format,
            })
        }
        other => Err(format!("未知子命令: {other}（gangzi --help 查看用法）")),
    }
}

// ───────────────────────── flag helpers ─────────────────────────

/// 提取全局 `--format`，返回（格式，余下参数）。
fn take_format(rest: &[String]) -> Result<(OutputFormat, Vec<String>), String> {
    let mut out = Vec::new();
    let mut format = OutputFormat::Json;
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "--format" {
            let v = rest.get(i + 1).ok_or("--format 需要值（json|table）")?;
            format = match v.as_str() {
                "json" => OutputFormat::Json,
                "table" => OutputFormat::Table,
                x => return Err(format!("非法 --format {x}（可选 json|table）")),
            };
            i += 2;
        } else {
            out.push(rest[i].clone());
            i += 1;
        }
    }
    Ok((format, out))
}

/// 把参数分成（flag 序列，位置参数）。flag = `--x` 或 `--x 值`（值不可以 `--` 开头）。
fn split_flags(rest: &[String]) -> (Vec<(String, Option<String>)>, Vec<String>) {
    let mut flags = Vec::new();
    let mut pos = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        if rest[i].starts_with("--") {
            let val = rest
                .get(i + 1)
                .filter(|v| !v.starts_with("--"))
                .cloned();
            let step = if val.is_some() { 2 } else { 1 };
            flags.push((rest[i].clone(), val));
            i += step;
        } else {
            pos.push(rest[i].clone());
            i += 1;
        }
    }
    (flags, pos)
}

/// 布尔 flag：出现即 true；带了值报错（防误用）。
fn has_flag(flags: &[(String, Option<String>)], name: &str) -> Result<bool, String> {
    match flags.iter().find(|(f, _)| f == name) {
        Some((_, Some(v))) => Err(format!("{name} 不接受值（收到 {v:?}）")),
        Some((_, None)) => Ok(true),
        None => Ok(false),
    }
}

fn flag_value(flags: &[(String, Option<String>)], name: &str) -> Result<Option<String>, String> {
    match flags.iter().find(|(f, _)| f == name) {
        Some((_, Some(v))) => Ok(Some(v.clone())),
        Some((_, None)) => Err(format!("{name} 需要值")),
        None => Ok(None),
    }
}

fn flag_values(flags: &[(String, Option<String>)], name: &str) -> Vec<String> {
    flags
        .iter()
        .filter(|(f, _)| f == name)
        .filter_map(|(_, v)| v.clone())
        .collect()
}

fn parse_u32(v: Option<&str>, default: u32, name: &str) -> Result<u32, String> {
    match v {
        None => Ok(default),
        Some(s) => s.parse().map_err(|_| format!("{name} 需要正整数（收到 {s:?}）")),
    }
}

/// ts_code 形态校验（spec §5 invalid_input）：`600519.SH` / `000001.SZ` / `830799.BJ`。
fn validate_ts_code(c: &str) -> Result<(), String> {
    let ok = c.len() >= 8
        && c.chars().take(6).all(|ch| ch.is_ascii_digit())
        && matches!(&c[6..], ".SH" | ".SZ" | ".BJ");
    if ok {
        Ok(())
    } else {
        Err(format!("非法 ts_code: {c}（期望 6位数字.SH/.SZ/.BJ，如 600519.SH）"))
    }
}

// ───────────────────────── tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(|x| x.to_string()).collect()
    }

    #[test]
    fn quote_maps_to_fetch_data_body() {
        let r = parse(&argv("quote 600519.SH 000001.SZ")).unwrap();
        assert_eq!(r.route, "quotes");
        assert_eq!(r.body["tsCodes"], serde_json::json!(["600519.SH", "000001.SZ"]));
        assert_eq!(r.body["include"]["quote"], true);
        assert_eq!(r.format, OutputFormat::Json);
    }

    #[test]
    fn quote_rejects_bad_ts_code() {
        let e = parse(&argv("quote maotai")).unwrap_err();
        assert!(e.contains("非法 ts_code"), "{e}");
    }

    #[test]
    fn kline_defaults_and_period_check() {
        let r = parse(&argv("kline 600519.SH")).unwrap();
        assert_eq!(r.body["include"]["klines"], serde_json::json!(["day"]));
        assert_eq!(r.body["limit"]["kline"], 120);
        let e = parse(&argv("kline 600519.SH --period hour")).unwrap_err();
        assert!(e.contains("非法 --period"), "{e}");
    }

    #[test]
    fn scan_default_and_raw_passthrough() {
        let r = parse(&argv("scan --limit 5")).unwrap();
        assert_eq!(r.body["scan"]["sortBy"], "change_pct_desc");
        assert_eq!(r.body["scan"]["limit"], 5);
        let r = parse(&[
            "scan".into(),
            "--raw".into(),
            r#"{"sortBy":"amount_desc","limit":3}"#.into(),
        ])
        .unwrap();
        assert_eq!(r.body["scan"]["sortBy"], "amount_desc");
    }

    #[test]
    fn news_flags_compose_request() {
        let r = parse(&argv("news --query 茅台 --source 财联社 --article --limit 7")).unwrap();
        assert_eq!(r.route, "news");
        assert_eq!(r.body["query"], "茅台");
        assert_eq!(r.body["sources"], serde_json::json!(["财联社"]));
        assert_eq!(r.body["includeArticle"], true);
        assert_eq!(r.body["limit"], 7);
    }

    #[test]
    fn account_include_flags() {
        let r = parse(&argv("account --positions --orders")).unwrap();
        assert_eq!(r.body["include"]["snapshot"], true);
        assert_eq!(r.body["include"]["positions"], true);
        assert_eq!(r.body["include"]["orders"], true);
        assert!(r.body["include"].get("watchlist").is_none());
    }

    #[test]
    fn format_table_and_unknown_command() {
        let r = parse(&argv("account --format table")).unwrap();
        assert_eq!(r.format, OutputFormat::Table);
        let e = parse(&argv("trade 600519.SH")).unwrap_err();
        assert!(e.contains("未知子命令"), "{e}");
        // 写操作子命令在 CLI 层即不可达（spec 验收）。
        for w in ["buy", "sell", "order", "operate"] {
            assert!(parse(&argv(w)).is_err(), "{w} 必须不是合法子命令");
        }
    }

    #[test]
    fn help_returns_usage() {
        let e = parse(&argv("--help")).unwrap_err();
        assert!(e.contains("只读查询 CLI"), "{e}");
    }
}
