//! `--format table` 的人类可读渲染（spec §3）。
//!
//! 只做轻量列对齐，不引入表格库；JSON 是默认且稳定的脚本接口，table 仅供人眼。
//! 渲染失败 / 结构不认识时回退打印 JSON（绝不丢数据）。

use serde_json::Value as JsonValue;

/// 按子命令渲染 table；无法识别的结构回退 pretty JSON。
pub fn render_table(command: &str, v: &JsonValue) -> String {
    let out = match command {
        "quote" => quote_table(v),
        "kline" => kline_table(v),
        "scan" => scan_table(v),
        "news" => news_table(v),
        "account" => account_table(v),
        _ => None,
    };
    out.unwrap_or_else(|| serde_json::to_string_pretty(v).unwrap_or_default())
}

fn s<'a>(v: &'a JsonValue, keys: &[&str]) -> &'a str {
    keys.iter().find_map(|k| v.get(*k).and_then(|x| x.as_str())).unwrap_or("-")
}

fn n(v: &JsonValue, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| v.get(*k))
        .map(|x| match x {
            JsonValue::String(t) => t.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "-".into())
}

fn row(cols: &[(&str, usize)]) -> String {
    cols.iter()
        .map(|(text, w)| format!("{:<width$}", text, width = w))
        .collect::<Vec<_>>()
        .join("  ")
}

fn quote_table(v: &JsonValue) -> Option<String> {
    let items = v.get("items")?.as_array()?;
    let mut lines = vec![row(&[("代码", 10), ("现价", 10), ("涨跌%", 8), ("新鲜度", 8)])];
    for it in items {
        let q = it.get("quote").unwrap_or(it);
        let fresh = q
            .get("freshness")
            .and_then(|f| f.get("status"))
            .and_then(|x| x.as_str())
            .unwrap_or("-");
        lines.push(row(&[
            (s(it, &["tsCode"]), 10),
            (n(q, &["price", "lastPrice"]).as_str(), 10),
            (n(q, &["changePercent", "changePct"]).as_str(), 8),
            (fresh, 8),
        ]));
    }
    Some(lines.join("\n"))
}

fn kline_table(v: &JsonValue) -> Option<String> {
    // 实际形态（FetchDataResponse）：items[0].klines = { "<period>": { points: [...] } }。
    let item = v.get("items")?.as_array()?.first()?;
    let klines = item.get("klines")?;
    let series = klines
        .as_object()
        .and_then(|m| m.values().next())
        .or_else(|| klines.as_array().and_then(|a| a.first()))?;
    let bars = series
        .get("points")
        .or_else(|| series.get("bars"))
        .or_else(|| series.get("items"))?
        .as_array()?;
    let mut lines = vec![row(&[("日期", 12), ("开", 9), ("高", 9), ("低", 9), ("收", 9), ("量", 12)])];
    for b in bars {
        lines.push(row(&[
            (s(b, &["tradeDate", "date", "ts"]), 12),
            (n(b, &["open"]).as_str(), 9),
            (n(b, &["high"]).as_str(), 9),
            (n(b, &["low"]).as_str(), 9),
            (n(b, &["close"]).as_str(), 9),
            (n(b, &["volume", "vol"]).as_str(), 12),
        ]));
    }
    Some(lines.join("\n"))
}

fn scan_table(v: &JsonValue) -> Option<String> {
    // 实际形态（ScanMarketResponse）：items[].{tsCode,name,quote:{price,changePercent,...}}。
    let rows_v = v.get("rows").or_else(|| v.get("items"))?.as_array()?;
    let mut lines = vec![row(&[("代码", 10), ("名称", 12), ("现价", 10), ("涨跌%", 8)])];
    for r in rows_v {
        let q = r.get("quote").unwrap_or(r);
        lines.push(row(&[
            (s(r, &["tsCode"]), 10),
            (s(r, &["name"]), 12),
            (n(q, &["price", "lastPrice"]).as_str(), 10),
            (n(q, &["changePercent", "changePct"]).as_str(), 8),
        ]));
    }
    Some(lines.join("\n"))
}

fn news_table(v: &JsonValue) -> Option<String> {
    let items = v.get("items")?.as_array()?;
    let mut lines = Vec::new();
    for it in items {
        let t = s(it, &["publishedAt", "fetchedAt"]);
        let src = s(it, &["source"]);
        let title = s(it, &["title"]);
        lines.push(format!("{t}  [{src}]  {title}"));
    }
    Some(lines.join("\n"))
}

fn account_table(v: &JsonValue) -> Option<String> {
    let mut lines = Vec::new();
    if let Some(snap) = v.get("snapshot") {
        lines.push(format!(
            "现金 {}  总资产 {}  持仓市值 {}",
            n(snap, &["cash"]),
            n(snap, &["totalAssets", "equity"]),
            n(snap, &["positionsValue", "marketValue"]),
        ));
    }
    if let Some(pos) = v.get("positions").and_then(|p| p.as_array()) {
        lines.push(String::new());
        lines.push(row(&[("持仓", 10), ("数量", 8), ("成本", 10), ("现价", 10), ("浮动盈亏", 12)]));
        for p in pos {
            lines.push(row(&[
                (s(p, &["tsCode"]), 10),
                (n(p, &["quantity", "qty"]).as_str(), 8),
                (n(p, &["avgCost", "cost"]).as_str(), 10),
                (n(p, &["lastPrice", "price"]).as_str(), 10),
                (n(p, &["unrealizedPnl", "pnl"]).as_str(), 12),
            ]));
        }
    }
    if let Some(orders) = v.get("orders").and_then(|o| o.as_array()) {
        lines.push(String::new());
        lines.push(row(&[("挂单", 10), ("方向", 6), ("数量", 8), ("价格", 10), ("状态", 10)]));
        for o in orders {
            lines.push(row(&[
                (s(o, &["tsCode"]), 10),
                (s(o, &["side", "direction"]), 6),
                (n(o, &["quantity", "qty"]).as_str(), 8),
                (n(o, &["price", "limitPrice"]).as_str(), 10),
                (s(o, &["status"]), 10),
            ]));
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn quote_table_renders_rows() {
        let v = json!({"items":[{"tsCode":"600519.SH","quote":{"price":"1820.5","changePercent":"1.2","freshness":{"status":"fresh"}}}]});
        let t = render_table("quote", &v);
        assert!(t.contains("600519.SH") && t.contains("1820.5") && t.contains("fresh"), "{t}");
    }

    #[test]
    fn unknown_shape_falls_back_to_json() {
        let v = json!({"weird": 1});
        let t = render_table("quote", &v);
        assert!(t.contains("\"weird\""), "回退 JSON: {t}");
    }

    #[test]
    fn account_table_renders_snapshot_and_positions() {
        let v = json!({
            "snapshot": {"cash":"900000","totalAssets":"1010000"},
            "positions": [{"tsCode":"600519.SH","quantity":100,"avgCost":"1700","lastPrice":"1820.5","unrealizedPnl":"12050"}]
        });
        let t = render_table("account", &v);
        assert!(t.contains("900000") && t.contains("600519.SH"), "{t}");
    }
}
