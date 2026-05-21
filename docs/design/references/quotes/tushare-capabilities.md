# TuShare Pro 接口能力清单（本项目实测）

> **采集时间**：2026-05-14（重跑）
> **方法**：`infrastructure/quotes/tushare/probe.rs` 遍历 68 个接口，每个发一次轻量请求，记录返回行数
> **结果文件**：`app_data_dir/tushare-probe-result.json`
> **范围**：仅列出**实测可用**（status=ok，65 个）的接口；empty / quota / 接口名错的不收录

---

## 📈 股票（A 股）

| 接口 | 用途 | 单次返 |
|---|---|---|
| `stock_basic` | 全市场股票档案 | 5515 条 |
| `daily` | 个股日 K | 6000 行（单次上限） |
| `weekly` | 个股周 K | 1746 行 |
| `monthly` | 个股月 K | 416 行 |
| `adj_factor` | 复权因子 | 6000 行 |
| `daily_basic` | PE/PB/PS/换手率/量比/市值 | 5493 行（单日全市场） |
| `stk_factor` | 技术因子（MACD/KDJ/RSI 已算好） | 8348 行 |

## 📊 指数

| 接口 | 用途 | 单次返 |
|---|---|---|
| `index_basic` | 指数档案 | 594 条（SSE 单市场，三市场合计 5920） |
| `index_daily` | 指数日 K | 8000 行 |
| `index_weekly` | 指数周 K | 1000 行 |
| `index_monthly` | 指数月 K | 418 行 |
| `index_dailybasic` | 指数 PE/PB/股息率 | 12 行（单日主流指数） |

## 💰 基金（场内 ETF/LOF）

| 接口 | 用途 | 单次返 |
|---|---|---|
| `fund_basic` | 基金档案（场内 + 场外） | 2560 条 |
| `fund_daily` | 场内基金日 K | 3390 行 |
| `fund_nav` | 基金净值（单位/累计/复权） | 3688 行 |
| `fund_portfolio` | 基金持仓明细 | 8000 行（单次上限） |

## 📊 财务报表

| 接口 | 用途 | 单次返 |
|---|---|---|
| `income` | 利润表 | 127 行 |
| `balancesheet` | 资产负债表 | 100 行 |
| `cashflow` | 现金流量表 | 90 行 |
| `forecast` | 业绩预告 | 16 行 |
| `express` | 业绩快报 | 4 行 |
| `fina_indicator` | 财务指标（ROE/毛利率/EPS） | 100 行 |
| `fina_mainbz` | 主营业务构成 | 150 行 |
| `disclosure_date` | 财报披露日历 | 105 行 |

## 💸 资金面 / 龙虎榜

| 接口 | 用途 | 单次返 |
|---|---|---|
| `top_list` | 龙虎榜每日 | 127 行（单日全榜） |
| `top_inst` | 龙虎榜机构席位 | 1274 行 |
| `moneyflow` | 个股资金流（DDE） | 3897 行 |
| `moneyflow_hsgt` | 沪深港通资金 | 1 行（单日） |
| `hsgt_top10` | 北向 TOP10 | 20 行 |
| `ggt_top10` | 港股通 TOP10 | 20 行 |
| `margin` | 融资融券汇总 | 3 行（三大交易所） |
| `margin_detail` | 融资融券明细 | 3912 行 |
| `repurchase` | 股票回购 | 2000 行 |
| `block_trade` | 大宗交易 | 293 行 |
| `share_float` | 限售解禁 | 214 行 |
| `stk_holdernumber` | 股东户数 | 125 行 |
| `top10_holders` | 前十大股东 | 364 行 |
| `top10_floatholders` | 前十大流通股东 | 820 行 |

## 📝 公司动作 / 元数据

| 接口 | 用途 | 单次返 |
|---|---|---|
| `dividend` | 分红送股 | 51 行 |
| `suspend_d` | 停复牌 | 27 行 |
| `namechange` | 股票曾用名 | 8 行 |
| `stk_managers` | 公司高管 | 193 行 |
| `stk_rewards` | 高管薪酬 | 3667 行 |
| `new_share` | IPO 新股 | 2000 行 |
| `hs_const` | 沪深股通成份 | 581 行 |

## 📚 板块 / 行业

| 接口 | 用途 | 单次返 |
|---|---|---|
| `concept` | 同花顺概念分类 | 879 个概念 |
| `concept_detail` | 概念成份股 | 5 行（按 concept_id） |
| `index_classify` | 申万行业分类 L1 | 28 个一级行业 |

## 🌐 港股 / 美股

| 接口 | 用途 | 单次返 |
|---|---|---|
| `hk_basic` | 港股档案 | 2737 条 |
| `hk_daily` | 港股日 K | 2328 行 |
| `us_basic` | 美股档案 | 6000 条 |
| `us_daily` | 美股日 K | 8000 行 |

## 🪙 可转债 / 期货 / 期权

| 接口 | 用途 | 单次返 |
|---|---|---|
| `cb_basic` | 可转债档案 | 1125 条 |
| `cb_daily` | 可转债日 K | 338 行 |
| `fut_basic` | 期货合约（DCE 大商所） | 3222 条 |
| `fut_daily` | 期货日 K | 1074 行 |
| `opt_basic` | 期权合约 | 12000 条 |
| `opt_daily` | 期权日 K | 15000 行 |

## 🌏 宏观经济

| 接口 | 用途 | 单次返 |
|---|---|---|
| `shibor` | Shibor 利率 | 330 行 |
| `cn_gdp` | GDP | 4 行（季度） |
| `cn_cpi` | CPI | 12 行 |
| `cn_ppi` | PPI | 12 行 |
| `cn_m` | 货币供应（M0/M1/M2） | 12 行 |
| `sf_month` | 社会融资规模 | 12 行 |

## 🗓️ 交易日历

| 接口 | 用途 | 单次返 |
|---|---|---|
| `trade_cal` | 交易日历（SSE） | 492 行 |

---

## 当前项目接入状态

| 接口 | 实际在用 | 入口 |
|---|---|---|
| `stock_basic` | ✅ | `pipeline::stocks::refresh_now` → `infrastructure::quotes::tushare::stock::fetch_all_stocks` |
| `daily` / `weekly` / `monthly` | ✅ | `infrastructure::quotes::cache::kline_cache::ensure_klines` → `stock::fetch_klines_in_range` |
| `adj_factor` | ✅ | `stock::fetch_adj_factor`（kline qfq 复权计算） |
| `daily_basic` | ✅ | `stock::fetch_daily_basic` / `fetch_daily_basic_by_date` → `fetch_stock_profile` / `scanner` |
| `index_basic` | ✅ | `pipeline::stocks::refresh_indexes` → `index::fetch_indexes_by_market` |
| `index_daily` / `index_weekly` / `index_monthly` | ✅ | `kline_cache` 通过 category=index 路由 |
| `index_daily` (latest) | ✅ | `pipeline::market_overview::fetch_market_overview` → `index::fetch_index_latest` |
| `fund_basic` | ✅ | `pipeline::stocks::refresh_funds` → `fund::fetch_listed_funds` |
| `fund_daily` | ✅ | `kline_cache` 通过 category=fund 路由 |
| `concept` / `concept_detail` | ✅ | `infrastructure::quotes::tushare::concept` → quotes IPC |
| `namechange` / `suspend_d` / `dividend` / `forecast` / `share_float` | ✅ | `events.rs` → `fetch_company_events` |
| `trade_cal` | ✅ | `calendar.rs` 已实现（caller 待接通） |
| `top_list` / `moneyflow` / `moneyflow_hsgt` / `hsgt_top10` / `margin` | ✅ | `flow.rs` → quotes IPC |
| `stk_factor` | ❌ | 待接入 |
| `income` / `balancesheet` / `cashflow` / `express` / `fina_indicator` / `fina_mainbz` / `disclosure_date` | ❌ | 财务三大表 + 衍生指标待接入 |
| `fund_nav` / `fund_portfolio` | ❌ | 基金净值 / 持仓待接入 |
| `stk_managers` / `stk_rewards` / `new_share` / `hs_const` / `top10_holders` / `top10_floatholders` / `stk_holdernumber` / `block_trade` / `repurchase` | ❌ | 待接入 |
| `index_dailybasic` / `index_classify` | ❌ | 待接入 |
| `hk_basic` / `hk_daily` / `us_basic` / `us_daily` | ❌ | 港美股待接入 |
| `cb_basic` / `cb_daily` / `fut_basic` / `fut_daily` / `opt_basic` / `opt_daily` | ❌ | 衍生品待接入 |
| `shibor` / `cn_gdp` / `cn_cpi` / `cn_ppi` / `cn_m` / `sf_month` | ❌ | 宏观待接入 |

> **接入率**：65 接口中 13 个真接通 caller（20%）；其余 52 个**接口模块已写但 caller 未接**——等具体业务需求触发再接通，不浪费工程量。

---

## 重新跑探测

```bash
# 1. 删 app_state 里的标记（让 scheduler 重新跑）
sqlite3 "$HOME/Library/Application Support/com.local.gangzi-terminal/gangzi-terminal.sqlite3" \
  "delete from app_state where key='gangzi-terminal.tushare-probe-done'"

# 2. 重启 app
npm run tmux:restart

# 3. 等约 60 秒，结果落在
cat "$HOME/Library/Application Support/com.local.gangzi-terminal/tushare-probe-result.json"
```

或前端 invoke：

```ts
await invoke("probe_tushare_capabilities");
```
