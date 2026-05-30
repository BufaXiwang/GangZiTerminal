# Quotes Provider Call Audit

Date: 2026-05-30

## Scope

This note records a focused audit of whether the Quotes provider adapters are calling and parsing upstream data sources correctly. It does not assess whether the upstream sources are exchange-authoritative.

Reviewed paths:

- `src-tauri/src/infrastructure/quotes/eastmoney/client.rs`
- `src-tauri/src/infrastructure/quotes/tencent/mod.rs`
- `src-tauri/src/infrastructure/quotes/sina/mod.rs`
- `src-tauri/src/infrastructure/quotes/tdx/adapter.rs`
- `src-tauri/src/infrastructure/quotes/tdx/manager.rs`

## Conclusion

The overall provider architecture is reasonable for a simulated trading and research terminal: TDX is the main SH/SZ source, HTTP providers are fallbacks, and the read path relies on local snapshots plus freshness checks.

However, the current provider calls cannot be treated as fully correct. Eastmoney has concrete call/parse issues. Tencent and Sina look mostly correct for basic fallback usage. TDX looks structurally aligned with pytdx/mootdx, but still needs live verification.

## Findings

### 1. Eastmoney BJ universe parser likely fails

`fetch_bj_universe()` models `data.diff` as `Vec<Row>`, but live Eastmoney `clist/get` response returned `diff` as an object keyed by numeric strings:

```json
"diff": {
  "0": { "f12": "810011", "f14": "优机定转" },
  "1": { "f12": "810013", "f14": "万通定转" }
}
```

Current code:

- `src-tauri/src/infrastructure/quotes/eastmoney/client.rs`
- `fetch_bj_universe()`
- `struct RespData { diff: Vec<Row> }`

Impact:

- BJ universe refresh may fail to parse or silently miss all BJ instruments depending on serde behavior.
- This undermines the spec expectation that Eastmoney supplements BJ instruments.

Suggested fix:

- Parse `diff` as either array or map, or use a custom deserializer tolerant of both shapes.
- Add a fixture test for the observed object-shaped response.

### 2. Eastmoney BJ quote `secid` mapping is not confirmed and appears unsafe

Current `secid_of()` maps BJ to `0.<code>`.

Observed live checks:

- `secid=0.430047` returned a payload with price fields as `0` and name containing an "already switched" marker.
- `secid=2.430047` returned no data.

Impact:

- BJ real-time quote fallback may produce missing or stale-looking data even when the instrument exists.
- If accepted without enough completeness checks, BJ snapshots could be misleading.

Suggested fix:

- Reconfirm Eastmoney's current BJ `secid` rules, especially for old `43/83/87` style codes versus new `92` style codes.
- Add live/fixture tests for representative BJ instruments.
- Do not mark BJ Eastmoney quote as usable unless `price`, `tradeDate`, and expected identity fields pass validation.

### 3. Eastmoney turnover rate scaling is likely wrong

Observed for `600519.SH`:

- Eastmoney `f168` returned `61`.
- Tencent displayed turnover as `0.61`.

Current code assigns:

```rust
turnover_rate: d.f168
```

Impact:

- `turnoverRate` may be 100x too high for Eastmoney quotes.
- Any scan/filter depending on turnover rate could rank or filter incorrectly.

Suggested fix:

- Confirm Eastmoney `f168` unit and normalize to shared `Percent`.
- If `f168 = percent * 100`, map as `d.f168.map(|v| v / 100.0)`.
- Add a provider fixture test.

### 4. Tencent basic quote mapping looks correct

Live response shape for `qt.gtimg.cn/q=sh600519` matched the current parser:

- `[3]` latest price
- `[4]` previous close
- `[5]` open
- `[6]` volume in lots
- `[9..28]` five-level book
- `[30]` exchange time
- `[33] / [34]` high / low
- `[37]` amount in ten-thousand CNY

Current normalization appears reasonable:

- volume `* 100`
- amount `* 10000`
- GBK decode

Residual risk:

- Needs fixture coverage for index/fund/BJ fallback behavior.

### 5. Sina basic quote mapping looks correct for last fallback

Live response shape for `hq.sinajs.cn/list=sh600519` matched the current parser:

- name, open, previous close, latest price, high, low
- volume already in shares
- amount in CNY
- date/time fields at the tail

The adapter also sets the required `Referer`.

This is acceptable as a last fallback for display. It should not be used for Account market-order simulation because it does not provide reliable depth in the current adapter.

### 6. TDX static structure looks reasonable, but live verification was not completed

The TDX implementation appears aligned with the intended pytdx/mootdx-compatible path:

- SH/SZ market mapping only; BJ skipped.
- batch quote requests.
- K-line pagination.
- failure causes reconnect/fallback.
- adapter normalizes to canonical `StockQuote` / K-line models.

Live test was not completed because `cargo` was not available in the current shell:

```text
zsh:1: command not found: cargo
```

Required follow-up:

- Run ignored live tests once Rust tooling is available.
- Cross-check a sample set against Tencent/Sina/Eastmoney for latest price, previous close, volume, amount, and five-level depth.

## Recommended Priority

1. Fix Eastmoney BJ universe parsing.
2. Reconfirm and fix Eastmoney BJ quote routing.
3. Fix Eastmoney turnover rate normalization.
4. Add provider fixture tests for Eastmoney/Tencent/Sina.
5. Run TDX live tests and record sample comparison results.

## Accuracy Statement

The current system is suitable as a research/simulation data pipeline only if consumers respect `freshness`, `source`, and item warnings. It should not be treated as exchange-authoritative market data. For simulated trading, Account should continue to fail closed on stale/missing quotes and missing depth.
