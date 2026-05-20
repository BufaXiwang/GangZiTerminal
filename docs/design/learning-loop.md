# Agent 学习闭环

> 这是当前在用的 agent 学习设计基线。所有概念以本文档为准。
>
> 历史演化（v1 Thesis → v2 Principle → v3 Expectation-driven → 当前合并版）见 git history。

---

## 关键概念

读后续章节前必懂的 6 个。

### Position（持仓）
**一肩挑「执行 + 假设」**。
- **执行字段**：code / shares / avg_entry_price / status
- **假设字段**（合并自旧 Expectation 聚合）：direction / take_profit / stop_loss / time_stop_at / invalidation_signals / signals_used / reasoning
- **PositionKind**：`Live`（真持仓，shares > 0，扣现金）/ `Watch`（"看好但不下注"，shares = 0，盘外可建）

### Signal（信号）
**触发 Position 开仓 / 平仓 / 调整的原子条件**。
- 规则信号：代码自动检测（trend / oscillator / volume / capital / sector / factor 6 类，~24 个 family）
- 视觉信号：LLM 看 K 线图识别（`VisualPatternRead`，头肩顶 / 双底 / 缺口等）
- 资讯信号：news tagger + agent 解读（6 个 family：政策利好/利空、业绩超预期/低于预期、板块利好、公司事件）

### Lesson（教训）
**每次 Position close 自动生成的原子观察**。"在 ST 板块涨停日追了，被诱多套住 7%"。
- 永不修改、永不删除
- 是 Heuristic emerge 的原料
- `outcome` 派生自 close_reason + PnL：`Hit` / `PartialHit` / `Miss` / `Expired`（中性）

### Heuristic（启发式规则）
**积累 ≥N 条共有模式的 Lessons 之后浮现的可重用规则**。带 `body / supporting_lesson_ids / application_count / hit_count / miss_count / confidence`。
- 三种 origin：`seed`（启动种 10 条经典）/ `user_stated`（用户口头规则）/ `agent_inferred`（emerge 出来）
- confidence 现算：`hit / (hit + miss)`，不存
- 注入 prompt：active + probationary 状态 + 按 regime 过滤 + confidence × application_count 降序 top-25

### Strategy（策略）
**一组"什么时候建 Position"的规则**。用户 + agent 共建。例："放量突破 20MA + 板块强势 + 北向净流入 → 建 position 目标 +5% 8 天"。
- 多个 Strategy 可并存，各自跟踪命中率
- scan tick 时按 Strategy.trigger_when 命中筛选

### Tick（扫描节拍）
**自驱观察循环的最小单位**。9 ticks/天 + 事件触发 + 定时 news review：

| Tick | 触发 | 干什么 |
|---|---|---|
| `scan_tick` × 9 | 9 个固定时刻 | ① 规则信号检测（纯代码）② 命中时 LLM mini-scan + 跑 auto_review |
| `news_review` (buffer ≥ M=20 OR 每 N=15min) | 攒批或定时（M/N 在 SettingsPage 可调） | 喂全量近期 news + 上下文给 agent |
| `close_reflection` | 每日 15:30 | heuristic emerge + lesson takeaway 填充 |

---

## 数据流：闭环全景

```
┌────────────────────────────────────────────────────────────────────┐
│  Strategy 层（用户 + agent 共建的规则集，可热改）                    │
│    trigger_when[]:SignalCondition  target:TargetRule  track ↻       │
└────────────────────────────────────────────────────────────────────┘
                  ↓ 应用
┌────────────────────────────────────────────────────────────────────┐
│  Tick 调度                                                          │
│    9 scan_tick + news_review (buffer M=20 / 每 N=15min)             │
│                                                                     │
│  阶段 1：规则信号扫描（纯代码，0 LLM）                                │
│           触发？─Y─> 阶段 2                                         │
│                  ↓                                                  │
│  阶段 2：LLM mini-scan / news_review                                 │
│           看 Strategy + heuristics + news → open_position(kind=...)  │
└────────────────────────────────────────────────────────────────────┘
                  ↓
┌────────────────────────────────────────────────────────────────────┐
│  Position（核心实体）                                                │
│    Live：shares + avg_cost + 假设字段                                │
│    Watch：shares=0 + 假设字段（盘外可建）                            │
│    判定字段：take_profit / stop_loss / time_stop_at /                │
│              invalidation_signals / direction                       │
└────────────────────────────────────────────────────────────────────┘
                  ↓ scan_tick 末尾跑 auto_review
┌────────────────────────────────────────────────────────────────────┐
│  Auto Review                                                        │
│    纯代码 judge_position 扫所有 open positions                       │
│    命中触发条件 → 不直接 close，触发 agent review run                │
└────────────────────────────────────────────────────────────────────┘
                  ↓
┌────────────────────────────────────────────────────────────────────┐
│  Agent position_review run                                          │
│    prompt: position + 触发原因 + 近期 news + 板块 + heuristics       │
│    工具: close_position / adjust_position / acknowledge_no_action    │
└────────────────────────────────────────────────────────────────────┘
                  ↓ 真 close 时
┌────────────────────────────────────────────────────────────────────┐
│  Lesson 生成                                                        │
│    observation: "在 X 价 N 天 close(reason)，PnL Y%"                 │
│    outcome: Hit / PartialHit / Miss / Expired （派生）              │
│    signals_in_play: position.signals_used 复制                       │
│    takeaway: 留空（reflection 时 LLM 填）                            │
└────────────────────────────────────────────────────────────────────┘
                  ↓
┌────────────────────────────────────────────────────────────────────┐
│  Heuristic 反向打标                                                  │
│    主路径：position_heuristic_links 表精确归因                       │
│    回落：signals_used family 与 supporting lessons 交集              │
│    应用：hit_count / miss_count / application_count                  │
└────────────────────────────────────────────────────────────────────┘
                  ↓
┌────────────────────────────────────────────────────────────────────┐
│  Heuristic emerge（reflection 15:30）                               │
│    扫最近 7 天 lessons → 按 signal_family 聚类 → ≥2 条同向自动       │
│    propose_heuristic(origin=agent_inferred)                         │
│                                                                     │
│  下一轮 chat / scan_tick 自然带上新认知（按 confidence top-N）        │
└────────────────────────────────────────────────────────────────────┘
```

---

## Position 详细

### 字段
```rust
pub struct Position {
    pub id: PositionId,
    pub code: StockCode,
    pub name: String,
    pub kind: PositionKind,                      // Live / Watch
    pub avg_entry_price: Yuan,
    pub current_shares: Shares,                  // Watch 恒为 0
    pub status: PositionStatus,                  // Open / Closed{exit_price, exit_at, reason}

    // 触发条件（auto_review 检查）
    pub stop_loss: Option<Yuan>,
    pub take_profit: Option<Yuan>,
    pub time_stop_at: Option<OccurredAt>,
    pub invalidation_signals: Vec<SignalKind>,

    // 假设字段
    pub direction: Direction,                    // Up / Down
    pub signals_used: Vec<SignalKind>,           // 入场归因
    pub reasoning: String,                       // 自然语言（无字数限制）

    // 审计
    pub source_analysis_id: String,
    pub entered_at: OccurredAt,
    pub last_acquisition_at: OccurredAt,
}
```

### PositionKind 行为对照

| 方面 | Live | Watch |
|---|---|---|
| shares | 100 整数倍 | 强制 0 |
| 现金 | 扣 cost + commission | 不动 |
| valuation 参与 | 是 | 不算市值（shares=0） |
| 交易时段规则 | 仅 9:30-15:00 + 集合竞价（未来扩展） | 任何时段 |
| T+1 规则 | 适用 | 不适用 |
| 涨跌停 / 盘口可成交 | 适用 | 不适用 |
| scale_position | 允许 | 拒绝（想转 live 请先 close 再开） |
| auto_review judge | 同 Live | 同 Live（共享同一纯函数） |
| Lesson 生成 | close 时 | close 时（pnl_pct=None） |

### CloseReason 与 Lesson outcome 派生
| CloseReason | 触发 | LessonOutcome | Heuristic 计 |
|---|---|---|---|
| `TakeProfit` | current_price ≥ take_profit | `Hit` | hit +1 |
| `StopLoss` | current_price ≤ stop_loss | `Miss` | miss +1 |
| `Invalidated` | 任一 invalidation_signal 在 signal_detections 命中 | `Miss` | miss +1 |
| `TimeStop` | now ≥ time_stop_at + PnL 反向 | `Miss` | miss +1 |
| `TimeStop` | now ≥ time_stop_at + 方向对未达 target | `PartialHit` | 中性（不计 hit/miss） |
| `TimeStop` | now ≥ time_stop_at + 无 take_profit 目标 | `Expired` | 中性 |
| `Manual` | agent 主观撤回 | 不写 Lesson | 不计 |

---

## Auto Review 详细

```rust
// pipeline/agent/auto_review.rs

// 接到 scan_scheduler 9 tick 末尾
pub async fn run(app: &AppHandle, episode_id: Option<String>) -> Result<ReviewResult, String> {
    let triggers = find_triggers(app)?;        // 纯代码扫
    for trigger in triggers {
        run_review_for_position(app, trigger).await;
    }
}

fn find_triggers(app: &AppHandle) -> Result<Vec<ReviewTrigger>, String> {
    // 对每个 open position：
    //   1. 拿 current_price 从 MARKET_SNAPSHOT
    //   2. 查 signal_detections.since(position.entered_at)
    //   3. judge_position 纯函数判定
    //   4. ShouldClose → 加入 triggers，附带 trigger_reason + note
}

async fn run_review_for_position(app: &AppHandle, trigger: ReviewTrigger) {
    // 启动一个 agent mini-scan run（trigger_kind = "position_review"）
    // prompt 含：
    //   - position 完整字段
    //   - 触发原因 + 当前价 + 距 entered_at 时间
    //   - 近 24h 相关 news（按 ticker / sector 过滤）
    //   - 近期同 code 的 lessons
    //   - active heuristics top-N
    //   - 市场状态 + 板块涨跌
    // agent 工具白名单：
    //   - close_position（同意触发）
    //   - adjust_position（移止损 / 加 horizon / 改 invalidation）
    //   - acknowledge_review_no_action（继续持，写 PositionEvent::Reviewed）
}
```

**关键设计：不直接 close**——纯代码识别"可能该 close"，但是否真 close 由 agent 决定。插针、跳空回弹、系统性下跌等场景，agent 看上下文比规则更准。

---

## Agent News 分析详细

> News BC 只提供数据（[news-module.md](./news-module.md)）；分析逻辑全部属于 Agent。
> 状态机表 `agent_news_analysis_state` 在 agent BC，News 模块不感知本节内容。

### 两档触发
| 触发器 | 来源 | 用途 |
|---|---|---|
| `news_batch` 定时 | adapters/news_batch_scheduler tokio timer | 每 N=15 分钟兜底（默认，SettingsPage 可调 1–60 min）|
| `news_batch` buffer overflow | adapter 监听 News BC 发的 `news-refreshed` event；pending ≥ M=20 时触发 | 突发某板块多消息汇集 |

**不筛 importance**——agent 自己判断哪些是 actionable（原 tagger 关键词分级已删，规则不准）。

### 状态机（agent_news_analysis_state 表）

```
   pending     ──claim_batch(M)──►   processing
   processing  ──mark_consumed────►  consumed     (agent run 成功)
   processing  ──revert──────────►   pending      (agent run 失败 → 等下次重试)
   processing  ──watchdog 30min──►   pending      (孤儿回收)
   processing  ──mark_failed─────►   failed       (预留：agent 显式拒绝)
```

LEFT JOIN news_items 找出"还没注册过 state 或仍 pending"的 news——这样 News 表入库后，agent 自己在下一次 tick 注册到自己的 state 表里，News 模块不感知。

### 并发安全
- `news_analysis_repo::claim_batch` 用 `INSERT ... SELECT LEFT JOIN ... ON CONFLICT DO UPDATE RETURNING` 单语句原子取走 + 标 processing
- 进程级 `REVIEW_IN_FLIGHT` AtomicBool（在 `pipeline/agent/news_batch.rs`）防多 tick 撞——run_once 一开始 `try_mark`，上一轮没结束就 skip 本轮
- RAII Drop guard 保证成功 / 失败 / panic 都释放锁
- 30min watchdog 兜底回收 processing 孤儿（进程崩 / 进入死循环的极端情况）
- 失败一律 `revert_processing_to_pending`（不写 failed，防瞬时故障永久漏分析）

### prompt 喂的数据
| 段 | 内容 |
|---|---|
| 触发上下文 | "本次触发：buffer 满 / 定时；本批 N 条；队列剩余 X 条" |
| 新增 news | 本批 news 全量（title + summary + source + published）|
| 持仓 | open positions（含 live + watch）|
| 自选股 | 当前 watchlist |
| 当前 heuristics | top-N by confidence + regime match |

### agent 任务
1. 识别有 actionable insight 的 news（不是所有都值得操作）
2. 对相关 watchlist / open positions 评估影响
3. 决定 `open_position(kind=live/watch)` / `adjust_position` / `add_to_watchlist` / `no_action`

输出：一段总结 + 0-N 个工具调用。

### 文件结构

```
domain/agent/news_analysis.rs        NewsAnalysisStatus enum
infrastructure/agent/news_analysis_repo.rs
                                     claim_batch / mark_consumed / mark_failed /
                                     revert_* / reclaim_stale / count_pending
pipeline/agent/news_batch.rs         REVIEW_IN_FLIGHT 锁 + run_once + maybe_buffer_overflow
pipeline/agent/news_review.rs        构 prompt + 跑 agent run
adapters/news_batch_scheduler.rs     timer loop + news-refreshed listener
```

---

## SignalKind 全 family 列表

按类别（详见 `domain/shared/signal.rs`）：

| 类别 | family 例 |
|---|---|
| **trend** | BreakoutAbove20MA / BreakoutBelow20MA / MACrossUp / MACrossDown / TrendlineBreak |
| **oscillator** | RSIOverbought / RSIOversold / BollingerBreakUpper / BollingerBreakLower / KDJDeath |
| **volume** | VolumeSpike / VolumeShrink / LimitUp / LimitDown |
| **capital** | OnDragonTigerList / NorthInflowStreak / NorthOutflowStreak / MarginExpand |
| **sector** | SectorStrengthAbove / SectorWeaknessBelow |
| **factor** | PESampleLow / TurnoverRateHigh |
| **visual** | VisualPatternRead { pattern, confidence } |

> 资讯不再通过 `SignalKind` 表达——agent 看 news 直接做出 open / adjust 决策，
> 自己写 reasoning 关联回 lesson。news → signal 的强类型映射被证明过度设计已删。

---

## Lesson + Heuristic 详细

### Lesson 表
```sql
create table lessons (
    id text primary key,
    position_id text not null,           -- v4 外键（旧 expectation_id 已并入 position）
    code text not null,
    observation text not null,           -- 代码生成的客观事实
    takeaway text not null,              -- LLM 填（reflection Phase 2），可空
    outcome text not null,               -- hit / partial_hit / miss / expired
    regime_at_close text,
    signals_in_play text,                -- JSON Vec<SignalKind>
    pnl_pct real,                        -- Watch 时为 null
    source_episode_id text,
    created_at text not null
);
```

### Heuristic 表 + 应用流程
```sql
create table heuristics (
    id text primary key,
    body text not null,                  -- 一句话规则（"高换手率板块易回调"）
    category text not null,              -- principle / known_bias / risk_preference
    origin text not null,                -- seed / user_stated / agent_inferred
    regime_tags text,                    -- JSON Vec<Regime>
    supporting_lesson_ids text,          -- JSON Vec<LessonId>
    application_count integer default 0,
    hit_count integer default 0,
    miss_count integer default 0,
    last_applied_at text,
    last_emerged_at text,
    retired_at text,
    retired_reason text,
    created_at text not null
);

create table position_heuristic_links (
    position_id text not null,
    heuristic_id text not null,
    primary key (position_id, heuristic_id)
);
```

### 反向打标（auto_review close 时）
```rust
fn record_signal_outcomes(app, position, outcome_hit) {
    // 主路径：position_heuristic_links 精确归因（agent 在 open_position 时声明的）
    let linked = position_heuristic_link_repo::list_for_position(app, &position.id)?;
    if !linked.is_empty() {
        for hid in linked {
            heuristic_repo::record_application_outcome(app, hid, outcome_hit, now);
        }
        return;
    }
    // 回落：signals_used family ∩ supporting_lessons.signals_in_play family
    // 让 agent 早期不必填 applied_heuristic_ids 也能进入 track record
}
```

### emerge（reflection 15:30）
```rust
// pipeline/agent/heuristic_emerge.rs
fn emerge() {
    // 1. 拉最近 7 天 lessons（已有 takeaway 的）
    // 2. 按 (signal_family_set, regime, outcome) tuple 聚类
    // 3. 每类 ≥2 条 → 调 propose_heuristic(origin=agent_inferred) 写一条
    //    body 由 takeaway 文本聚合，supporting_lesson_ids 链回
    // 4. 重复 emerge 则更新 last_emerged_at（用于 UI "本周新增"标签）
}
```

### 衰减 / retire
| 触发 | 状态变化 |
|---|---|
| 连续 3 次 miss | active → challenged |
| 连续 5 次 miss | challenged → probationary |
| 长期未应用（30 天）| → dormant |
| 用户显式撤 / 与新规则冲突 | → retired |

`effective_state ∈ {active, probationary}` 才注入 prompt。retired 不删，留审计。

---

## Strategy DSL

```rust
pub struct Strategy {
    pub id: StrategyId,
    pub name: String,
    pub description: String,
    pub trigger_when: Vec<SignalCondition>,   // signal family 数组
    pub trigger_logic: TriggerLogic,          // And / Or
    pub target: TargetRule,                    // direction + pct_relative_to_current + horizon_days
    pub enabled: bool,
    pub applied_count: u32,
    pub hit_count: u32,
    pub miss_count: u32,
    pub created_at: OccurredAt,
    pub updated_at: OccurredAt,
}
```

scan_tick 流程：
1. 对每只 watchlist + position 股，跑 signal_detector 拿当前 SignalKind 集合
2. 按 enabled strategy 的 trigger_when + trigger_logic 匹配
3. 命中 → 触发 mini-scan agent run（LLM 看 strategy + heuristics 决定真 open）
4. 真 open 后 → strategy.applied_count++
5. Position close 时 → 反向给关联 strategy 计 hit/miss

---

## Agent 工具表

| 类别 | 工具 | 备注 |
|---|---|---|
| 读 | `get_account` / `get_position` | snapshot |
| 读 | `get_quote` / `get_market_overview` / `get_indicators` / `get_kline` | snapshot |
| 读 | `scan_market` / `get_top_list` / `get_moneyflow` / `get_concept_performance` / `get_company_events` | async fetch |
| 读 | `search_news` | DB 读 |
| 写 | `open_position(kind, direction, take_profit?, stop_loss?, invalidation_signals, signals_used, reasoning, applied_heuristic_ids?, ...)` | mutation |
| 写 | `close_position(id, reason, note)` | mutation |
| 写 | `scale_position(id, shares_delta)` | mutation（仅 Live） |
| 写 | `adjust_position(id, take_profit?, stop_loss?, time_stop_at?, ...)` | mutation |
| 写 | `acknowledge_review_no_action(id, reasoning)` | position_review 专用——选择继续持有 |
| 写 | `propose_heuristic` / `apply_heuristic` / `retire_heuristic` | mutation |
| 写 | `create_strategy` / `enable_strategy` / `disable_strategy` | mutation |
| 视觉 | `analyze_chart` / `propose_visual_pattern` | render → vision LLM → SignalKind |
| Sub agent | `delegate(type=researcher/bear_advocate, task)` | LLM-in-LLM |
| 工程 | `compact_now` | 主动压缩历史 |

---

## 与 architecture.md 的关系

- 模块边界 / 依赖方向 / DDD 7 步法 → architecture.md § 1
- BC 间接口契约 → architecture.md § 2
- 实体清单 + 持久化映射 → architecture.md § 3
- 完整数据库 schema → `src-tauri/src/infrastructure/db/migrations.rs::SCHEMA_SQL`
- 当前文档：学习闭环 + agent 行为协议
