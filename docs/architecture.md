# Architecture

> **本文档是权威设计基线**。所有模块、数据流、依赖方向都以这里为准。代码与此不符的，是代码该改。
>
> 历史版本见 git log；本次（2026-05-18）同步当前 DDD-lite 落地状态：`domain / infrastructure / pipeline / adapters` 四层、chat-only agent、typed Account/News、删除旧简报/复盘结果模型。

---

## 0. 一句话定位

> **Agent 自驱动的 A 股研究终端**——Agent 从市场数据 + 资讯里识别机会，在模拟账户里实盘验证，把过程沉淀成可审计、可复盘的判断链。用户是观察者，不是审批员。

不连真券商。所有交易都是模拟。

---

## 1. 核心哲学（不可妥协的 7 条）

### 1.1 Agent 自驱动（Notify Mode）

Agent 看到符合自己框架的机会 → **直接调 SimAccount 写工具下单** → 然后通过 chat event 通知用户"我做了什么、为什么"。**没有审批流**。

用户介入只有 3 种合法方式：
- chat 里 ask（"你刚才为什么"）
- chat 里 instruct（"X 板块避开" → 进 memory）
- chat 里 command（"帮我平掉 600519" → agent 调工具执行）

### 1.2 单一真源 + 派生 over 存储

每类数据有且只有一个权威 source。派生数据**绝不冗余存**——避免漂移整类 bug。

| 数据 | 真源 | 派生策略 |
|---|---|---|
| 行情（实时） | **TDX**（主，SH/SZ）+ EM（BJ + 故障兜底）+ 腾讯 / 新浪（极端兜底） | `MARKET_SNAPSHOT` in-memory，旁路盘中 15s / 盘外 60s |
| 行情（历史 K） | TuShare（日/周/月）+ EM（分钟 + 分时） | `kline_cache` SQLite TTL |
| 持仓事件 | `position_events`（append-only） | — |
| 持仓状态 | `simulated_positions`（快照） | 可从 events 重算 |
| 现金余额 | events walk 派生 | **不存** |
| PnL / 总资产 | 快照时算 | **不存** |
| 投资记忆 | `app_state[KEY_MEMORY]` | `InvestorMemory` 合并规则；不再另存派生画像 |

### 1.3 模块边界硬约束（依赖单向）

```
            ┌────────────────────┐
            │  Chat (观察 + 干预)  │
            └─────────┬──────────┘
                      │ subscribe events / dispatch user input
                      ↓
            ┌────────────────────┐
            │      Agent         │
            │   (decision loop)  │
            └──┬─────┬─────┬─────┘
        read  │ act │      │ read
       snapshot│     │      │ snapshot
              ↓     ↓      ↓
        ┌────────┐ ┌────────────┐ ┌──────────┐
        │ Quotes │ │ SimAccount │ │   News   │
        │snapshot│ │  snapshot  │ │ snapshot │
        └────┬───┘ └─────┬──────┘ └──────────┘
             ↑           │
             └───────────┘ (account reads quotes snapshot for valuation)
```

当前代码分层（**Rust use 必须遵守**）：

```
adapters/       Tauri commands + LLM tools（唯一外部协议边界）
   ↓
pipeline/       application use cases：chat / account / refresh / scheduler
   ↓
infrastructure/ SQLite / HTTP / provider / cache / snapshot
   ↓
domain/         entities / value objects / rules（纯 Rust，无 I/O）
```

**层间依赖矩阵**（行 = 调用方，列 = 被调方；✅ 允许，❌ 禁止）：

| ＼ 被调 | domain | infrastructure | pipeline | adapters | tauri/db/http |
|---|:-:|:-:|:-:|:-:|:-:|
| **domain** | ✅ | ❌ | ❌ | ❌ | ❌（必须无 I/O） |
| **infrastructure** | ✅ | ✅ | ❌ | ❌ | ✅ |
| **pipeline** | ✅ | ✅ | ✅ | ❌ | ✅ |
| **adapters** | ✅ | ✅ | ✅ | ✅ | ✅ |

**BC 间依赖矩阵**（行 = 调用方 BC，列 = 被调方 BC）：

| ＼ 被调 | quotes | account | news | agent |
|---|:-:|:-:|:-:|:-:|
| **quotes** | ✅ | ❌ | ❌ | ❌ |
| **account** | ✅（只为 valuation 读 quotes snapshot） | ✅ | ❌ | ❌ |
| **news** | ❌ | ❌ | ✅ | ❌ |
| **agent** | ✅（**只通过** `adapters/agent_tools` 反腐译码） | ✅（同上） | ✅（同上） | ✅ |

**关键约束**：
- `domain/` 不允许 use `tauri | rusqlite | reqwest | infrastructure | pipeline | adapters`
- `infrastructure/` 可实现 DB / HTTP / cache / provider，但不允许 use `pipeline | adapters`
- `pipeline/` 负责编排 use case；不暴露 `#[tauri::command]`；不允许 use `adapters`
- `adapters/` 是唯一 IPC / LLM tool 协议层；可以 use 所有下层；做 DTO 转换 + 协议反腐
- **Quotes / SimAccount / News 任何一层都不允许 import agent 代码**（核心边界）
- Agent 调三个执行模块走 `adapters/agent_tools/*.rs`（实现 `pipeline::agent::tools::Tool`），由 `adapters/chat_commands` 构造 `ToolRegistry` 注入 pipeline——Tool trait/Registry 抽象在 pipeline，具体 tool 在 adapters，pipeline 不反向依赖 adapters

### 1.4 持久化是命脉

任何 mutation **先写 event 再生效**。三层含义：
- **可审计**：每个开平仓、每个 memory 更新、每个 agent/tool 决策都有时间戳 + 来源标签
- **可恢复**：进程重启从 SQLite 重建全部状态
- **可学习**：事件链是 agent 学习的原料——胜率、错误模式、风格演化都基于历史

### 1.5 Snapshot-First 数据访问（**新增核心约定**）

Agent / Chat / UI **永远不直接 fetch**。所有数据读取都走**内存 snapshot**——同步、毫秒级、不等 I/O。Snapshot 由对应模块**自己用后台任务近实时维护**。

```
传统模式：
  Agent decide → fetch quotes (3s I/O) → fetch account (1s I/O) → LLM → ...
  ↑ 每次都串行等

Snapshot-first：
  Background: Quotes refresh loop 持续刷新 snapshot
              Account snapshot 在 events 变化 + Quotes 更新时重算
  Foreground: Agent decide → read snapshot (< 1ms) → LLM → act → ...
              ↑ 同步读，agent 不感知数据从哪来
```

**好处**：
- **解耦**：Agent 不知道数据怎么来、什么时候刷新；snapshot 由 owner 模块自治
- **性能**：Agent 决策不等网络；UI 渲染立刻拿数据
- **一致性**：同一时刻的 snapshot 是全局一致视图（避免"agent 看到的 quote 和 account 算 PnL 用的 quote 不同时刻"）
- **可测**：mock snapshot 就能测 agent 行为，不用起 Tauri / HTTP

**实现要点**：
- 每个 source 模块（Quotes / Account / News）暴露**两套 API**：
  - `refresh_*` / `update_*`：mutation 入口，**只在模块内部 + 调度器调**
  - `get_*_snapshot` / `read_*`：**任何人**都能同步读
- Snapshot 是内存里的 `RwLock<...>` 或 `OnceLock<DashMap<...>>`——并发读零开销
- 启动时从 SQLite + 配置 hydrate；运行时维护

### 1.6 不背历史包袱（重构纪律）

> ⏳ **生效期：快速迭代期（current）**
>
> 这条规则**只在产品未上线、无外部用户、无数据依赖**的阶段适用。一旦满足以下任一条件就**撤销本条**、改成标准的 deprecation / migration policy：
> - 产品对外发布、有用户在用
> - 出现需要数据迁移而无法重置的真实持仓 / 用户记忆
> - 引入了第三方集成（外部 API 消费者）
>
> 触发条件时，本条由作者（项目所有者）显式宣布撤销并改写。

**当前阶段**：个人项目，无外部 API 消费者，无升级窗口约束。当设计演化时，**直接改历史代码、迁数据、删旧路径**——不维护兼容层、不留 deprecated wrapper。

**具体含义**：
- **签名变更直接传播**：API 改了就改所有调用点；不做 v1/v2 并存
- **schema 变了直接迁**：一次性 migration；不保留"老格式 + 新格式"双兼容读
- **概念重命名直接全文替换**：不留 `type alias` / `#[deprecated]` 桩
- **重构边界一刀切**：旧版选择和新版不兼容时，**删了重写**比"渐进 deprecate"快、清晰
- **fix forward 优先于 revert**：发现重构错了，继续改正而不是 revert——commit 历史是连续的演化记录

**明确不做的反模式**：
- ❌ `#[deprecated]` + 保留旧函数 / 旧字段
- ❌ 双数据格式分支（`if old_schema_v1 { ... } else { ... }`）
- ❌ "兼容老 KV key" 的 fallback 读取
- ❌ rename 后保留 `pub use OldName = NewName`
- ❌ 删除的代码留 `// kept for backward compat` 注释

**对应的工程纪律**：
- 重构 PR 和功能 PR 分开（PR description 必须说明是哪类）
- 每次 schema migration 在 `docs/design/migrations/` 留笔（用过即弃，不是兼容层文档）
- 全部 cargo test 过才提交——没有"保留兼容旧测试"的逃生路径

**为什么这么严**：上轮 cash 漂移 bug 的根因之一就是"老代码写 `KEY_CASH_BALANCE`，新代码也读这个 key"——双源并存让真源不明。删干净 = 没有歧义。

**边界澄清**：这条**不是说**"为了删而删"或者"不允许有迁移期"。它说的是：
- ✅ migration 期可以有（PR 内部多次 commit），但 **PR merge 前删干净**
- ✅ 数据库一次性 ALTER TABLE / 重建表，不维护两份 schema
- ❌ 不留"半年后再删"的代码桩

### 1.7 DDD-driven 开发（**所有新功能必须遵守**）

> 自 2026-05-18 起，**所有新代码必须按 BC + 层级定位**。 跳层 / 反向依赖 / 跨 BC 错位的 PR 不接受。

**新功能开发的标准 7 步**（按顺序，每步通过再下一步）：

```
Step 1. 选定 BC（quotes / account / news / agent）
        判据：业务概念归属。"涨跌幅扫描" → quotes；"加仓规则" → account；
              "资讯抽取" → news；"工具调用 / 记忆" → agent。
        若一个功能跨多个 BC（如"按持仓自动订阅行情"），写在 pipeline 层用
        多个 BC 的公开 API 编排——而不是在 domain 里互相 import。

Step 2. 写 domain 层（如果是新概念）
        - 纯 Rust：无 tauri、无 rusqlite、无 reqwest、无 chrono 网络时间
        - newtype ID + value object 强类型（见 § 9.2 / 9.3）
        - 规则用纯函数 / 聚合根方法
        - 写单元测试（无 mock，纯函数测）

Step 3. 写 infrastructure 层（如果有 I/O）
        - DB 表 / migration / repository
        - HTTP client / provider
        - snapshot 内存表 + RwLock / OnceLock
        - 实现 trait 把 domain 类型投射到 row / wire format

Step 4. 写 pipeline 层（编排 use case）
        - 调用 infrastructure 读写数据
        - 调用 domain 算规则 / 派生
        - 暴露 use case 函数；**不暴露 #[tauri::command]**
        - 跨 BC 编排只在这一层（如 account valuation 读 quotes snapshot）

Step 5. 写 adapters 层（如果对外暴露）
        - Tauri command：薄包装，调 pipeline 函数；做 DTO 转换
        - LLM 工具：实现 pipeline::agent::tools::Tool；
          在 build_chat_registry 里注册

Step 6. 依赖方向自检（PR 前必跑）
        grep -rE "use crate::adapters"      src-tauri/src/{pipeline,domain,infrastructure}
        grep -rE "use crate::pipeline"      src-tauri/src/{domain,infrastructure}
        grep -rE "use crate::infrastructure" src-tauri/src/domain
        grep -rE "use crate::(adapters::agent|pipeline::agent|domain::agent|infrastructure::agent)" \
          src-tauri/src/{domain,infrastructure,pipeline}/{quotes,account,news}
        任一非空 = bug。

Step 7. 门禁：cargo check && cargo test && npm run build 全绿
```

**常见错放反例**：

| 错放位置 | 正确位置 | 为什么 |
|---|---|---|
| `domain/quotes/fetch_klines.rs` 调 reqwest | `infrastructure/quotes/tushare/klines.rs` | domain 不允许 I/O |
| `pipeline/account/positions.rs` 直接写 SQL | `infrastructure/account/repository.rs` 出函数，pipeline 调它 | pipeline 只编排，不接 SQL |
| `infrastructure/account/repository.rs` 调 `pipeline::market::overview` | 把需要的逻辑放进 domain，infra 调 domain | infra 不依赖 pipeline |
| `adapters/account_commands.rs` 里写涨跌停校验 | `domain/account/rules.rs` 加规则 + pipeline 调；adapter 只做 DTO | 业务规则不进 adapter |
| `domain/quotes/types.rs` import `crate::domain::agent::*` | 解耦：把 agent 消费 quotes 的 DTO 放到 `domain/agent` 或 `adapters/agent_tools` | quotes BC 不感知 agent |
| `pipeline/chat.rs` 直接调 `adapters::agent_tools::*` | 把 Tool 抽象放 pipeline，concrete impl 在 adapter，adapter 注入 registry | pipeline → adapters 反向 |

**BC 边界澄清**：
- **quotes ↔ account 唯一允许的方向**：account 在 `infrastructure/account/valuation.rs` 里读 `MARKET_SNAPSHOT`（quotes 提供的同步快照）算 PnL。account 不调 `pipeline::market::*`，不调 `infrastructure::quotes::tushare::*`——只接 snapshot 出口。
- **agent ↔ 其它三个 BC**：agent 永远从 `adapters/agent_tools/<bc>.rs` 经反腐译码层调三个执行模块的公开 API。三个执行模块不能反向 import 任何 agent 代码（包括 domain/agent）。
- **跨 BC 共享类型**：放 `domain/shared/`（StockCode / Money / TradeDate）。不要为了共用一个 helper 而让 BC A 的 domain import BC B 的 domain。

---

## 2. 4 个模块的责任 + Public API

> 本节是接口契约。所有 API 标了"读" / "写" / "维护"三类。
> - **读**：同步，从 snapshot 取，不触发 I/O
> - **写**：mutation，可能 async（写盘 + 重建 snapshot）
> - **维护**：scheduler / refresh loop 才调，外部不调

### 2.1 Quotes — 市场数据 source of truth

**责任**：A 股 + 基金 + 大盘的只读数据获取 + 内存 snapshot 维护。**定位是"研究员 + 短线 trader 综合工具"**——盘口、分钟 K、基本面、资金面、公司动作都覆盖。

```rust
// ========== 实时（snapshot，同步读）==========
pub fn get_quote(code: &StockCode) -> Option<StockQuote>;            // 含五档盘口 + 内外盘
pub fn get_quotes_batch(codes: &[StockCode]) -> HashMap<StockCode, StockQuote>;
pub fn get_market_overview() -> Option<MarketOverview>;
pub fn get_indicators(code: &StockCode) -> Option<IndicatorSnapshot>;
pub fn get_fundamentals(code: &StockCode) -> Option<DailyBasic>;     // PE/PB/换手/市值
pub fn get_stock_profile(code: &StockCode) -> Option<StockProfile>;  // 个股全档案（含基本面）

// ========== 历史（snapshot，同步读） ==========
pub fn get_klines(code: &StockCode, period: KlinePeriod) -> Option<KlineSeries>;
pub fn get_index_klines(index_code: &StockCode, period: KlinePeriod) -> Option<KlineSeries>;
pub fn get_minute_klines(code: &StockCode, period: MinutePeriod) -> Option<MinuteKlineSeries>;

// ========== 资金面 / 公司动作（async fetch） ==========
pub async fn fetch_top_list(app: &AppHandle, trade_date: Option<TradeDate>) -> Result<Vec<TopListItem>, QuotesError>;
pub async fn fetch_moneyflow(app: &AppHandle, code: &StockCode, days: usize) -> Result<Vec<MoneyFlowItem>, QuotesError>;
pub async fn fetch_north_flow(app: &AppHandle, days: usize) -> Result<Vec<NorthMoneyFlow>, QuotesError>;
pub async fn fetch_north_top10(app: &AppHandle, trade_date: TradeDate) -> Result<Vec<NorthHolding>, QuotesError>;
pub async fn fetch_margin_summary(app: &AppHandle, days: usize) -> Result<Vec<MarginSummary>, QuotesError>;
pub async fn fetch_company_events(app: &AppHandle, code: &StockCode, days_ahead: i32) -> Result<Vec<CompanyEvent>, QuotesError>;

// ========== 板块 ==========
pub async fn fetch_concept_list(app: &AppHandle) -> Result<Vec<ConceptSector>, QuotesError>;
pub async fn fetch_concept_performance(app: &AppHandle, trade_date: Option<TradeDate>) -> Result<Vec<ConceptPerformance>, QuotesError>;
pub async fn fetch_concept_members(app: &AppHandle, concept_code: &str) -> Result<Vec<StockCode>, QuotesError>;

// ========== 全市场扫描 ==========
pub async fn scan_market(app: &AppHandle, filter: ScanFilter, limit: usize) -> Result<ScanResult, QuotesError>;
pub async fn scan_market_query(app: &AppHandle, conditions: Vec<ScanCondition>, sort_by: ScanSort, limit: usize) -> Result<ScanResult, QuotesError>;

// ========== 基金 ==========
pub async fn fetch_fund_info(app: &AppHandle, code: &str) -> Result<FundInfo, QuotesError>;
pub async fn fetch_fund_nav(app: &AppHandle, code: &str, limit: usize) -> Result<FundNavSeries, QuotesError>;
pub async fn fetch_fund_holdings(app: &AppHandle, code: &str) -> Result<FundPortfolio, QuotesError>;
pub async fn fetch_fund_managers(app: &AppHandle, code: &str) -> Result<FundManagerList, QuotesError>;

// ========== 交易日历 ==========
pub fn is_trading_day(date: TradeDate) -> bool;
pub fn next_trading_day(date: TradeDate) -> TradeDate;
pub fn previous_trading_day(date: TradeDate) -> TradeDate;
pub fn current_trade_date() -> TradeDate;                            // 北京时间今日
pub async fn refresh_trade_calendar(app: &AppHandle, year: i32) -> Result<usize, QuotesError>;

// ========== Mutation（refresh loop / ensure 用）==========
pub async fn refresh_quotes(app: &AppHandle, codes: &[StockCode]) -> Result<(), QuotesError>;
pub async fn refresh_market_overview(app: &AppHandle) -> Result<(), QuotesError>;
pub async fn refresh_klines(app: &AppHandle, code: &StockCode, period: KlinePeriod, limit: usize) -> Result<(), QuotesError>;
pub async fn refresh_minute_klines(app: &AppHandle, code: &StockCode, period: MinutePeriod) -> Result<(), QuotesError>;
pub async fn refresh_fundamentals(app: &AppHandle, codes: &[StockCode]) -> Result<(), QuotesError>;
pub async fn refresh_stocks_universe(app: &AppHandle) -> Result<usize, QuotesError>;
pub async fn ensure_quote(app: &AppHandle, code: &StockCode) -> Result<StockQuote, QuotesError>;
pub async fn ensure_klines(app: &AppHandle, code: &StockCode, period: KlinePeriod) -> Result<KlineSeries, QuotesError>;

// ========== 同步只读 helpers ==========
pub fn resolve_stock(code_or_name: &str) -> Option<StockRef>;
pub fn compute_indicators(klines: &[KlinePoint], cfg: IndicatorConfig) -> IndicatorSnapshot;
```

**Snapshot 维护策略**：

| 数据 | 谁维护 | 频率 |
|---|---|---|
| 单股报价 + 盘口 snapshot（Account subscriptions + 核心指数） | refresh loop | 盘中 15s / 盘外 60s / 周末 10min（TDX 主路径） |
| 单股报价 snapshot（其它 code） | `ensure_quote` 调用 | 按需 |
| 基本面 snapshot（Account subscriptions + 核心指数） | refresh loop | 每日盘后（21:00） |
| 大盘指数 snapshot | refresh loop | 盘中 30s / 盘后日级 |
| 日 K 线 snapshot | 启动 hydrate + refresh loop 增量 | 每日盘后 |
| 分钟 K 线 snapshot | UI 选中 / warm 任务按需维护 | 盘中 5min / 盘外冻结 |
| stocks universe | scheduler | 每日 08:30 |
| 交易日历 | scheduler | 每年 1 月 + 启动 hydrate |
| 资金面 / 公司动作 / 板块 | **不进 snapshot**——按需 async fetch | — |

**专业数据完整性补充**（区别于纯研究员工具）：

| 维度 | 给到的数据 |
|---|---|
| **盘口** | 五档 bid/ask 价 + 量、委比、委差、内外盘（StockQuote 扩展字段） |
| **多周期** | 日/周/月 K + 1m/5m/15m/30m/60m 分钟 K |
| **基本面** | PE/PE_TTM/PB/PS/股息率/换手率/量比/总市值/流通市值（DailyBasic） |
| **资金面** | 主力资金流（已有）+ 北向资金 + 沪深港通 top10 + 融资融券余额 |
| **公司动作** | 分红 / 停牌 / ST 状态变更 / 业绩预告 / 解禁（CompanyEvent enum） |
| **板块** | 概念列表 + 板块涨跌榜 + 板块成分 |
| **交易日历** | trade_cal 接口同步，支持节假日 / 长假后正确倒推 |
| **复合查询** | `scan_market_query` 支持多条件 + 排序（"RSI<30 且 5 日跌>8% 且额>1亿"） |

**Quotes 不做**：
- 写 `position_events` / `simulated_positions`
- 管理自选股 CRUD（watchlist 属于 SimAccount；Quotes 只消费 Account subscriptions）
- LLM 调用
- 推 chat 消息

### 2.2 SimAccount — 模拟账户 source of truth

**责任**：模拟交易 + A 股规则校验 + 持仓事件链 + 自选股管理 + 账户 snapshot 维护。

```rust
// === 读（同步，从 snapshot 取）===
pub fn get_snapshot() -> AccountSnapshot;
pub fn get_position(id: &PositionId) -> Option<Position>;
pub fn list_open_positions() -> Vec<Position>;
pub fn list_closed_positions(limit: usize) -> Vec<Position>;
pub fn list_events(id: &PositionId) -> Vec<PositionEvent>;
pub fn cash_available() -> Money;
pub fn list_watchlist() -> Vec<StockCode>;

// === 写（mutation——agent 工具调用入口）===
pub async fn open_position(app: &AppHandle, req: OpenRequest, source: SourceTag) -> Result<Position, AccountError>;
pub async fn close_position(app: &AppHandle, id: &PositionId, reason: CloseReason, source: SourceTag) -> Result<Position, AccountError>;
pub async fn scale_position(app: &AppHandle, id: &PositionId, shares_delta: Shares, thesis: String, source: SourceTag) -> Result<Position, AccountError>;
pub async fn adjust_stops(app: &AppHandle, id: &PositionId, sl: Option<Money>, tp: Option<Money>, ts: Option<DateTime>, source: SourceTag) -> Result<Position, AccountError>;
pub async fn add_watchlist(app: &AppHandle, code: StockCode) -> Result<(), AccountError>;
pub async fn remove_watchlist(app: &AppHandle, code: &StockCode) -> Result<(), AccountError>;
pub fn reset_account(app: &AppHandle) -> Result<usize, AccountError>;

// === 维护（scheduler 调）===
/// 自动止损止盈：读 Quotes snapshot → 检查 open positions 触发条件 → 自动平仓
/// 由 SimAccount 自治；scheduler 周期性调度。
pub async fn scan_and_trigger_stops(app: &AppHandle) -> Result<Vec<Position>, AccountError>;

/// Account snapshot 重算——在 events 变化 / Quotes snapshot 更新时调
pub fn rebuild_snapshot(app: &AppHandle) -> Result<(), AccountError>;
```

**Snapshot 维护策略**：

| 触发 | 重算什么 |
|---|---|
| 任何写工具（open/close/scale/adjust）成功后 | 全 snapshot（cash + positions + 估值） |
| Quotes snapshot 更新事件 | 只重算 market_value + total_pnl（cash 不变） |
| 启动时 | 从 events 派生 cash + 读 positions + 拉 Quotes 估值 |
| `scan_and_trigger_stops` 触发平仓 | 同上"写工具" |

**SimAccount 不做**：
- LLM 调用
- 资讯查询
- 行情源获取 / 报价 provider 维护（只读 Quotes snapshot 做估值）
- 推 chat 消息（只 emit `positions-changed` event）

### 2.3 News — 资讯流

**责任**：feed 拉取 + 入库 + 状态机 + 搜索 + 文章抽取。

```rust
// === 读 ===
pub fn list_pending(limit: usize) -> Vec<NewsItem>;
pub fn list_recent(limit: usize, status: Option<NewsStatus>) -> Vec<NewsItem>;
pub fn count_pending() -> i64;
pub fn search(query: &str, limit: usize) -> Vec<NewsItem>;
pub async fn get_article(app: &AppHandle, news_id: &NewsId) -> Result<Option<ArticleContent>, NewsError>;

// === 写（mutation——future agent workflow / pipeline 调）===
pub fn claim_pending(app: &AppHandle, ids: &[NewsId]) -> Result<usize, NewsError>;   // pending → processing
pub fn mark_consumed(app: &AppHandle, ids: &[NewsId]) -> Result<usize, NewsError>;   // processing → consumed
pub fn revert_claim(app: &AppHandle, ids: &[NewsId]) -> Result<usize, NewsError>;    // processing → pending

// === 维护（scheduler 调）===
pub async fn refresh_feeds(app: &AppHandle) -> Result<RefreshResult, NewsError>;
pub fn recover_stale_processing(app: &AppHandle) -> Result<usize, NewsError>;
```

**News 不做**：
- NLP / 情感分析（agent 的事）
- 关联股票自动识别（见 § 10 Q3）
- LLM 调用

### 2.4 Agent — 决策引擎

**责任**：当前只有 chat pipeline 会触发 LLM run；通过 tool registry 调 Quotes / SimAccount / News；产出答复、交易动作、memory 更新；事件流通知 Chat UI。

**Pipeline 现状**：

| Pipeline | 触发 | Source 输入（优先读 snapshot） | 动作 |
|---|---|---|---|
| `chat` | user message | Quotes + Account snapshot + News + 对话历史 + InvestorMemory | 答复 + 可能的开/平/调仓 + memory 更新 |
| `refresh` | scheduler | 由 Quotes / Account / News 各自调度，不在 Agent 层 | 维护 snapshot / cache / DB |

`briefing` / `review` 已下线，旧简报/复盘结果模型已删除。未来若恢复定时简报或复盘，必须先补本 spec，再以新的 pipeline + typed domain model 接入，不复活旧结果模型。

**Agent 工具集**：

| 类别 | 工具 | snapshot vs fetch |
|---|---|---|
| 行情快照（同步） | `get_quote` / `get_market_overview` / `get_indicators` / `get_kline` | snapshot（必要时由明确工具触发 ensure/fetch） |
| 研究查询（async） | `scan_market` / `get_top_list` / `get_moneyflow` / `get_fund_*` | 一次性 fetch（不进 snapshot） |
| 账户读 | `get_account` / `get_position` | snapshot |
| 账户写 | `open_position` / `close_position` / `scale_position` / `adjust_stops` | mutation |
| 资讯 | `search_news` | typed repository 读 DB |
| 记忆 | `update_memory` / `remove_memory` | mutation |

### 2.5 Chat — 观察 + 干预层

**责任**：渲染 agent 决策事件 + 流式 LLM 文本；接受用户输入并 dispatch 给 agent。

```rust
// Tauri commands
pub async fn send_chat_message_now(app, content, images) -> Result<...>;

// 前端订阅的 events
"chat-message-appended" / "agent-event" / "positions-changed" / "quotes-snapshot-updated"
```

Chat **不**操作 SimAccount、Quotes、News——只触发 agent run。

---

## 3. 数据模型

### 3.1 实体清单 + 持久化映射

| 实体 | SQLite 表 | 内存 snapshot | 谁写 | 谁读 |
|---|---|---|---|---|
| `StockRow` | `stocks` | `STOCKS_REF` HashMap | scheduler 调 quotes::refresh_stocks_universe | `resolve_stock` |
| `KlineRow` | `klines` + `kline_meta` | `KLINE_SNAPSHOT` | Quotes refresh loop | `get_klines` |
| `StockQuote` | — | `MARKET_SNAPSHOT` HashMap | Quotes refresh loop | `get_quote` / `get_quotes_batch` / scanner / Account valuation |
| `NewsItem` | `news_items` | — | News refresh | UI / Agent `search_news` |
| `ArticleContent` | `article_contents` | — | News article extractor | `get_article` |
| `SimulatedPosition` | `simulated_positions` | `ACCOUNT_SNAPSHOT.positions` | SimAccount 写工具 | snapshot reads |
| `PositionEvent` | `position_events` | — | SimAccount 写工具 | `list_events` |
| `ChatMessage` | `chat_messages` | — | chat pipeline | Chat UI |
| `InvestorMemory` | `app_state[KEY_MEMORY]` | — | memory tools + chat pipeline | Agent prompt |
| `AgentRun` | `agent_runs` | — | observer | 调试 |
| `KvState` | `app_state` | — | Settings + scheduler | 全员 |

### 3.2 关键不变量

**派生数据绝不冗余存储**：
- `cash_available` = `INITIAL` + `Σ position_events.cash_delta`
- `total_assets` = `cash_available` + `Σ open_positions.market_value`（market_value 现算）
- `total_pnl` = `total_assets - INITIAL`
**Event 是真源**：仓位状态可以从 events walk；`simulated_positions` 只是性能快照，不一致时以 events 为准重算。

**SourceTag 必填**（强类型 enum）：每条 PositionEvent 都标 sourceKind + sourceRef。

### 3.3 核心数据结构（详）

> 数据类型按"对外契约"列出。Newtype IDs / Value Objects 见 § 9。

```rust
// ========== Quotes 模块 ==========

// === 实时报价（含盘口）===
pub struct StockQuote {
    pub code: StockCode,
    pub name: String,
    pub price: Option<Yuan>,
    pub change_percent: Option<f64>,     // 百分数
    pub change: Option<Yuan>,
    pub day_volume: Option<Lots>,         // 当日成交量
    pub day_amount: Option<Yuan>,         // 当日成交额
    pub high: Option<Yuan>,
    pub low: Option<Yuan>,
    pub open: Option<Yuan>,
    pub previous_close: Option<Yuan>,
    /// 五档盘口（来自 EM ulist.np 扩展字段）
    pub bid_prices: Option<[Yuan; 5]>,    // 买一到买五
    pub bid_volumes: Option<[Lots; 5]>,
    pub ask_prices: Option<[Yuan; 5]>,    // 卖一到卖五
    pub ask_volumes: Option<[Lots; 5]>,
    pub bid_total: Option<Lots>,
    pub ask_total: Option<Lots>,
    pub inside_volume: Option<Lots>,      // 内盘（主动卖）
    pub outside_volume: Option<Lots>,     // 外盘（主动买）
    /// 交易所给的报价时间戳
    pub quote_time: i64,                  // unix ms
    /// 本地拉取时间——agent 判断"snapshot 多旧"用
    pub captured_at: i64,                 // unix ms
}

// === K 线 ===
pub struct KlinePoint {
    pub date: TradeDate,
    pub open: Yuan, pub close: Yuan, pub high: Yuan, pub low: Yuan,
    pub volume: Lots,
    pub amount: Yuan,
}
pub struct KlineSeries {
    pub code: StockCode,
    pub period: KlinePeriod, pub adj: AdjMode,
    pub points: Vec<KlinePoint>,
    pub source: HistorySource,
    pub stale: bool,
    pub warning: Option<String>,
}
pub enum KlinePeriod { Day, Week, Month }
pub enum AdjMode { None, Qfq, Hfq }       // 前复权 / 后复权

// === 分钟 K（新增）===
pub enum MinutePeriod { M1, M5, M15, M30, M60 }
pub struct MinuteKlinePoint {
    pub timestamp: i64,                   // unix ms（分钟边界）
    pub open: Yuan, pub close: Yuan, pub high: Yuan, pub low: Yuan,
    pub volume: Lots, pub amount: Yuan,
    pub vwap: Option<Yuan>,               // 成交量加权均价
}
pub struct MinuteKlineSeries {
    pub code: StockCode, pub period: MinutePeriod,
    pub points: Vec<MinuteKlinePoint>,
    pub date: TradeDate,                  // 仅当日 / 跨多日
    pub stale: bool,
}

// === 基本面（新增）===
pub struct DailyBasic {
    pub code: StockCode, pub trade_date: TradeDate,
    pub pe: Option<f64>, pub pe_ttm: Option<f64>,
    pub pb: Option<f64>, pub ps: Option<f64>, pub ps_ttm: Option<f64>,
    pub dv_ratio: Option<f64>, pub dv_ttm: Option<f64>,
    pub turnover_rate: f64,               // %
    pub turnover_rate_float: Option<f64>, // 流通股换手率
    pub volume_ratio: f64,
    pub total_share: Lots,                // 总股本（手）
    pub float_share: Lots,
    pub free_share: Option<Lots>,
    pub total_mv: Yuan,                   // 总市值
    pub circ_mv: Yuan,                    // 流通市值
}

// === 个股全档案（新增）===
pub struct StockProfile {
    pub stock_ref: StockRef,
    pub fundamentals: Option<DailyBasic>,
    pub list_date: Option<TradeDate>,
    pub list_status: ListStatus,
    pub is_st: bool,
    pub indicators: Option<IndicatorSnapshot>,
}
pub enum ListStatus { Listed, Suspended, Delisted }

// === 技术指标（可配参数）===
pub struct IndicatorConfig {
    pub ma_periods: Vec<u32>,             // 默认 [5,10,20,60,120]
    pub ema_periods: Vec<u32>,            // 默认 [12,26]
    pub rsi_periods: Vec<u32>,            // 默认 [6,14,24]
    pub macd: (u32, u32, u32),            // 默认 (12,26,9)
    pub kdj: (u32, u32, u32),             // 默认 (9,3,3)
    pub boll: (u32, f64),                 // 默认 (20, 2.0)
    pub atr_period: u32,                  // 默认 14
    pub cci_period: u32,                  // 默认 14
}
impl Default for IndicatorConfig { /* 经典参数 */ }

pub struct IndicatorSnapshot {
    pub close: Yuan, pub as_of: TradeDate,
    pub ma: BTreeMap<u32, f64>,
    pub ema: BTreeMap<u32, f64>,
    pub macd: (f64, f64, f64),
    pub rsi: BTreeMap<u32, f64>,
    pub kdj: (f64, f64, f64),
    pub cci: f64,
    pub boll: (f64, f64, f64),
    pub atr: f64,
    pub obv: f64,
    pub volume_ratio: f64,
    pub vwap_day: Option<Yuan>,           // 当日 VWAP
}

// === 全市场扫描 ===
pub enum ScanFilter { LimitUp, LimitDown, TopGain, TopLoss, TopAmount, TopVolume }
pub struct ScanCondition { pub field: ScanField, pub op: ScanOp, pub value: f64 }
pub enum ScanField {
    ChangePct, Amount, Volume, TurnoverRate, VolumeRatio,
    Pe, Pb, Ps, DvRatio, TotalMv, CircMv,
    Rsi(u32), MaCrossUp(u32, u32),        // MA(short) 上穿 MA(long)
}
pub enum ScanOp { Gt(f64), Lt(f64), Eq(f64), Between(f64, f64) }
pub enum ScanSort { ChangePctDesc, ChangePctAsc, AmountDesc, VolumeDesc, TurnoverRateDesc }
pub struct ScanResult {
    pub items: Vec<ScanItem>,
    pub trade_date: TradeDate,
    pub captured_at: i64,
    pub from_cache: bool,
}

// === 大盘 / 板块 ===
pub struct MarketIndex { pub code: StockCode, pub name: String, pub price: Option<Yuan>, pub change_percent: Option<f64>, pub timestamp: i64 }
pub struct MarketBreadth { pub rise: u32, pub fall: u32, pub flat: u32 }
pub struct MarketOverview { pub indices: Vec<MarketIndex>, pub breadth: MarketBreadth, pub timestamp: i64 }
pub struct ConceptSector { pub code: String, pub name: String, pub member_count: usize }
pub struct ConceptPerformance { pub code: String, pub name: String, pub change_percent: f64, pub leader: Option<StockCode>, pub amount: Yuan }

// === 资金面 ===
pub struct TopListItem { pub trade_date: TradeDate, pub code: StockCode, pub name: String, pub close, pct_change, turnover_rate, amount, net_amount, net_rate: Option<f64>, pub reason: String }
pub struct MoneyFlowItem { pub trade_date: TradeDate, pub code: StockCode, pub net_small/net_mid/net_large/net_extra_large/net_total: Option<Yuan> }
pub struct NorthMoneyFlow { pub trade_date: TradeDate, pub sh_north: Yuan, pub sz_north: Yuan, pub total: Yuan }
pub struct NorthHolding { pub code: StockCode, pub holding_amount: Yuan, pub holding_ratio: f64 }
pub struct MarginSummary { pub trade_date: TradeDate, pub financing_balance: Yuan, pub margin_balance: Yuan, pub financing_buy: Yuan, pub margin_sell: Yuan }

// === 公司动作（新增）===
pub enum CompanyEvent {
    Dividend { announce_date: TradeDate, ex_date: TradeDate, cash_ratio: f64, stock_ratio: f64 },
    Suspension { begin_date: TradeDate, end_date: Option<TradeDate>, reason: String },
    StChange { effective_date: TradeDate, new_status: StStatus },
    EarningsForecast { period: String, forecast_type: ForecastType, range: (Option<f64>, Option<f64>), reason: String },
    ShareUnlock { unlock_date: TradeDate, unlock_shares: Lots, total_shares: Lots, ratio: f64 },
}
pub enum StStatus { Normal, ST, StarST, Delisted }
pub enum ForecastType { Increase, Decrease, Profit, Loss, Continued }

// === 交易日历 ===
pub struct TradeCalendar { pub trading_days: BTreeSet<TradeDate>, pub last_synced: i64 }

// === 基金 ===
pub struct FundInfo { ... }              // basic 信息
pub struct FundNavPoint { pub nav_date: TradeDate, pub unit_nav: f64, pub accum_nav, adj_nav: Option<f64> }
pub struct FundNavSeries { ... }
pub struct FundHolding { ... }
pub struct FundPortfolio { ... }
pub struct FundManagerList { ... }

// === 错误 ===
pub enum QuotesError {
    MissingToken,
    Network(String),
    Decode(String),
    Provider { source: &'static str, code: Option<i64>, msg: String },
    NotFound(String),
    InvalidInput(String),
    RateLimited,                          // TuShare 限流
    QuotaExceeded,                        // TuShare 积分不足
}

// ========== SimAccount / News / Agent 数据模型 ==========
// 见 § 2.2 / § 2.3 / § 2.4 各模块的 Public API 已经定义
```

### 3.4 Agent 输出协议

> 这一节是 LLM ↔ pipeline 之间的格式契约。当前只保留 Chat：Markdown + tool calls；Memory 有 merge 规则。
>
> 旧简报/复盘 JSON 结果模型已删除。未来恢复 briefing/review 时，必须重新定义新的输出协议和持久化模型，不能复用旧 JSON 契约。

#### Chat assistant `contentJson`

Chat 回复正文是 Markdown；assistant message 的 `contentJson` 字段存运行元数据：

```json
{
  "runId": "...",
  "turns": 2,
  "localToolCalls": 1,
  "serverToolCalls": 0,
  "memoryUpdates": {},
  "memoryRemovals": {}
}
```

用户图片：保存到 app data dir 后以 image block 形态发给 provider。支持 PNG / JPEG /
WebP / GIF，每条消息最多 4 张、每张最大 8 MB。

#### Memory 合并规则

`memory::merge_investor_memory(current, add, remove)` 的行为：

- **80 字 cap**：每条 entry 字数硬上限
- **字段 cap**：`focusThemes` 16 / `preferredMarkets` 8 / `learningGoals` 12 /
  `knownBiases` 12 / `investmentPrinciples` 18 / `watchQuestions` 18 / `recentInsights` 12
- **顺序**：新增 prepend → 去重 → trim → cap → 应用精确字符串 remove
- `riskPreference` 是单字符串字段，非空 remove 等于清空

#### Off-Contract（明确不做）

- ❌ Frontend 直接调 model API 或拼 prompt——必须走 `infrastructure::agent::provider::ChatProvider`
- ❌ Frontend 解析 provider 输出——只消费结构化 `AgentEvent`
- ❌ Resumed-by-id provider sessions——连续性来自 SQLite，每次 run wrt provider stateless
- ❌ 外部 CLI / stdio MCP——已被进程内 agent loop + `ToolRegistry` 替代
- ❌ `agent_tasks` 表的 runtime 写入——schema 保留只是历史残留（per § 1.6 后续会删）

---

## 4. 数据流：核心循环

```
┌──────────────────────────────────────────────────────────────┐
│                Agent 自驱动循环（snapshot-first）              │
│                                                               │
│  ┌──────────────────────────────────────────────────┐         │
│  │  Background tasks (持续运行)                       │         │
│  │  - Quotes refresh loop → 更新 MARKET_SNAPSHOT     │         │
│  │  - Account.rebuild_snapshot (events 变 / quotes 变) │       │
│  │  - News refresh loop → 入库 + 状态机              │         │
│  │  - SimAccount.scan_and_trigger_stops (周期)        │         │
│  └──────────────────────────────────────────────────┘         │
│                       ↑                                       │
│                       │  drives                               │
│                       │                                       │
│  (1) Trigger          │                                       │
│      user chat message（agent）/ scheduler tick（refresh only） │
│      │                                                        │
│      ↓                                                        │
│  (2) Agent read snapshots (sync, < 1ms)                       │
│      quotes::get_quote / get_market_overview / get_indicators │
│      account::get_snapshot                                    │
│      news repository search / recent reads                    │
│      │                                                        │
│      ↓                                                        │
│  (3) Agent decide (LLM + tool registry)                       │
│      工具调用：search_news / scan_market / get_kline / ...     │
│      │ （一次性查询走 async fetch，进 snapshot 给后续）         │
│      ↓                                                        │
│  (4) Agent act (写工具 mutation)                              │
│      account::open_position / close_position / ...            │
│        → 写 events + positions + 重建 snapshot                 │
│        → emit "positions-changed"                             │
│      │                                                        │
│      ↓                                                        │
│  (5) Agent log + notify                                       │
│      写 chat_messages（assistant 解释 + 决策摘要）              │
│      emit "chat-message-appended"                             │
│      │                                                        │
│      ↓                                                        │
│  (6) Persistence + Audit                                      │
│      agent_runs 写 audit row                                  │
│      │                                                        │
│      ↓                                                        │
│  (7) Feedback                                                 │
│      Background: scan_and_trigger_stops 触发止损止盈           │
│      Chat: 用户追问 / 指令触发下一次 agent run                  │
│      │                                                        │
│      ↓                                                        │
│  (8) Memory                                                   │
│      update_memory tool 更新长期记忆                           │
│      下次 prompt 自然带上新认知                                │
└──────────────────────────────────────────────────────────────┘
```

### 4.1 用户介入点

| 介入方式 | 实现 | 时机 |
|---|---|---|
| 给 agent 指导 | chat 消息 → agent 调 `update_memory` | 任何时候 |
| 命令 agent 操作 | chat 消息 → agent 调写工具 | 任何时候 |
| 重置账户 | UI 按钮 → `account::reset_account` 直调 | 用户主动 |
| 暂停 agent | settings: agentEnabled=false | 任何时候 |
| 查看决策理由 | chat 历史 / SimulationPage 持仓事件链 | 任何时候 |

**没有**：agent 出方案后等用户确认的流程。

---

## 5. 持久化原则

### 5.1 写顺序

```
1. write append-only event(s)  → position_events / news_items 状态机
2. update state snapshot       → simulated_positions / news_items payload status
3. rebuild in-memory snapshot  → ACCOUNT_SNAPSHOT / MARKET_SNAPSHOT 等
4. emit notification event     → "positions-changed" 等
```

**单 SQLite 事务包裹 (1)+(2)**。failure → rollback → state 不被半提交污染。
**(3) 失败不回滚 DB**——下一次重建 snapshot 时自纠正。

### 5.2 派生 over 存储

见 § 1.2 表格 + § 3.2 不变量。

### 5.3 启动恢复

进程崩溃后重启：
- ✅ stocks 表已有 → 跳过刷新
- ✅ 半提交的 news processing 状态 → 自动 revert 到 pending（`recover_stale_processing`）
- ✅ 各 snapshot 从 SQLite + 配置 hydrate（MARKET_SNAPSHOT 启动时空、第一次 refresh 后填）
- ✅ 进行中的 agent run 不恢复（user 重发即可）

---

## 6. 模块详细规约

### 6.1 Quotes 的"不变性"承诺

- 同一 `(code, period, adj, date)` 的 K 线值幂等不变（前复权除外）
- `compute_indicators(同输入) → 同输出`（pure function）
- `resolve_stock(code)` 在 stocks 表不变期间结果不变

### 6.2 SimAccount 的"原子性"承诺

每个 public 写方法保证：
- 要么全成功（events + position state + snapshot rebuild + emit）
- 要么全失败（什么都不写）
- 不存在"事件写了但 position 没翻状态"或反之

实现：rusqlite `Transaction` 包 events + positions；snapshot rebuild 在事务 commit 后；emit 在 rebuild 后。

### 6.3 Agent 的"决策可追溯"承诺

每次 agent 触发的 mutation 的 `SourceTag` 必填：
- 哪个来源（chat / auto_stop / system / future pipeline）
- 哪个 LLM run（agent_run_id）
- 关联的 news / chat_message / position event

---

## 7. 现状与目标的 Gap（重构 backlog）

> **DDD 4 层落地状态（2026-05-18 复审）**：依赖方向已全部单向化，pipeline → adapters 反向引用已消除，所有 BC 拆分到 `domain / infrastructure / pipeline / adapters` 四层。新功能开发遵循 § 1.7。剩余条目都是**业务能力增强**，不是结构债。

### 7.1 结构债（✅ 全部清零）

| 项 | 状态 |
|---|---|
| DDD 4 层结构 | ✅ adapters → pipeline → infrastructure → domain；grep 自检全绿 |
| BC 间隔离（quotes/account/news 不感知 agent）| ✅ grep `crate::*agent*` 在三个 BC 各层都 0 命中 |
| IPC 边界 | ✅ `#[tauri::command]` 只在 `adapters/`；前端不绕过 use case |
| Agent tool 边界 | ✅ 抽象 `pipeline/agent/tools` + 具体实现 `adapters/agent_tools`；registry 由 `adapters::chat_commands` 注入 pipeline |
| Account 聚合根 | ✅ `domain/account/aggregate.rs` 承接事件构造 + state 变更 |
| Newtype IDs | ✅ `PositionId / StockCode / NewsId / TradeDate / OccurredAt` |
| Account 模块独立目录 | ✅ domain 拆 `aggregate / position / events / snapshot / cash / rules / sizing` |
| News 模块独立目录 | ✅ `domain/news` + `infrastructure/news` + `pipeline/news`；`NewsStatus` 状态流 |
| Snapshot-first 数据访问 | ✅ 核心行情 / 账户读已走 snapshot；agent 工具按需 fetch 限于研究查询 |
| Quotes 全局 AppHandle | ✅ 显式参数为主 |
| 五档盘口数据 | ✅ StockQuote 含 bid/ask 五档、内外盘、委比（TDX 主路径填充） |
| Watchlist 归属 | ✅ Account-owned KV；Quotes 只消费 subscriptions |

### 7.2 业务能力 backlog（与 DDD 无关）

| 项 | 现状 | 目标 |
|---|---|---|
| `indicators_at_open` 持久化 | 未实现 | open 时算并存 PositionEvent payload |
| 交易日历 | clock.rs 硬编码 | 接 TuShare `trade_cal`，启动 hydrate 一年；scanner 倒推用日历不试错 |
| 指数历史 K | MarketIndex 只有快照 | `get_index_klines(index_code, period)` |
| StockQuote 数据新鲜度 | 只有 quote_time | 加 `captured_at`——agent 判断 snapshot 多旧 |
| 技术指标参数可配 | 硬编码周期 | `compute_indicators(klines, cfg: IndicatorConfig)` |
| Newtype 单位（金额/股数）| f64/i64 满天飞 | `Yuan / KYuan / Lots / Shares` 全链路覆盖 |
| Position 字段可见性 | 仍 public | 收窄到 private + 聚合根方法 |
| News claim/consume | 未启用 | 恢复定时 agent workflow 时接入调度 |

---

## 8. 文档更新规则

- 任何架构 / 接口 / 数据模型变更**先改本文档**再写代码
- 任何新功能开发**按 § 1.7 的 7 步走**：先选 BC + 选层，再写代码
- PR description 引用本文档对应章节（最少 § 1.3 依赖矩阵 + § 1.7 工作流）
- 模块边界 / 核心哲学的改动需要 ADR — 放到 `docs/design/` 子目录
- 本文档保持精简（< 1500 行）；细节往子文档迁

---

## 9. DDD-lite 结构约定（**新增**）

### 9.1 目录结构

```
src-tauri/src/
├── domain/                          ← 纯 domain，无 I/O，无 Tauri 依赖
│   ├── shared/                      跨 BC 复用
│   │   ├── ids.rs                   newtype IDs (PositionId, StockCode, ...)
│   │   ├── money.rs                 Money value object
│   │   ├── shares.rs                Shares value object (整百校验)
│   │   ├── time.rs                  TradeDate, OccurredAt
│   │   └── errors.rs                共享错误类型
│   ├── quotes/                      Quotes Bounded Context
│   │   ├── types.rs                 KlinePoint, StockQuote, IndicatorSnapshot, ...
│   │   ├── indicators.rs            纯函数：compute_indicators / MA / RSI / ...
│   │   └── mod.rs
│   ├── account/                     Account Bounded Context
│   │   ├── aggregate.rs             Account 聚合根
│   │   ├── position.rs              Position 实体
│   │   ├── events.rs                PositionEvent enum + payload
│   │   ├── snapshot.rs              AccountSnapshot
│   │   ├── rules.rs                 A 股规则校验（纯函数）
│   │   ├── sizing.rs                仓位 sizing 规则
│   │   ├── cash.rs                  现金派生
│   │   ├── types.rs                 兼容 facade（重构期）
│   │   └── mod.rs
│   ├── news/                        News Bounded Context
│   │   ├── types.rs                 NewsItem / NewsId / NewsStatus + 状态转移规则
│   │   ├── errors.rs                NewsError
│   │   └── mod.rs
│   └── agent/                       Agent Bounded Context
│       ├── types.rs                 AgentRequest / AgentEvent / ProviderKind / PipelineKind
│       ├── memory.rs                InvestorMemory VO
│       └── mod.rs
│
├── infrastructure/                   ← I/O 实现（HTTP / DB / cache / provider）
│   ├── quotes/                      Quotes 子域 I/O
│   │   ├── tushare/                 TuShare HTTP（历史 K / 财务 / 基金 / 板块）
│   │   ├── eastmoney/               EM HTTP（分时 / 分钟 K / 实时 fallback）
│   │   ├── tdx/                     **TDX TCP**（完整端口 mootdx-rust，实时报价主路径）
│   │   │   ├── types.rs / error.rs / helper.rs / hosts.rs
│   │   │   ├── client/              TdxHqClient + cmd（security_quotes / bars / list / count）
│   │   │   └── reader/              本地 .day / .lc1 / .lc5 离线文件解析
│   │   ├── realtime/                多源 dispatch（TDX > EM > 腾讯 > 新浪）
│   │   │   ├── tdx.rs / em.rs / tencent.rs / sina.rs
│   │   │   ├── proxy_pool.rs        SOCKS5 / HTTP 代理池 + EMA 健康度
│   │   │   └── mod.rs               DispatchSource 递增填充 + 健康度排序
│   │   ├── cache/                   SQLite TTL 缓存（kline_cache / minute_kline_cache）
│   │   ├── snapshot/                MARKET_SNAPSHOT 内存
│   │   └── core_indexes.rs          4 大核心指数硬编码
│   ├── account/                     Account 子域 I/O
│   │   ├── repository.rs            position + event 持久化（含 domain ↔ DB 投射）
│   │   ├── watchlist.rs             Account-owned 自选股 KV
│   │   ├── valuation.rs             AccountSnapshot 派生
│   │   ├── snapshot_cache.rs        ACCOUNT_SNAPSHOT in-memory
│   │   └── migration.rs             legacy 数据 → events 一次性补偿
│   ├── news/                        NewsNow / RSS / article extractor / repository
│   ├── agent/                       LLM provider + agent/chat repository
│   ├── app_state/                   KV repository
│   ├── db/                          SQLite connection / migrations
│   └── ...
│
├── pipeline/                         ← 用例编排（application layer）
│   ├── account/                      AccountService + subscriptions + close
│   ├── agent/                        agent loop / prompt / compact / config / observer
│   │   └── tools/                    Tool trait + ToolContext + ToolRegistry（抽象，无具体 tool）
│   ├── chat.rs                       chat use case：接受 ToolRegistry 注入
│   ├── market/                       refresh / overview / universe / kline_warm
│   ├── news/refresh.rs
│   ├── scheduler.rs                  news / market / account / kline 后台 loop
│   ├── history.rs · memory.rs · context.rs · quotes_fetch.rs · events.rs ·
│   │   stocks.rs · chat_attachments.rs · util.rs   跨 use case 复用的 helper
│   └── mod.rs
│
├── adapters/                         ← Tauri / LLM 边界
│   ├── *_commands.rs                #[tauri::command] 函数（唯一 IPC surface）
│   ├── agent_tools/                 具体 LLM 工具（实现 pipeline::agent::tools::Tool）
│   │                                + build_chat_registry 工厂；chat_commands 在每次 run
│   │                                启动时构造 registry 注入 pipeline
│   └── mod.rs
│
└── main.rs
```

### 9.2 Newtype IDs（强制）

```rust
// domain/shared/ids.rs
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PositionId(String);
impl PositionId { pub fn new() -> Self { Self(Uuid::new_v4().to_string()) } pub fn as_str(&self) -> &str { &self.0 } }

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StockCode(String);
impl StockCode {
    pub fn new(s: impl Into<String>) -> Result<Self, InvalidIdError> {
        let s = s.into();
        if s.len() == 6 && s.chars().all(|c| c.is_ascii_digit()) {
            Ok(Self(s))
        } else {
            Err(InvalidIdError::BadStockCode(s))
        }
    }
    pub fn as_str(&self) -> &str { &self.0 }
}

pub struct NewsId(String);
pub struct ChatMessageId(String);
pub struct AgentRunId(String);
```

**规则**：所有跨函数传递的 ID 都用 newtype，**禁止用 `String`**。编译期防 ID 混淆。

### 9.3 Value Objects（强制）

> Newtype 单位 / 量化金额——编译期防止"手 vs 股"、"元 vs 千元"、"百分数 vs 万分比"混淆。

```rust
// domain/shared/money.rs
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Yuan(f64);                   // 元（内部 / 前端展示）
impl Yuan {
    pub fn new(v: f64) -> Result<Self, ValueError> {
        if v.is_finite() { Ok(Self(v)) } else { Err(ValueError::NonFinite) }
    }
    pub fn value(&self) -> f64 { self.0 }
    pub fn from_kyuan(v: KYuan) -> Self { Self(v.value() * 1000.0) }
}
pub struct KYuan(f64);                  // 千元（TuShare amount 字段默认单位）
impl KYuan { /* 类似 */ }

// 历史保留的 Money 别名（per § 1.6 等迁完直接删，不留 deprecated）
pub type Money = Yuan;                  // 临时迁移期；迁完删

// domain/shared/shares.rs
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Shares(i64);                 // 股
impl Shares {
    pub fn new(n: i64) -> Result<Self, RuleError> {
        if n < 100 || n % 100 != 0 { Err(RuleError::SharesNotIntegerLot { shares: n }) }
        else { Ok(Self(n)) }
    }
    pub fn value(&self) -> i64 { self.0 }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Lots(i64);                   // 手（A 股交易单位，1 手 = 100 股）
impl Lots {
    pub fn new(n: i64) -> Self { Self(n.max(0)) }
    pub fn value(&self) -> i64 { self.0 }
    pub fn to_shares(self) -> Shares { Shares::new(self.0 * 100).expect("lots * 100 always integer-lot") }
}
impl From<Shares> for Lots { fn from(s: Shares) -> Self { Self(s.value() / 100) } }

// domain/shared/trade_date.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct TradeDate(i32);              // YYYYMMDD 整数表示，可比较可排序
impl TradeDate {
    pub fn new(yyyymmdd: i32) -> Result<Self, ValueError> { /* validate */ }
    pub fn from_iso(s: &str) -> Result<Self, ValueError> { /* "YYYY-MM-DD" → 整数 */ }
    pub fn to_iso(&self) -> String { format!("{:04}-{:02}-{:02}", ...) }
    pub fn next(&self) -> TradeDate { /* 加一日历日，不考虑节假日 */ }
}

// domain/shared/time.rs
pub struct OccurredAt(i64);             // unix ms
impl OccurredAt {
    pub fn now() -> Self { Self(chrono::Utc::now().timestamp_millis()) }
    pub fn beijing_today() -> TradeDate { /* UTC+8 转 YYYYMMDD */ }
}
```

**单位换算规则**（编译期保证）：

```rust
let amount_kyuan: KYuan = tushare_response.amount.into();   // TuShare 返千元
let amount_yuan: Yuan = Yuan::from_kyuan(amount_kyuan);     // 显式换算
let shares: Shares = lots.to_shares();                       // Lots → Shares 显式
let market_value: Yuan = price.value() * shares.value() as f64;  // 类型一致才能算
```

**反模式（编译器拦下）**：
- ❌ `let mv = price * shares` —— Yuan * Shares 编译报错
- ❌ `let amount: f64 = response.amount;` —— 失去单位信息
- ❌ `lots.value() == shares.value()` —— 不同类型比较

### 9.4 Aggregate Root：Account

```rust
// domain/account/aggregate.rs
pub struct Account {
    positions: Vec<Position>,      // private
}

impl Account {
    pub fn new(positions: Vec<Position>) -> Self { ... }
    pub fn open_position(&mut self, cmd: OpenPositionCommand) -> Result<AccountMutation, AccountError> { ... }
    pub fn close_position(&mut self, cmd: ClosePositionCommand) -> Result<AccountMutation, AccountError> { ... }
    pub fn scale_position(&mut self, cmd: ScalePositionCommand) -> Result<AccountMutation, AccountError> { ... }
    pub fn adjust_stops(&mut self, cmd: AdjustStopsCommand) -> Result<AccountMutation, AccountError> { ... }
}
```

**关键设计**：
- `positions` 是 private——外面无法绕过聚合根替换仓位集合
- 所有 mutation 走 Account 方法，方法内强制走"校验 → 生成 event → 应用到 positions 快照"流程
- `AccountMutation` 返回 `position + event + positions`，pipeline/account service 负责单事务落盘和 emit

### 9.5 Repository Pattern（当前实现）

当前 Account 持久化由 `infrastructure/account/repository.rs` 的 `PositionRepo` 承接，pipeline/account service 负责把聚合根返回的 mutation 落成单事务。

```rust
pub struct PositionRepo { app: AppHandle }

impl PositionRepo {
    pub fn list_all(&self) -> Result<Vec<Position>, AccountError> { ... }
    pub fn list_open(&self) -> Result<Vec<Position>, AccountError> { ... }
    pub fn list_events_batch(&self, ids: &[PositionId]) -> Result<Vec<PositionEvent>, AccountError> { ... }
    pub fn commit_event_and_positions(&self, event: &PositionEvent, positions: &[Position]) -> Result<(), AccountError> {
        // 单事务：append PositionEvent + replace simulated_positions snapshot
    }
}
```

### 9.6 反模式（明确不做）

| ❌ 不做 | 理由 |
|---|---|
| 分离 domain 成独立 crate | 编译变慢、IDE 跳转麻烦；single binary 够用 |
| Event Sourcing（事件回放重建状态） | 已用 SQLite 直接存 state；events 是审计 + 派生 cash 用，不需要全回放 |
| CQRS 读写分离 | 单用户 < 100 ops/sec，没必要 |
| 6 层 Hexagonal Architecture | over-abstract，4 层（domain/infra/app/adapters）够用 |
| Saga / Process Manager | 现有 pipeline 已足 |
| Domain Service / Application Service 两层分离 | 合一就行 |

---

## 10. 设计决策记录（**新增**）

> 这一节记录"为什么这么选"。后续遇到诱惑想反过来时回看这里。

### Q1: 自动止损止盈 → **SimAccount 自治**

`SimAccount::scan_and_trigger_stops` 是 SimAccount 内部方法；scheduler 周期调用。

**选 SimAccount 自治的理由**：
- "什么触发平仓"是账户规则的一部分（stop_loss 字段本身在 Position 上）
- 触发逻辑（price ≤ stop_loss）是纯 domain 规则
- scheduler 只是个 ticker，不该懂"规则"

**实现**：内部调 `quotes::get_quotes_batch` 读 snapshot → 应用规则 → 触发的调 `Account::close`。

### Q2: `indicators_at_open` → **冻结存到 PositionEvent payload**

开仓时 agent / SimAccount 调 `quotes::compute_indicators(klines)` 算一份 snapshot，存到 `PositionEvent::Opened` 的 payload 里。Position 实体也带 `indicators_at_open: Option<IndicatorSnapshot>`。

**理由**：
- 审计 + 学习用：一年后看历史持仓能复盘"当时看到什么"
- 不会因后续 K 线变化导致 fake history
- 多花一次 fetch_klines + compute_indicators 调用值得（开仓本身是低频操作）

### Q3: News 不做关联股票识别 → **保持原始资讯**

NewsItem 只存源数据（id, title, summary, url, status）；不做"自动识别这条资讯影响哪些股票"。

**理由**：
- 模块职责单一
- 关键词匹配误识别率高（不同公司同名 / 概念股 / 联想关系）
- LLM 在 agent run 中能自己识别——这正是 agent 的工作
- 后续要做也是 agent 工具（`tag_news_with_symbols`），不是 News 模块的事

### Q4: Snapshot 不重复缓存 → **复用 Quotes snapshot**

Account snapshot 计算 `market_value` 时读 `quotes::get_quotes_batch` 拿当前价。Quotes 内部已有 in-memory snapshot（refresh loop 维护），不需要 Account 再加一层缓存。

**理由**：
- 单一真源（Quotes snapshot 是行情真源）
- 避免双层缓存的失效不同步问题
- Account snapshot 重建是事件驱动（events 变 / quotes snapshot 更新事件），不是定时拉

### Q5: `position_events.payload` → **JSON 通用**

`payload_json` 列存 JSON。内部用 Rust enum + serde 强类型化（`PositionEventPayload`），但 DB schema 是 JSON 字段。

**理由**：
- 加新 event kind 不改 schema
- agent 主要 walk events（不 SQL 查内部字段），JSON 性能损失可忽略
- 强类型化在 Rust 层做（enum + serde），DB 层简单

### Q-CORE: Snapshot-First Data Access → **核心架构约定**

> 这是这次最重要的设计决定，单独记录。

**决定**：Agent / Chat / UI **不直接 fetch 数据源**。所有读取走**内存 snapshot**——由 source 模块（Quotes / Account / News）自己用后台任务近实时维护。

**理由**：
- 解耦：Agent 不感知数据怎么来、什么时候刷新
- 性能：决策路径不等网络 I/O
- 一致性：同一时刻的 snapshot 是全局一致视图
- 可测：mock snapshot 就能测 agent 行为

**实现**：
- Quotes：refresh loop 周期更新 `MARKET_SNAPSHOT`；agent 调 `get_quote(code)` 同步读
- Account：events 变化 + Quotes snapshot 更新事件触发 `rebuild_snapshot`；agent 调 `get_snapshot()` 同步读
- News：保持当前 pull 模式（资讯不需要那么实时）

**约束**：
- 任何模块都**不允许**在 hot path 上 trigger fetch（除非显式 `ensure_*` 调用）
- snapshot 必须**永远可读**——绝不阻塞调用者

### Q-UNIT: Newtype 单位（强制）

**决定**：金额 / 股数 / 日期 用 newtype 区分单位，编译期防混。

- `Yuan` / `KYuan` — 元 / 千元，互转必须显式
- `Lots` / `Shares` — 手（A 股交易单位） / 股，互转必须显式
- `TradeDate` — YYYYMMDD 整数，可比较可排序，区别于普通日期串
- `OccurredAt` — unix ms 时间戳，区别于"显示用时间串"

**理由**：cash 漂移、量额单位混淆是历史 bug 重灾区。f64 不带语义，编译器拦不下"把成交额当成交量"。

**反模式**：`f64` / `i64` 满天飞且字段名不带单位提示。

### Q-PROFILE: trader 完整性扩展（非纯研究员）

**决定**：Quotes 模块按"研究员 + 短线 trader 综合工具"定位扩展数据完整性。

具体覆盖：
- 盘口：五档 + 委比 + 内外盘
- 多周期：日/周/月 + 1m/5m/15m/30m/60m 分钟 K
- 基本面：PE/PB/换手/市值（DailyBasic）
- 资金面：主力 + 北向 + 融资融券
- 公司动作：分红 / 停牌 / ST / 业绩预告 / 解禁
- 板块：概念列表 + 涨跌榜 + 成分
- 交易日历：trade_cal 接入

**理由**：纯研究员工具做不了日内 / 短线 / 量化策略；多花一点接入成本换全栈定位。

**反模式**：把 Quotes 模块当成"只读 K 线 + 实时报价"的窄定义。

### Q-DISPATCH: 实时报价多源 dispatch + TDX 主路径

**决定**：实时报价改用**多源递增填充**模式，优先级 `TDX > EM > 腾讯 > 新浪`。

**实现位置**：`infrastructure/quotes/realtime/`，含 `tdx.rs` / `em.rs` / `tencent.rs` / `sina.rs` 四个 `RealtimeQuoteSource` 实现 + `mod.rs` 的 `DispatchSource`。

**TDX 主路径**：
- 完整端口 mootdx-rust（参考 <https://github.com/mootdx/mootdx>）到 `infrastructure/quotes/tdx/`，含 types / client / cmd / reader 全套（~1600 行）
- 16 个公共 HQ 服务器（华泰 / 招商 / 上海电信 / 北京联通 / 杭州电信...）+ `connect_bestip()` 并行竞速
- 私有 TCP 二进制协议，单 IP 风控基本无效
- 同步 TCP → `tokio::task::spawn_blocking` 包成 async

**递增填充模式**：
```rust
let mut filled: HashSet<String> = HashSet::new();
for src in order {
    let pending = ts_codes.iter().filter(|c| !filled.contains(c.as_str())).collect();
    if pending.is_empty() { break; }
    src.fetch(pending) → 拿到的 ts_code 入 filled
}
```

**分工**：
- **TDX**（主）：SH/SZ 所有标的；BJ 静默跳过（mootdx Market enum 只有 SZ/SH）
- **EM**（补缺）：BJ 北交所标的 + TDX 偶发故障时的兜底
- **腾讯 / 新浪**：极端场景兜底

**频率**（scheduler `market_quote_interval`）：
- 盘中 15s（TDX 无风控压力）
- 盘外 60s
- 周末 / 节假日 10min

**理由**：
- 之前 EM 单源 90s 仍频繁触发 IP 风控（私有 IP 黑名单）
- TDX 多服务器分散 + 二进制协议 = 抗风控最强
- 健康度 EMA + 拉黑：拿到 ≥1 条算成功，全失败才扣分；TDX 处理 SH/SZ 不被"没处理 BJ"扣健康度

**代理池**（次要）：`realtime/proxy_pool.rs` 支持用户配 SOCKS5 / HTTP 代理列表，TDX 不走（TCP 不通用），EM / 腾讯 / 新浪 走。代理池在 TDX 出现前是抗风控主方案，TDX 接入后转辅。

**反模式**：
- ❌ 单源直连——任何源被 IP 风控就全挂
- ❌ "全失败才 fallback"——TDX 不支持 BJ 时整批失败浪费 TDX 优势

### Q-ALERT: Alerting 不在当前范围（标记，未实现）

**决定**：当前所有 Quotes API 都是 pull（同步读 snapshot 或 async fetch）。**没有 push / 订阅 / 触发告警**机制。

**理由**：scope 控制——告警 / 监控涉及独立的事件系统、阈值配置、推送通道。Agent 现有的"周期 refresh + scan"模式能覆盖 95% 决策路径。

**未来加 alert 时**：单独设计 alert engine 模块，不要把告警逻辑塞进 Quotes。

---

**版本**：2026-05-12 整体重写 + 2026-05-13 trader 视角数据扩展
**前一版**：见 git history（commit `39423a2`）
