# Agent v3 — Expectation-Driven 设计

> **状态**：审定后落地。本方案吸收 v2 第三方 audit 暴露的硬伤（学习闭环断点 / 自然语言 invalidation 无法代码判定 / proposed→active 死路）+ 真实交易员学习模式（Steenbarger 日志循环 / Dalio Principles / Annie Duke 决策vs结果 / Bayesian 信念更新 / 量化策略生命周期）+ 用户产品方向（多 tick 主动扫描 / 视觉形态分析 / 消息面集成）。
> **落地形态**：稳定后**直接替换** v2 设计（`docs/design/agent-redesign.md`）+ `pipeline/agent/identity.md` + `docs/architecture.md` 相关章节。**Thesis 整个概念删除**。
> **范围**：Agent + Account + News 三个 BC，前端 chat / settings / 全部 agent 相关页。

---

## 关键概念

读 v3 前必懂的 6 个概念。后面章节默认你已经知道。

### Expectation（投资预期）
**一个可量化、可代码验证的押注**。"我赌 600519 在 8 个交易日内涨到 1850"。带 `code / direction / target_price / horizon_days / reasoning / signals_used / theme / state`。**Expectation 是 v3 整个系统的核心实体**——所有学习、决策、复盘都围绕它转。取代 v2 的 Thesis。

### Strategy（策略）
**一组"什么时候建 Expectation"的规则**。用户 + agent 共建。例："放量突破 20MA + 板块强势 + 北向净流入 → 建 expectation 目标 +5% 8 天"。多个 Strategy 可并存（突破型 / 回踩型 / 事件驱动），各自跟踪命中率。

### Signal（信号）
**触发 Expectation 的原子条件**。枚举 24 个 + Custom 兜底——分趋势 / 摆动 / 量能 / 资金 / 板块 / 因子 / 视觉 / 消息 7 类。规则信号由代码自动检测（cheap, always-on），视觉信号由 LLM 看图识别。

### Lesson（教训）
**每次 Expectation 终态时自动生成的原子观察**。"在 ST 板块涨停日追了，被诱多套住 7%"。永不修改、永不删除——是 Heuristic emerge 的原料。

### Heuristic（启发式规则）
**积累足够支持的 Lessons 之后浮现的可重用规则**。带 `body / supporting_lesson_ids / application_count / hit_count / miss_count / confidence` 字段。Phase 1 替代 v2 的 Principle，差异是**有 track record**——不能凭空写，必须基于实证。

### Tick（扫描节拍）
**自驱观察循环的最小单位**。Phase 1 设 9 ticks：盘前 + 8 盘中/盘后。每个 tick 分两阶段：① 规则信号检测（纯代码，always-on）② LLM mini-scan（仅当 ≥1 信号触发）。

---

## 0. 现状诊断（v2 → v3 升级动因）

v2 重构后第三方 audit 揭露的硬伤（详见 audit transcript 历史）：

1. **学习闭环字面意义为 0** — `agent_inferred` principle 永远停在 proposed，下次 chat 看不见
2. **Thesis 是 prompt 约束不是代码约束** — `invalidation` 是自然语言，代码无法自动判定，全靠 LLM 自觉读
3. **regime / hit_count 反作弊全失效** — `current_regime=None` 硬编码；`increment_hit` 0 调用方
4. **没有自驱观察** — 只 15:30 一条 tick，agent 不主动看市场
5. **消息面被动检索** — agent 不调 `search_news` 就完全看不到资讯，资讯没自动关联到股票
6. **形态识别空白** — 头肩顶 / 双底等叙事性形态既没算法也没视觉
7. **学习是"自上而下"凭空写** — `propose_principle` 让 LLM 直接写抽象原则，没有 lessons 支撑

v3 用 **Expectation 一等 + Signal 结构化 + Lesson 自动累积 + Heuristic 浮现 + 9-tick 自驱 + News tagger + 视觉分析**一次性堵掉这 7 个洞。

---

## 1. 目标形态：闭环架构

```
┌────────────────────────────────────────────────────────────────────┐
│  Strategy 层（用户+agent 共建的规则集，可热改）                       │
│    └─ trigger_when[]:SignalCondition  target:TargetRule  track ↻    │
└────────────────────────────────────────────────────────────────────┘
                  ↓ 应用
┌────────────────────────────────────────────────────────────────────┐
│  Tick 调度（9 ticks/天 + chat 触发 + News High 即时触发）            │
│                                                                     │
│  阶段 1：规则信号扫描 50 自选股 × 24 信号（纯代码，0 LLM）            │
│           触发？─Y─> 阶段 2                                         │
│             N─> tick 结束                                           │
│                  ↓                                                  │
│  阶段 2：LLM mini-scan（仅触发股）                                   │
│           看 Strategy → 决定建/调/撤 Expectation                    │
└────────────────────────────────────────────────────────────────────┘
                  ↓
┌────────────────────────────────────────────────────────────────────┐
│  Expectation（核心实体）                                             │
│    code + direction + target_price + horizon + signals_used         │
│    reasoning + theme + state:pending/hit/missed/expired/...         │
└────────────────────────────────────────────────────────────────────┘
                  ↓ 到期或提前到价
┌────────────────────────────────────────────────────────────────────┐
│  Expectation Review（代码自动判 hit/miss）                            │
│    hit  → signals_used 各 +1 hit                                    │
│    miss → signals_used 各 +1 miss + 自动写 Lesson                   │
│    expired → 不计 hit/miss（节奏判断错而已）                          │
└────────────────────────────────────────────────────────────────────┘
                  ↓
┌────────────────────────────────────────────────────────────────────┐
│  Lesson 累积  →  ≥2 共有模式 emerge Heuristic                       │
│    Heuristic.supporting_lesson_ids[] / hit_count / miss_count       │
│    confidence = hit / (hit + miss)                                  │
└────────────────────────────────────────────────────────────────────┘
                  ↓
┌────────────────────────────────────────────────────────────────────┐
│  Heuristic.confidence > 0.6 → 进 chat prompt（Active）              │
│  Heuristic.confidence 0.3-0.6 → 进 prompt 带 ⚠️ Challenged          │
│  下次 LLM mini-scan / chat 看到自己学到的——闭环真闭合                │
└────────────────────────────────────────────────────────────────────┘
```

关键差异 vs v2：
- **Expectation 取代 Thesis** → 自然语言失效条件 → 价格+时间硬目标 → 代码可自动判定
- **Lesson 是底层原料** → Heuristic 从 lessons emerge → 不允许凭空写
- **confidence 连续** → 替代 proposed/active 离散状态
- **9 tick** → agent 自驱观察 → 不再被动等用户

---

## 2. 数据模型

### 2.1 新增表（v3 schema bump）

```sql
-- 核心：Expectation（替代 v2 的 theses）
create table expectations (
    id text primary key,
    code text not null,
    direction text not null check (direction in ('up', 'down', 'range_bound')),
    target_price real,                       -- nullable when state='watching'
    target_price_ceiling real,               -- 区间目标的上沿（可选）
    horizon_days integer not null,
    reasoning text not null,
    signals_used text not null,              -- JSON array of SignalKind serialized
    conviction text not null check (conviction in ('low', 'medium', 'high')),
    theme text,                              -- 跨股聚合标签
    supersedes_expectation_id text,          -- 链向上一个
    state text not null check (state in ('pending', 'hit', 'missed', 'expired', 'cancelled', 'superseded')),
    regime_at_creation text,
    created_at text not null,
    expires_at text not null,
    closed_at text
);
create index idx_expectations_code_state on expectations(code, state);
create index idx_expectations_state_expires on expectations(state, expires_at);
create index idx_expectations_theme on expectations(theme);

-- Expectation 事件链（append-only 状态机记录）
create table expectation_events (
    id integer primary key autoincrement,
    expectation_id text not null,
    kind text not null,                      -- created/hit/missed/expired/cancelled/superseded/user_feedback/note
    payload text,                            -- JSON
    occurred_at text not null
);
create index idx_expectation_events_id on expectation_events(expectation_id, occurred_at);

-- Strategy（用户+agent 维护的规则集）
create table strategies (
    id text primary key,
    name text not null,
    description text,
    config_json text not null,               -- 完整 DSL：trigger_when[] + target
    enabled integer not null default 1,
    applied_count integer not null default 0,
    hit_count integer not null default 0,
    miss_count integer not null default 0,
    created_at text not null,
    updated_at text not null
);

-- Lesson（per-expectation 原子观察，自动生成）
create table lessons (
    id text primary key,
    expectation_id text not null,
    code text not null,
    observation text not null,               -- "在 ST 板块涨停日追入，被诱多套住 7%"
    takeaway text not null,                  -- "ST 板块涨停日的回踩通常是诱多"
    outcome text not null check (outcome in ('hit', 'miss', 'expired')),
    regime_at_close text,
    signals_in_play text,                    -- JSON array
    pnl_pct real,                            -- 关联持仓的盈亏百分比（可选）
    created_at text not null
);
create index idx_lessons_expectation on lessons(expectation_id);
create index idx_lessons_code_time on lessons(code, created_at desc);

-- Heuristic（替代 v2 的 principles，带 track record）
create table heuristics (
    id text primary key,
    body text not null,
    category text not null check (category in ('principle', 'known_bias', 'risk_preference')),
    origin text not null check (origin in ('user_stated', 'agent_inferred', 'seed')),
    regime_tags text,                        -- JSON array
    supporting_lesson_ids text,              -- JSON array（user_stated/seed 可为空）
    application_count integer not null default 0,
    hit_count integer not null default 0,
    miss_count integer not null default 0,
    last_applied_at text,
    retired_at text,
    retired_reason text,
    created_at text not null
);
create index idx_heuristics_origin on heuristics(origin);
create index idx_heuristics_confidence on heuristics(hit_count, miss_count);

-- Signal detection log（per-tick 检测结果，调试 + 命中率统计用）
create table signal_detections (
    id integer primary key autoincrement,
    tick_id text not null,                   -- 关联到 agent_episodes.run_id 或独立 scan tick id
    code text not null,
    signal_kind text not null,               -- SignalKind serialized
    signal_params text,                      -- JSON
    detected_at text not null
);
create index idx_signal_detections_code_time on signal_detections(code, detected_at desc);
create index idx_signal_detections_tick on signal_detections(tick_id);

-- News tagger 输出（v3 新增消息面集成）
create table news_tags (
    news_id text primary key,
    kind text not null check (kind in ('earnings','halt','restructure','regulatory','ownership','operating','policy','sector_trend','market','other')),
    importance text not null check (importance in ('high','medium','low')),
    sectors text,                            -- JSON array
    tagged_at text not null
);

create table news_tickers (
    news_id text not null,
    code text not null,
    primary key (news_id, code)
);
create index idx_news_tickers_code_time on news_tickers(code, news_id);
```

### 2.2 删除表

- `theses` / `thesis_codes` / `thesis_events` —— v2 Thesis 整体下线
- `principles` —— 改名为 `heuristics`（schema 大改：加 supporting_lesson_ids / application_count / hit_count / miss_count / retired_at），不保留旧数据

### 2.3 修改表

- `simulated_positions.thesis_id` → `simulated_positions.current_expectation_id`（语义更准；agent 主动开仓必有，用户命令开仓可空）
- `watchlist_entries` 加 `theme: TEXT NULL`
- `agent_episodes.thesis_ids` → `agent_episodes.expectation_ids`

### 2.4 SCHEMA_VERSION 升到 3

`infrastructure/db/connection.rs::SCHEMA_VERSION = 3`，启动时若现存 < 3 → 备份为 `gangzi-terminal.sqlite3.legacy-v2-{ts}` + 重建。

---

## 3. Signal 枚举（24 + Custom）

```rust
// domain/agent/signal.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SignalKind {
    // ===== 趋势 / 动量（4） =====
    BreakoutAbove20MA,
    BreakoutBelow20MA,
    MA5CrossAbove20,
    MA5CrossBelow20,
    MACDGoldenCross,
    MACDDeathCross,
    New20DayHigh,
    New20DayLow,

    // ===== 摆动 / 均值回归（2） =====
    RSIOversold { period: u32 },       // 默认 14
    RSIOverbought { period: u32 },
    BollingerBreakUpper,
    BollingerBreakLower,

    // ===== 量能（3） =====
    VolumeSpike { ratio: f32 },        // 量比 > ratio（默认 1.5）
    VolumeShrink { ratio: f32 },       // 量比 < ratio（默认 0.5）
    VolumePriceDivergence,

    // ===== 资金 / 主力（3） =====
    NorthInflowStreak { days: u32 },
    NorthOutflowStreak { days: u32 },
    OnDragonTigerList,

    // ===== A 股特殊（2） =====
    LimitUp,
    LimitDown,
    LimitUpFlooded,                    // 一字板

    // ===== 板块 / 事件（2） =====
    SectorStrengthAbove { pct: f32 },
    UpcomingEvent { kind: String, days_ahead: u32 },

    // ===== 基本面因子（4） =====
    PEBelowSectorPct { pct: f32 },
    PBBelowThreshold { value: f32 },
    ROEAboveThreshold { pct: f32 },
    EarningsGrowthAbove { pct: f32 },

    // ===== 消息（1） =====
    NewsCatalystMatched {
        kind: NewsKind,                // earnings/halt/restructure/...
        importance: NewsImportance,    // high/medium/low
    },

    // ===== 视觉（1） =====
    VisualPatternRead {
        pattern: String,               // "double_bottom" / "head_and_shoulders_top" / ...
        confidence: f32,
        timeframe: String,             // "day"/"week"/"60m"
    },

    // ===== 兜底 =====
    Custom { tag: String },
}
```

24 个枚举（含可调参数变体）。每个 Signal 检测路径：

| 类别 | 检测代码位置 | 数据源 |
|---|---|---|
| 趋势/摆动/量能/形态算法 | `infrastructure/quotes/signal_detector.rs::technical` | `KLINE_SNAPSHOT` + indicators |
| 资金 | `signal_detector.rs::moneyflow` | 现有 `get_top_list`/`get_moneyflow`/`get_north_flow` workers |
| A 股特殊 | `signal_detector.rs::a_share_specific` | `MARKET_SNAPSHOT.StockQuote`（涨跌停判定） |
| 板块 | `signal_detector.rs::sector` | `get_concept_performance` cache |
| 因子 | `signal_detector.rs::fundamental` | TuShare `daily_basic` + `income` |
| 消息 | `infrastructure/news/tagger.rs` 入库时打 + scan tick 拉最近窗口 | `news_items` + `news_tags` + `news_tickers` |
| 视觉 | LLM 通过 `analyze_chart` 工具看图后调 `propose_visual_pattern` | `chart_renderer.rs` 渲染的 PNG |

---

## 4. Strategy DSL

Strategy 是 JSON 文档，落到 `strategies.config_json`：

```json
{
  "id": "breakout-momentum-default",
  "name": "动量突破策略",
  "description": "放量突破 20MA + 板块共振 + 北向加仓 → 5-8 天目标位",
  "trigger_when": [
    {"signal": "BreakoutAbove20MA"},
    {"signal": "VolumeSpike", "params": {"ratio": 1.5}},
    {"signal": "SectorStrengthAbove", "params": {"pct": 3.0}}
  ],
  "trigger_logic": "AND",
  "target": {
    "direction": "up",
    "pct_above_current": 7,
    "horizon_days": 8
  },
  "conviction_rule": {
    "high_if": [
      {"signal": "NorthInflowStreak", "params": {"days": 5}}
    ],
    "medium_default": true
  },
  "enabled": true
}
```

**DSL 设计要点**：
- `trigger_when` 数组里所有条件 AND 起效；想 OR 就建多个 Strategy
- `target` 用相对当前价的百分比（不是绝对价）+ 相对天数，方便复用
- `conviction_rule` 可选，根据附加 signals 升级 conviction（不影响 trigger）
- `enabled` 用户可一键禁用某条 strategy 不删

**Phase 1 内置 3 条 seed strategies**（启动时 seed_strategies.rs 注入）：
1. **动量突破型**（上面例子）
2. **超跌反弹型**（RSI < 30 + BollingerBreakLower + 板块未恶化 → 反弹 +4% 5 天）
3. **资金驱动型**（OnDragonTigerList + 当日涨幅 > 5% + NorthInflowStreak → +6% 10 天）

用户在 chat 可让 agent 加新 strategy / 改参数 / 禁用某条。

---

## 5. 触发模型 / 调度

### 5.1 9 ticks 时刻表（Asia/Shanghai）

| Tick | 时间 | 主要任务 |
|---|---|---|
| **盘前** | 09:15 | 复盘隔夜消息（NewsImportance 累积）+ 集合竞价异常扫描 + 早盘观察清单 |
| **盘中 1** | 09:40 | 开盘 10min 后扫——观察是否有突破/跌破 |
| **盘中 2** | 10:10 | 30min 间隔 |
| **盘中 3** | 10:40 | 30min 间隔 |
| **盘中 4** | 11:10 | 上午最后扫 |
| **盘中 5** | 13:10 | 下午开盘 10min 后 |
| **盘中 6** | 13:40 | 30min 间隔 |
| **盘中 7** | 14:10 | 下午中段（**主动跳过 14:40 / 15:00 防尾盘 noise**）|
| **盘后** | 15:30 | 完整 review + 完整 scan + 写 Lessons + 明日观察 |

非交易日全部跳过。

### 5.2 单 tick 两阶段架构

```
Tick fires
   ↓
[Phase 1: 规则扫描] 纯代码 / 0 LLM
   For each watchlist 股票 (≤50):
     - 读 MARKET_SNAPSHOT 拿当前 quote
     - 读 KLINE_SNAPSHOT 拿最近 60 日 K
     - 读 news_tags + news_tickers 拿距离上次 tick 之间该股相关 news
     - 对 24 个 SignalKind 逐个跑 detect()
     - 收集触发的 (code, signals[]) 元组
   写 signal_detections 表（审计）
   ↓
[过滤层]
   For each (code, signals[]):
     - signals 数 < min_signals_for_mini_scan（默认 2）→ skip
     - code 在最近 30min 已触发过 2 次 → skip
     - 全局当日 LLM mini-scan ≥ 100 → skip
   ↓
[Phase 2: LLM mini-scan] 逐股
   For each (code, signals[]) passing filter:
     - 构造 mini-scan prompt:
        - identity.md (cached)
        - 该股完整上下文：quote / 60 日 K / 触发的 signals / 现有 active expectation（如有）/ 相关 active heuristics
        - 触发的 strategies 列表 + 各自 track record
     - 如果 ≥2 信号汇合或 expectation 临门 → 附 analyze_chart 结果
     - LLM 决定：create_expectation / update_expectation / cancel_expectation / no_action
     - 写 agent_episodes（trigger_kind='scan'，trigger_ref=tick_id）
```

**关键约束**：
- `min_signals_for_mini_scan = 2` 防一次性误信号触发（可配）
- 单股 30min 内最多 2 次 mini-scan
- 全局每日 100 次 LLM scan 上限（agent_episodes count）
- NewsImportance=High 资讯**绕开**这些限制立即触发（停牌 / 立案不能等）

### 5.3 Chat 触发的 mini-scan

用户在 chat 涉及自选股 → 走同一 mini-scan 入口（带 `trigger_kind='user_chat'`）。如果 chat 没涉及自选股（一般性问题）→ 标准 chat pipeline。

实现：`pipeline/chat.rs` 解析用户消息提取 code → 涉及自选股 → 调 `scan::run_mini_scan(code, ChatTrigger)` 然后把结果合并进 chat 回复。

### 5.4 Expectation Review tick

15:30 盘后那次 tick 末尾**额外**跑一遍 expectation review（也可独立调度，但合并省事）：

```rust
// pipeline/agent/expectation_review.rs
pub fn run_review(app: &AppHandle) -> Result<ReviewResult> {
    let expirable = expectation_repo::list_pending_expiring_today(app)?;
    for exp in expirable {
        let quote = market_snapshot::get(&exp.code);
        let outcome = judge_outcome(&exp, &quote);   // 纯函数判 hit/missed/expired
        // 写状态推进 + 关联 signals_used 各 +1
        expectation_repo::transition(app, &exp.id, outcome.state, outcome.reason)?;
        for sig in &exp.signals_used {
            heuristic_repo::record_signal_outcome(app, sig, &outcome.state)?;
        }
        // 自动写一条 Lesson
        let lesson = generate_lesson_from_expectation(&exp, &outcome);
        lesson_repo::create(app, &lesson)?;
        // 关联 position 自动平仓（如果 outcome=missed 且 thesis 失败语义）
        if outcome.state == ExpectationState::Missed && exp.has_open_position() {
            auto_close_position(app, &exp).await?;
        }
    }
}
```

`judge_outcome` 是**纯函数**：

```rust
fn judge_outcome(exp: &Expectation, quote: &StockQuote) -> Outcome {
    let now = OccurredAt::now();
    let price = quote.price.unwrap_or_default();

    if now >= exp.expires_at {
        // 到期：看 target 是否达到
        match (exp.direction, exp.target_price) {
            (Direction::Up, Some(target)) if price.value() >= target.value() =>
                Outcome::hit("到期前已达 up target"),
            (Direction::Down, Some(target)) if price.value() <= target.value() =>
                Outcome::hit("到期前已达 down target"),
            _ => Outcome::missed("到期未达 target"),
        }
    } else if let Some(target) = exp.target_price {
        // 未到期：看是否提前到价（盘后扫描时常态）
        match exp.direction {
            Direction::Up if price.value() >= target.value() => Outcome::hit("提前达 up target"),
            Direction::Down if price.value() <= target.value() => Outcome::hit("提前达 down target"),
            _ => Outcome::still_pending(),
        }
    } else {
        Outcome::still_pending()
    }
}
```

---

## 6. 视觉分析（chart vision）

### 6.1 渲染层
新增 `infrastructure/quotes/chart_renderer.rs`（Rust `plotters` crate）：

```rust
pub struct ChartIndicators {
    pub mas: Vec<u32>,           // [5, 20, 60]
    pub volume: bool,
    pub macd: bool,
    pub rsi: bool,
    pub bollinger: bool,
}

pub fn render_kline_png(
    code: &StockCode,
    period: KlinePeriod,
    lookback: usize,             // 60 默认
    indicators: ChartIndicators,
    annotations: Vec<ChartAnnotation>,  // 当前价 / 近期 expectation 标记
) -> Result<Vec<u8>, String>     // PNG bytes
```

输出：1200x800 PNG。15min 内缓存（同 code+period+indicators 命中）。

### 6.2 Tools

```rust
// adapters/agent_tools/charts.rs

pub struct AnalyzeChartTool { /* ... */ }
impl Tool for AnalyzeChartTool {
    fn description() -> &'static str {
        "渲染指定股票的 K 线图，返回图片块用于视觉形态分析。\
        用于识别叙事性形态（头肩顶/双底/旗形/楔形/背离/衰竭蜡烛）—— \
        算法信号已覆盖的简单指标无需调此工具。\
        看完图后请调 propose_visual_pattern 把形态读结果落地。"
    }
    fn input_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {"type": "string"},
                "period": {"type": "string", "enum": ["day", "week", "60m"], "default": "day"},
                "lookback": {"type": "integer", "default": 60},
                "with_indicators": {
                    "type": "array",
                    "items": {"type": "string", "enum": ["ma", "macd", "rsi", "boll"]}
                }
            },
            "required": ["code"]
        })
    }
    async fn execute(&self, input: Value, _ctx: &ToolContext)
        -> (Vec<ToolResultContent>, bool)
    {
        let png = chart_renderer::render_kline_png(...)?;
        let base64 = STANDARD.encode(&png);
        (vec![ToolResultContent::Image {
            mime: "image/png".into(),
            data: base64,
        }], false)
    }
}

pub struct ProposeVisualPatternTool { /* ... */ }
// schema: code, pattern (string), confidence (0-1), timeframe, notes
// execute: 写一条 signal_detections + 关联到 active expectation 或当前 mini-scan
```

### 6.3 触发规则
| 场景 | 调 `analyze_chart` | 理由 |
|---|---|---|
| 用户 chat 显式提"看图"/"形态" | ✅ | 用户驱动 |
| 用户 chat 涉及某只股票首次扫该股 | ✅ 一次 | 给 chat 视觉 context |
| Scheduled tick 该股触发 ≥2 算法信号 | ✅ | 多信号汇合值得二审 |
| Scheduled tick 0 或 1 信号 | ❌ | 不值 token |
| Expectation 即将到期 + 距 target < 3% | ✅ | "差临门一脚"形态可能给关键判断 |
| Expectation 已 hit | ❌ | 无需再看 |

每日 vision 预算 ~20 次 × 1300 token ≈ 26K token 增量。

---

## 7. News 集成

### 7.1 Tagger（pure rules，no LLM）

`infrastructure/news/tagger.rs`：news 入库后异步跑（不阻塞入库 pipeline）：

```rust
pub fn tag(news: &NewsItem, conn: &Connection) -> NewsTags {
    let body = format!("{} {}", news.title, news.content_text);
    let tickers = extract_tickers(&body, conn);      // 正则 \b[03456789]\d{5}\b + stocks 表验证
    let sectors = extract_sectors(&body, conn);      // 匹配 concepts.name
    let kind = classify_kind(&body);                 // 关键词匹配 NewsKind
    let importance = classify_importance(&body, &kind);
    NewsTags { tickers, sectors, kind, importance, tagged_at: now() }
}
```

NewsKind 关键词（种子 50 条，可扩）：
- `earnings`：业绩 / 财报 / 预增 / 预减 / 净利润
- `halt`：停牌 / 复牌 / 暂停
- `restructure`：重组 / 并购 / 资产注入 / 借壳
- `regulatory`：立案 / 处罚 / ST / 退市风险 / 监管
- `ownership`：解禁 / 减持 / 增持 / 大股东
- `operating`：中标 / 签约 / 合同 / 投产 / 新产品
- `policy`：政策 / 部委 / 文件 / 规划 / 补贴
- `sector_trend`：板块 / 行业 / 概念 / 主题
- `market`：大盘 / 指数 / 资金面 / 流动性

NewsImportance:
- `high`：停牌 / 立案 / 退市风险 / 重大资产重组 / 重大违规 / 业绩巨亏 — 盘中触发 mini-scan
- `medium`：财报 / 解禁 / 中标 / 板块政策 — 累积下个 tick
- `low`：一般市场评论 / 行业新闻 — 盘后消化

### 7.2 Scan tick 集成

```rust
fn collect_news_signals(app: &AppHandle, code: &StockCode, since: OccurredAt) -> Vec<SignalKind> {
    let news_ids = news_repo::list_for_code_since(app, code, since)?;
    news_ids.iter()
        .filter_map(|id| news_tags::get(app, id).ok())
        .map(|tags| SignalKind::NewsCatalystMatched {
            kind: tags.kind,
            importance: tags.importance,
        })
        .collect()
}
```

NewsImportance=High 单独 short-circuit 触发 mini-scan，绕过 budget。

---

## 8. 学习闭环（Lesson → Heuristic emerge）

### 8.1 Lesson 自动生成

`pipeline/agent/expectation_review.rs::generate_lesson_from_expectation`：

```rust
fn generate_lesson_from_expectation(exp: &Expectation, outcome: &Outcome) -> Lesson {
    // 由 reflection 后台 LLM 调用一次（每个 expectation 终态）
    // 输入：完整 expectation 内容 + 期间价格 / 量能 / 关键事件
    // 输出：observation (事实) + takeaway (一句话教训)
    // 调用模型：cheap channel（compact 模型）以省成本
}
```

Phase 1 简化：takeaway 让 LLM 在 reflection 末尾生成，observation 用纯代码生成（"在 X 价开仓，Y 天后 Z 价平，盈亏 N%"）。

### 8.2 Heuristic emerge 算法

每个 reflection tick 末尾跑：

```rust
fn try_emerge_heuristics(app: &AppHandle) -> Result<Vec<HeuristicId>> {
    let recent_lessons = lesson_repo::list_recent(app, 50)?;
    // 聚合：找 takeaway 文本相似度 ≥ 0.7 的 lesson 簇（embedding 或简单 token jaccard）
    let clusters = cluster_lessons_by_takeaway(&recent_lessons);
    let mut emerged = Vec::new();
    for cluster in clusters {
        if cluster.len() < 2 { continue; }           // 至少 2 条支持
        if already_has_heuristic(&cluster) { continue; } // 不重复 emerge
        let h = Heuristic {
            body: synthesize_body(&cluster),         // LLM 生成统一表述
            origin: PrincipleOrigin::AgentInferred,
            supporting_lesson_ids: cluster.iter().map(|l| l.id.clone()).collect(),
            ..Default::default()
        };
        heuristic_repo::create(app, &h)?;
        emerged.push(h.id);
    }
    Ok(emerged)
}
```

Phase 1 简化：cluster 算法用 token jaccard ≥ 0.6（不上 embedding）；2 条支持就 emerge。后续可调阈值或上 embedding。

### 8.3 confidence 派生 + prompt 注入

```rust
impl Heuristic {
    pub fn confidence(&self) -> Option<f32> {
        let total = self.hit_count + self.miss_count;
        if total < 3 { return None; }        // 样本不足
        Some(self.hit_count as f32 / total as f32)
    }
    pub fn effective_state(&self) -> EffectiveState {
        if self.retired_at.is_some() { return EffectiveState::Retired; }
        match self.confidence() {
            Some(c) if c >= 0.6 => EffectiveState::Active,
            Some(c) if c >= 0.3 => EffectiveState::Challenged,
            Some(_) => EffectiveState::Dormant,
            None => EffectiveState::Probationary,  // 样本不足，仍进 prompt 但带标记
        }
    }
}
```

`list_for_prompt` 拉 effective_state in (Active, Challenged, Probationary) + origin=user_stated/seed（这两类无条件进 prompt）。

### 8.4 Signal→Heuristic 命中率反馈

`expectation_review` 标 hit/miss 时，**signals_used 数组**每个 signal 对应的最近一条 Heuristic（按 supporting_lesson_ids 关联）+1 application_count 和 +1 hit/miss——直接闭合 Signal 实战表现到 Heuristic 的命中率。

---

## 9. 安全网

| § | 项 | 落点 |
|---|---|---|
| 9.1 | 反"自我合理化" reflection 强制结构 | reflect.rs prompt 模板 |
| 9.2 | Heuristic 防膨胀 | 自动 dormant；retired 软删；prompt 上限 25 条 |
| 9.3 | Regime 切换 | `current_regime` 接 `domain/quotes/regime::detect_regime`；切换时 active heuristics 进入 probationary |
| 9.4 | user_stated 防注水 | `heuristic_repo::record_application_outcome` 对 origin=user_stated 拒绝 hit/miss 累加 |
| 9.5 | Bull/Bear Steelman | identity.md v3 §3 强制 |
| 9.6 | Seed Heuristics | 10 条手写（同 v2 seed_principles，移到 heuristic_repo + seed_heuristics.rs） |
| 9.7 | 数字纪律 | identity.md §7 必调工具校验 |
| 9.8 | 涨跌停 = 流动性断点 | LimitUp/LimitDown signal 触发后 scan prompt 显式提示"想买买不到 / 想卖卖不出" |
| 9.9 | Budget 防爆 | scan tick 单股 30min 2 次 + 全日 100 次（NewsHigh 例外）|

### 9.10 验证：账户收益率（主）+ 机制健康度（辅）

主指标（agent 有效性 ground truth）：
- cumulative_return / max_drawdown / win_rate / avg_holding_days / expectation_hit_rate / **signal_hit_rate_table** / theme_hit_rate

辅指标（机制健康度）：
- Expectation 完整度（signals_used ≥1 + target_price 非空的比例）
- Reflection 触发率（每日 closed positions 是否都有对应 lesson）
- Heuristic 流动性（emerge 数 / 月 + 淘汰数 / 月）
- Signal coverage（24 个 signal 被触发的均匀度）
- Tick 触发率（每个 tick mini-scan 触发占比，太高烧钱、太低无效）
- Regime 切换检测正常

---

## 10. identity.md v3 改写要点

新 identity.md 大致 11 节：

1. **你是谁** — 操盘手 + 学习者用户（保留）
2. **你怎么被唤醒** — chat / 9 tick scan / expectation review tick
3. **决策框架（含 Bull/Bear Steelman）**
4. **Expectation 纪律** —— 取代旧 Thesis 纪律
5. **Strategy 纪律** —— 何时引用哪条 strategy；用户调 strategy 边界
6. **Signal 纪律** —— 24 个枚举介绍；何时用 visual chart
7. **Lesson + Heuristic 纪律** —— 学习如何沉淀；user_stated vs agent_inferred
8. **数字纪律** —— 必须调工具
9. **多图分析纪律** —— 收到多张图先逐张描述再综合；图片顺序就是用户传的顺序
10. **工具一览** —— 见 §10
11. **输出风格** —— 操盘手讲思路

---

## 11. 工具表

### 11.1 新增（v3）
| Tool | 用途 |
|---|---|
| `create_expectation(code, direction, target_price, horizon_days, reasoning, signals_used, conviction, theme?)` | 建预期 |
| `update_expectation(id, target_price?, horizon_days?, reasoning?)` | 调整未到期预期 |
| `cancel_expectation(id, reason)` | 主动取消（区别于 missed/expired）|
| `analyze_chart(code, period?, lookback?, with_indicators?)` | 渲染 K 线图返回 ImageBlock |
| `propose_visual_pattern(code, pattern, confidence, timeframe, notes)` | agent 看完图回报形态 |
| `update_strategy(id, config_changes)` | 用户对话驱动调整 strategy（agent 代写）|
| `add_to_watchlist(code, theme?)` | 加自选 + 可选 theme tag |

### 11.2 修改（v3）
| Tool | 改动 |
|---|---|
| `open_position` | `thesis_id` 字段改名 `expectation_id`；schema 设 `required: false` 但 description 强约束 agent 主动建仓必传 |

### 11.3 删除（v2 → v3）
- `create_thesis` / `update_thesis_state` / `attach_thesis_feedback`
- `propose_principle` / `confirm_principle` / `retire_principle`（前两个改名为 `record_heuristic_feedback` 给用户反馈用；retire 保留改名 `retire_heuristic`）

### 11.4 保留（v2）
- 所有 quotes / research / account read / news 类工具不变
- `close_position` / `scale_position` / `adjust_stops` 不变

---

## 12. 前端 spec

### 12.1 5 个核心 surface

| Surface | 性质 | 关键内容 |
|---|---|---|
| **Today**（默认首屏，read-only） | 主屏 | 9 ticks 进度 + 今日 scan episodes + active expectations 摘要 + 账户横条 |
| **Expectations** | read-only（取代 Theses）| 按 state 分组 + theme filter + 状态机时间线 |
| **Strategies** | read-only + 用户可在 chat 改 | 列表 + 各自 track record |
| **Positions** | read-only | 持仓关联 expectation + 主指标面板 |
| **Heuristics**（取代 Principles）| read-only | confidence / hit / miss / supporting_lessons 链接 |
| **Lessons** | read-only | 全部自动生成的教训 |
| **Chat** | **唯一输入通道** | intent / agent 自驱消息视觉区分 / 工具调用内联链接 / **10 图上限** |

### 12.2 多图增强（用户具体要求）
- 后端 `MAX_IMAGES_PER_MESSAGE = 10`
- 前端 `maxImagesPerMessage = 10`
- 单张大小上限 8MB 不变
- **UX**：批量拖拽 / 粘贴一次性入队；缩略图预览；单张可删除；计数 `N/10` 显示
- **agent 多图纪律**写入 identity.md §9

### 12.3 Watchlist 上限
- 后端 `MAX_WATCHLIST_SIZE = 50`，超出 add 返回 err
- 前端 SimulationPage 显示 `N/50`

### 12.4 "问 agent" context-aware 跳转
- 各 surface 加按钮，预填 chat 上下文跳转
- chat 端 `agent-prefill` event 已存在，复用

### 12.5 Tauri commands + events
- 新增 commands：`list_expectations / get_expectation / list_expectation_events / list_strategies / list_lessons / list_heuristics / list_signal_detections / get_health_metrics / get_account_metrics / trigger_scan_now`
- 新增 events：`expectations-changed / strategies-changed / heuristics-changed / lessons-changed / scan-tick-completed`

---

## 13. 依赖与边界（DDD 4 方向自检）

- `Expectation` 归 **account BC**（持仓的预测；agent 是消费者）
- `Strategy / Lesson / Heuristic / Signal / signal_detector / chart_renderer / news tagger` 归 **agent BC**（用 quotes 数据但概念归 agent）
- 跨 BC 唯一引用：`SimulatedPosition.current_expectation_id: Option<ExpectationId>`（已有同形态先例）
- 现有 4 方向 grep 自检全部保持绿

---

## 14. 迁移路径（v2 → v3）

`SCHEMA_VERSION = 3`，启动时：
1. 现存 schema_meta.version < 3 → 备份 DB 为 `.legacy-v2-{ts}`
2. 建 v3 schema（全新 expectations/strategies/lessons/heuristics/news_tags/news_tickers/signal_detections + 改名 simulated_positions.thesis_id → current_expectation_id）
3. seed_heuristics 注入 10 条原 seed principle
4. seed_strategies 注入 3 条默认 strategy

**不做** v2 → v3 数据迁移。设计 spec 早期阶段不背包袱。

---

## 15. Phase 1 工作分解

按依赖顺序：

```
Week 1：domain + DB schema
  ☐ domain/account/expectation.rs (Expectation aggregate + 状态机)
  ☐ domain/agent/{strategy,signal,lesson,heuristic}.rs
  ☐ 删除 domain/account/thesis.rs
  ☐ migrations.rs schema v3：所有新表 + 删旧表 + position 字段改名
  ☐ connection.rs SCHEMA_VERSION = 3
  ☐ infrastructure repositories (expectation_repo / strategy_repo / lesson_repo / heuristic_repo / signal_detection_repo)
  ☐ infrastructure/news/tagger.rs
  ☐ infrastructure/quotes/signal_detector.rs (24 个枚举的纯代码检测)
  ☐ infrastructure/quotes/chart_renderer.rs (plotters PNG 渲染)
  ☐ infrastructure/agent/seed_heuristics.rs + seed_strategies.rs
  ☐ cargo check + 单元测试

Week 2：pipeline + adapters
  ☐ pipeline/agent/scan.rs (单 tick 两阶段主流程)
  ☐ pipeline/agent/expectation_review.rs (judge_outcome + lesson 生成 + 自动平仓)
  ☐ pipeline/agent/reflect.rs 重构 (expectation-based)
  ☐ pipeline/agent/heuristic_emerge.rs (cluster + emerge 算法)
  ☐ adapters/scan_scheduler.rs (9 ticks 调度)
  ☐ adapters/agent_tools：新增 expectation/strategy/chart/visual_pattern 工具；删 thesis / principle 工具
  ☐ pipeline/chat.rs：chat 涉及自选股调 mini-scan
  ☐ pipeline/news/refresh.rs 集成 tagger
  ☐ NewsImportance=High short-circuit 触发逻辑

Week 3：identity + 安全网
  ☐ identity.md v3 重写
  ☐ Reflection 强制对照 prompt segment
  ☐ Regime 接入：scan_tick 调 detect_regime 注入 prompt + heuristic filter
  ☐ 主指标 + 辅指标派生函数 (account/metrics.rs + agent/health_metrics.rs)
  ☐ Budget 防爆代码（每股 30min 2 次 / 全局 100 次/天）
  ☐ docs/architecture.md 同步 v3
  ☐ 删 docs/design/agent-redesign.md（替换为 v3 的引用）

Week 4：前端
  ☐ Tauri commands：list_expectations / list_strategies / list_lessons / list_heuristics / get_signal_detections / get_account_metrics / get_health_metrics / trigger_scan_now
  ☐ 4 个 events emit
  ☐ ExpectationsPage（取代 ThesesPage）
  ☐ StrategiesPage / LessonsPage / HeuristicsPage（取代 PrinciplesPage）
  ☐ TodayPage 改造：episode 时间线 + 今日 scan 进度 + active expectations 摘要
  ☐ ChatPage：多图 4→10 + UX 批量入队 + 缩略图预览 + 单张可删
  ☐ SimulationPage：position 关联 expectation；主指标面板；watchlist 50 上限
  ☐ "问 agent" 跳转按钮接通
  ☐ 默认首屏 → today

Week 5：联调 + 验证
  ☐ cargo check 0 warnings
  ☐ cargo test 全绿（含新 signal_detector 测试 / chart_renderer 测试 / expectation_review.judge_outcome 纯函数测试）
  ☐ DDD 4 方向自检
  ☐ npm run build
  ☐ Tmux dev 启动 + 手动跑：用户 chat → 触发 mini-scan → 建 expectation → 模拟时间快进 → review → lesson → heuristic emerge → 闭环跑通
```

---

## 16. Phase 2 推迟项

- 双层 reflection（低层市场认知 + 高层决策反思）
- Episodic memory + embedding 向量召回
- Multi-mode pipeline 拆 observe/decide/reflect 各自独立
- Intent tagging 7 类完整版（当前简化 3 类够用）
- Heuristic cluster 算法上 embedding（替代 token jaccard）
- Strategy 自动调权重（让 hit rate 自动影响 strategy.enabled）
- Backtest harness（用真实历史数据回放）
- 行业 / 主题相关性图谱（"哪些股是该主题的核心标的"）

---

## 17. 不做的事

- ❌ Multi-agent / sub-agent
- ❌ 真实券商接入
- ❌ Trajectory compression for fine-tuning
- ❌ Messaging gateway（Telegram / Slack）
- ❌ 旧 Thesis 概念以任何形式保留
- ❌ 旧 Principle 字面保留（改名 Heuristic + 语义升级）
- ❌ proposed → active 二态状态机（confidence 连续派生取代）
- ❌ 文件附件上传（只多图）
- ❌ pre-15:30 close 时段触发 tick（避开尾盘 noise）

---

## 18. 已知风险

| 风险 | 缓解 |
|---|---|
| LLM 视觉读图 hallucinate 形态 | propose_visual_pattern 必填 confidence；持续追踪 visual signal 命中率，hallucinate 会自然降权 |
| Heuristic cluster 算法粗糙（token jaccard） | Phase 1 接受；积累足够 lessons 后 Phase 2 上 embedding |
| 9 ticks 高频在熊市 / 横盘可能成本不划算 | tick 内部信号触发率低就自动少跑 mini-scan；budget cap 兜底 |
| 视觉图缓存 15min 在快变行情可能误用 | 缓存 key 包含 quote_time 分钟级精度（不只是分钟级日期）|
| Strategy track record 噪音 | applied_count < 10 不显示 confidence；用户 chat 改 strategy 时强制说明理由（写进 strategy.notes_log） |
| 用户改 strategy 改坏了 | 每次 strategy 修改写一条 strategy_events；可回退 |
| NewsImportance 分级关键词漏报 / 误报 | tagger 规则可热改；importance=high 列表保守起步 |
| Lesson 自动生成噪音多 | takeaway 由 LLM 写但 observation 由代码生成保证客观；后续可加用户标"无效"按钮过滤 |
| Expectation 滚动 supersedes 链可能无限 | Phase 1 不限；UI 显示时仅展开最近 5 层 |

---

## 19. 关键设计决定（FAQ）

**Q: Expectation 单只股票同时几个 active？**
A: 1 个。同方向更新走 supersedes 链；反方向不允许并存（必须先 cancel 旧的）。避免同时押矛盾的两边。

**Q: target_price 单点 vs 区间？**
A: 双向都支持。Phase 1 默认单点（涨到 X / 跌到 Y）；区间用 `target_price + target_price_ceiling` 双字段。

**Q: horizon_days 用交易日还是日历日？**
A: 交易日。`expires_at = trade_calendar.add_business_days(created_at, horizon_days)`。

**Q: Strategy 命中率统计窗口？**
A: 全部历史 + 最近 30 天双重展示。"全部"反映 strategy 长期表现，"30 天"反映当前 regime 适用性。

**Q: 用户改 Strategy 时 agent 角色？**
A: agent 是用户的"配置助手"——用户口头说"把动量突破策略的量比阈值从 1.5 改到 2.0"，agent 调 `update_strategy` 工具修改。每次改写一条 strategy_events 记录 reason。

**Q: 视觉 LLM 选哪个模型？**
A: 跟 chat 主 channel 一样（Anthropic Claude 4.7 multimodal / OpenAI GPT-5 / DeepSeek-VL 等）。不单独配置。

**Q: heuristic seed 10 条进 prompt 跟 agent_inferred 怎么共存？**
A: 共存。`list_for_prompt` 按规则取：seed(永远进) + user_stated(永远进) + agent_inferred(按 confidence 过滤) + Probationary(样本不足但进 prompt 带标记)。总数上限 25 条。

**Q: Lesson 谁写？LLM 还是代码？**
A: 混合。observation 字段由代码生成（"在 X 价开仓 Y 天后 Z 价平 盈亏 N%"）；takeaway 由 reflection 时 LLM 写（"ST 板块涨停日的回踩通常是诱多"）。Phase 1 简化：两者都由 LLM 在 reflect.rs 末尾生成。

**Q: 自动平仓什么时候触发？**
A: expectation 转 missed/expired 且关联 position 仍 open 时，**仅当 expectation 是 agent 主动建仓那条**自动调 close_position。用户命令开仓的不动（用户自己负责）。

---

## 20. 验收标准

跑通以下端到端流程才算 Phase 1 done：

1. ✅ 用户加 5 只股到 watchlist
2. ✅ 09:40 tick 自动触发；对 5 只股各跑 24 信号检测
3. ✅ 某只股满足 Strategy A 的 trigger_when → LLM mini-scan → 建 expectation
4. ✅ 前端 ExpectationsPage 立即出现这条 expectation（events 推送生效）
5. ✅ 同股票 30min 内不再次触发
6. ✅ 用户在 chat 说"看下 600519 的图" → 用 chat-driven mini-scan + 调 analyze_chart → 看到图 → agent 在 chat 里回报形态
7. ✅ 模拟时间快进到 horizon expires → 15:30 review tick 跑 → 判 missed → 自动平仓 + 写 Lesson + signals_used 各 +1 miss
8. ✅ 重复 2-3 次类似 lesson 累积 → reflection 末尾 emerge 一条 Heuristic（agent_inferred）
9. ✅ 下次 chat 该 Heuristic 进 prompt（confidence > 0.6 或仍 probationary）
10. ✅ 用户在 chat 说"撤掉这条原则" → agent 调 retire_heuristic
11. ✅ NewsImportance=High 资讯入库（停牌） → 立即触发 mini-scan
12. ✅ DDD 4 方向自检全绿
13. ✅ cargo check + cargo test + npm run build 全绿

---

完整设计闭环。准备开工。
