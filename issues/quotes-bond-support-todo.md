# TODO（部分完成）：Quotes 债券支持（可转债 / 国债 / 企业债）

> 关联 spec：[docs/design/quotes-module.md](../docs/design/quotes-module.md) §2 统一标的模型。
>
> **✅ 已完成（2026-06-02，commit 待填）：按需取债券的价格处理**。底层 provider 方法本就 category-无关，
> 现把 `map_security_quote` 的价格缩放改成 **decimal-driven**：`universe::is_bond(market, code)` 识别债券
> （**4 位小数**，实测可转债 TDX integer=真值×10000）→ `×0.01` 校正，不再 10× 错（实证与腾讯一致）。
> **universe 仍不收债券**。债券无复权（xdxr 空 → none）。
> 即「按代码取债券行情」开箱即用且报价正确。
>
> **⏳ 仍延后：债券一等公民**（下面「真要做时的清单」里除价格处理外的部分）。决策日 2026-06-01。

## 背景

当前 Quotes universe **只覆盖股票 / 指数 / 场内基金**三类。TDX `security_list` 原始全量含交易所
所有证券（实测 ~50,570：股 5208、指 557、基 1721，**其余 ~43,084 是国债/可转债/企业债/逆回购等**），
由 `infrastructure/quotes/universe.rs::classify` 白名单过滤——债券前缀返回 `None` 被丢弃。

**能力已存在于 infra 层但未暴露**：底层 TDX provider 方法（`TdxConnectionManager::fetch_quote /
fetch_quotes / fetch_daily_kline / fetch_kline_* / fetch_minute_*`）是 **category-无关**的——`TsCode`
只校验「6 位数字 + 市场后缀」，传入债券代码（如 `110059.SH` 可转债、`100303.SH` 国债）TDX 照样返回
数据。只是 universe 策展 + 对外读路径（list_market / scan / fetch_data 遍历 curated universe）不会
让债券冒出来。

## 为什么现在不做

- 债券不是研究 / 模拟交易的核心（股 / 基 / 指 + 资讯才是）。
- 直接放开会引入正确性坑（见下「必须处理的坑」），需要专门设计，不值得现在塞。

## 真要做时的清单（顺序）

1. **spec 先行**：在 `quotes-module.md` 把债券纳入 universe 范围 + canonical model 落点（`InstrumentCategory`
   增 `Bond`，可能再细分可转债 / 国债 / 企业债）。
2. **价格小数位**：债券（可转债）在 TDX quote 是 **4 位小数**编码（integer=真值×10000）。`map_security_quote`
   已处理：`is_bond → ×0.01`、`Fund → ×0.1`、股/指 ×1.0。否则报价 **10× / 100× 偏高**（与之前
   ETF `510300` 那个 bug 同类，commit ce25baf）。最稳妥是按 `category` 或真实 `decimal_point` 缩放。
3. **classify 放行**：`universe.rs::classify_sh` / `classify_sz` 把债券前缀映射到 `Bond`（SH 100/110/120/130、
   SZ 12xxxx 可转债等），不再 `None` 丢弃。注意：当前 SH 这些前缀是被**有意丢弃**的（见 commit 92cd9e6
   修了「债券误当指数」），放行时要分类成 Bond 而不是 Index。
4. **universe 纳入 + 读路径**：list_market / 扫描 / 详情是否展示债券、是否单独 tab。
5. **复权**：债券**无 xdxr/复权**概念，adjust 路径要对 Bond short-circuit（none 即终态）。
6. **盘口 / 成交**：可转债有盘口，可参与模拟交易吗？若要交易需 Account BC 配合（T+0、不同涨跌幅规则等）。
7. **测试**：golden / live 覆盖债券报价缩放、universe 计数、classify 分类。

## 现状锚点（实现位置）

- 过滤：`src-tauri/src/infrastructure/quotes/universe.rs::{classify_sh, classify_sz, classify}`
- 报价缩放：`src-tauri/src/infrastructure/quotes/tdx/manager.rs::map_security_quote`（`price_scale` by category）
- provider 方法（category-无关）：`src-tauri/src/infrastructure/quotes/tdx/manager.rs`
- universe 计数测试：`src-tauri/src/pipeline/quotes/live_integration_tests.rs::quotes_live_universe_classified_counts`
