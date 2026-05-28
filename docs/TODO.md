# TODO / Backlog

> 短期不做、但需要记录决策 + 上下文，避免日后忘记为什么没做。  
> 一旦决定开始某项，把对应条目移到 spec / issue 里跟踪，从这里删除。

---

## TDX 协议扩展：逐笔成交 (transaction)

**状态**：决定延后，等具体业务触发再补。

**功能**：TDX `get_transaction_data` (cmd `0x0fb1`) 拉当日逐笔成交记录。每条 = `{time, price, volume, direction(buy/sell)}`。

**已评估的实现成本**：~120-150 行（1 个新 cmd 文件 + manager 方法 + 测试）。详见 sub-agent gap 分析报告（2026-05-28）。

**为什么延后**：
- 当前 `domain/quotes/indicators.rs` 的技术指标（MA / MACD / KDJ / BOLL / RSI）只需 K 线，不依赖逐笔
- 主力净流入 / 大单分类 / 龙虎榜席位类功能产品方向未定
- 数据量大：单只活跃股一天 ~3 万笔；5000 标的 × 30000 笔 ≈ 1.5 亿条/日，存储 + 同步代价明显
- TDX 单次返回 ~2000 笔，全市场全量同步要 7.5 万+ 次调用

**触发补充的条件**（任一发生即可启动）：
- [ ] 产品方向确定要做"主力资金流向"指标
- [ ] Agent 工具需要"主动买卖盘比例"这类细粒度数据
- [ ] 龙虎榜分析功能纳入路线图（需要席位级数据）

---

## TDX 协议扩展：finance / F10 / block / 历史逐笔

**状态**：不做。

**理由**：
- `finance` / `F10` (公司信息、财务字段)：TuShare daily_basic / events 已覆盖
- `block` (板块信息)：sector 映射可用其他方式
- 历史逐笔 (`transactions(date)`)：回测场景才需要；当前不做回测引擎

如果未来路线图明确加入回测或离线分析，再单独评估。

---

## 交易日历假日表续更

**状态**：每年滚动维护，由人工 PR 提交。

**位置**：`src-tauri/src/domain/quotes/trade_calendar.rs::HOLIDAYS` + `COMPENSATORY_WORKDAYS`。

**维护节奏**：
- 国务院办公厅一般在每年 11-12 月发布次年法定节假日通知（例：[国办发明电〔2024〕14号](https://www.gov.cn/zhengce/zhengceku/) 即 2025 年安排）。
- 发布后 1 周内提 PR 续更对应年份的 `HOLIDAYS` 和 `COMPENSATORY_WORKDAYS`，并补 `is_trading_day(...)` 测试用例覆盖关键日（春节首/末、国庆、调休补班）。

**当前覆盖**：
- 2024 / 2025：已确认。
- 2026：基于国务院 2025-11 发布的初版通知；如有官方修订需同步更新。`COMPENSATORY_WORKDAYS` 2026 段暂缺（待官方通知发布后核对补齐）。
- 2027+：未发布，待官方通知后续补。

**TuShare 校准**：`TushareHealthState.isAvailable = true` 时由 pipeline 调 `trade_cal` 校准本地推算；差异点以 TuShare 为准并记录修正日志（见 spec quotes-module.md §5 "交易日历"）。

