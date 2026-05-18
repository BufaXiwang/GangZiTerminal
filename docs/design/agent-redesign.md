# Agent 重构最终方案 (v1)

> **状态**：审定后即落地。本方案吸收两轮 sub-agent review（结构 review + 业界对标 review）的修订。v0 草稿在 git 历史中。
> **落地形态**：稳定后**直接替换** `src-tauri/src/pipeline/agent/identity.md` + `docs/architecture.md` § 2.4 / § 3 / § 4。不保留旧版本。
> **范围**：Agent 模块整体 + Account 模块（thesis 升一等聚合根归属 account）+ Chat 模块角色重定义。Quotes / News 不动。
> **分两阶段**：Phase 1 (2-3 周可落) 闭合最小学习循环；Phase 2 等数据沉淀后再加扩展能力。

---

## 关键概念：Thesis 是什么

Thesis（**投资论点**）= 一笔交易的"为什么"被结构化、可证伪、可独立于持仓存在的实体。不是一段理由文字，是一个完整的可证伪假设包，包含 4 件事：

1. **hypothesis（核心论点）**：你赌的是什么发生
2. **invalidation（失效条件）**：什么事发生就证明论点错了，必须撤
3. **validation_checks（验证指标）**：盯哪些指标能确认论点在兑现
4. **conviction（置信度）**：Low / Medium / High

**例子**：

```
Thesis #42: 光模块算力需求兑现，龙头估值未反映

hypothesis:
  AI 算力建设进入兑现期，800G/1.6T 光模块龙头业绩拐点，
  当前 PE 40x 还没反映明年订单可见度。

invalidation:（任一触发就撤）
  - 中际旭创/新易盛月度出货环比连续 2 个月负增长
  - 板块 PE 突破 80x（估值已反映完毕）
  - 微软/Meta 资本开支指引下修

validation_checks:（盯这些确认论点在兑现）
  - 月度出货数据同比 > 50%
  - 中际旭创单季营收增长 > 40%
  - 板块成交额占比连续 3 周上升
  - 北向资金对该板块净买入

conviction: Medium
target_codes: [300308 中际旭创, 300502 新易盛]
state: active
```

**Thesis 升一等聚合根（相对于当前埋在 opened-event payload 的 `thesis: String`）带来 5 个核心能力**：

1. **可以无持仓存在**——"在跟踪但没建仓"是合法状态，当前模型表达不了
2. **可以对应多只股票**——同一逻辑分仓多只，1 thesis → N positions
3. **可以独立于持仓被证伪**——invalidation 触发 → thesis `invalidated` → agent 自动平所有关联持仓
4. **是 reflection 的核心对象**——复盘"光模块算力论点错在哪"而不是"为什么平掉 300308"
5. **状态机可查询**——`drafted → active → validated / drifted / invalidated / abandoned`，"想法现在还成立吗"是 SQL 可查事实

**为什么这是学习闭环的前提**：没有结构化的"为什么"，reflection 没东西可反思；agent 只能事后自由叙事"市场非理性"。把 thesis 从文本提升为实体，是 agent 可学习的最小前置条件。

---

## 0. 现状诊断

当前 agent 实现的根本结构问题，按严重度：

1. **没有自驱动**——agent 只在用户消息到达时工作。architecture.md § 1.1 写的"Agent 自驱动"是口号没兑现。
2. **没有学习回路**——`PositionEvent` 链积累了所有"决策→结果"原料，但**没有任何机制读回去**。close 之后 agent 永远不知道当初为什么开、是不是被证伪了。
3. **Thesis 埋在 opened-event payload 里**——无法表达"在跟踪但没建仓"、"一个假设分散到多只股票"、"假设证伪但仓位还没平"这些天然形态。
4. **Memory 是 8 个 flat list**——整段塞 prompt，检索不到具体经验，写进去取不回来。"自迭代"没有载体。
5. **agent_runs 只记 raw run**——没有 trigger → thesis → action → outcome 的链，复盘跑不动。
6. **User input 混进 InvestorMemory**——交易偏好、决策反馈、交互风格混进同一个 8-list，三种本质不同的影响没分流。

---

## 1. 目标形态：四循环

```
┌─────────────────────────────────────────────────────────────────┐
│                                                                 │
│  Observation loop  ←── 时钟 + 行情/资讯/账户事件 ──→ [Phase 2]    │
│       │                                                         │
│       │ 识别"值得思考的事件"，生成或激活 Thesis                    │
│       ↓                                                         │
│  Decision loop     ←── 用户指令 / 手动触发 ──→  [Phase 1 保留]   │
│       │                                                         │
│       │ Thesis + 持仓 + 召回经验 → action                        │
│       ↓                                                         │
│  Learning loop     ←── 收盘 tick / close_position ──→ [Phase 1]  │
│       │                                                         │
│       │ 对照 thesis invalidation/validation 归因                 │
│       │ 写 principle (proposed)；hit 后升 active                 │
│       ↓                                                         │
│  Interaction loop  ←── 用户 chat 消息 ──→  [Phase 1 保留]        │
│           回答 / 偏好吸收 / 反馈记录 / 手动触发                    │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

四个循环共享一个 agent identity（操盘手人格）。**Phase 1 不拆 pipeline**，chat-only pipeline 内部按触发源切 prompt segment 即可；Phase 2 数据沉淀后再拆 `observe.rs / decide.rs / reflect.rs`。

---

## 2. 七个设计抉择（最终决定）

| # | 抉择 | 决定 | Phase |
|---|---|---|---|
| (a) | Thesis 升一等聚合根，归 **account BC** | ✅ | Phase 1 |
| (b) | Memory 拆层：Identity + Principles（带 regime_tags / hit_count / state machine） | ✅（不含 episodic）| Phase 1 |
| (b') | Episodic memory（vector 召回 + decay） | ⏸ | Phase 2 |
| (c) | 单 identity 多 mode（pipeline 拆分） | ⏸（Phase 1 chat-only 内部切 prompt） | Phase 2 |
| (d) | 触发机制：时钟为骨 + 事件加急 + budget | 部分（Phase 1 只有 15:30 收盘 reflection tick） | Phase 1 部分 / Phase 2 完整 |
| (e) | `agent_runs` → `agent_episodes`（trigger / thesis_ids / outcome_summary 三列） | ✅ 最小改造 | Phase 1 |
| (f) | Chat 降级为 Interaction 通道 | ✅ | Phase 1 |
| (g) | 用户输入分类 + intent tagging | 简化版（3 类：instruction / feedback / question） | Phase 1 简化 / Phase 2 完整 7 类 |

### 关键修订（相对 v0）

1. **Thesis 归 account 不归 agent**：thesis 本质是"持仓的理由"，account 已经在 PositionEvent payload 存 `thesis: String`，升一等是 account 内部演进；agent 只是 thesis 的主要作者，**不是所有者**。这解决 v0 里 `SimulatedPosition.thesis_id` 跨 BC 引用问题。
2. **大幅瘦身 Phase 1**：v0 想一次做完七抉择（估算 8-12 周），review 指出实际 15-20 倍工作量。Phase 1 只做 3 件事，闭合最小学习循环。
3. **Episodic memory 推迟**：从 thesis_events + agent_episodes 派生即可，不单建表；vector embedding 推 Phase 2。
4. **多 mode pipeline 推迟**：chat-only pipeline 用 prompt segment 区分触发源，不拆四条独立 pipeline。
5. **Intent tagging 简化为 3 类**：v0 的 7 类（preference/feedback/correction/instruction/trigger/question/style_pref）实际边界很糊。Phase 1 只分 instruction / feedback / question，preference/correction 都走 question 让 agent 自决要不要写 principle。
6. **A 股特化补全**（v0 漏的）：涨跌停作为 thesis invalidation；摩擦成本（印花税 0.05% 卖出 + 过户费 + 滑点）；T+1 已在 account 强制。
7. **安全网必备**（见 § 5）：reflection 对照 invalidation / principle proposed→active / regime_tags / bull-bear steelman / 10 seed principles / round-trip cap。

---

## 3. 数据模型 (Phase 1)

### 3.1 新增表（归属 account BC）

```sql
-- Thesis: 持仓的"为什么"，aggregate root
theses (
  id TEXT PRIMARY KEY,
  hypothesis TEXT NOT NULL,
  invalidation TEXT NOT NULL,          -- 结构化条件 + 自然语言双写
  validation_checks TEXT,              -- JSON array (validation 列表不需查询)
  conviction TEXT NOT NULL,            -- Low/Medium/High
  state TEXT NOT NULL,                 -- drafted | active | validated | drifted | invalidated | abandoned
  regime_at_creation TEXT,             -- bull/bear/choppy（创建时市场状态）
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  closed_at INTEGER
);

-- Thesis ↔ 股票多对多（一个 thesis 可对应多只）
thesis_codes (
  thesis_id TEXT NOT NULL,
  code TEXT NOT NULL,                  -- StockCode
  PRIMARY KEY (thesis_id, code)
);

-- Thesis 事件链（状态机 + 用户反馈 append-only）
thesis_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  thesis_id TEXT NOT NULL,
  kind TEXT NOT NULL,                  -- created/activated/validation_check_hit/drifted/invalidated/validated/abandoned/user_feedback
  payload TEXT,                        -- JSON
  occurred_at INTEGER NOT NULL
);
```

### 3.2 新增表（归属 agent BC）

```sql
-- Principles: 长期投资原则 / 已知偏差（替代 InvestorMemory）
principles (
  id TEXT PRIMARY KEY,
  body TEXT NOT NULL,                  -- ≤120 字
  category TEXT NOT NULL,              -- principle/known_bias/risk_preference
  origin TEXT NOT NULL,                -- user_stated | agent_inferred
  state TEXT NOT NULL,                 -- proposed | active | dormant | retired
  regime_tags TEXT,                    -- JSON array: ['bull','bear','choppy']
  hit_count INTEGER NOT NULL DEFAULT 0,
  last_applied_at INTEGER,
  created_at INTEGER NOT NULL
);
-- 软删除替代 supersedes 链表：旧条 state=retired，新条独立
```

### 3.3 修改表

```sql
-- 原 agent_runs 改名 + 加列
ALTER TABLE agent_runs RENAME TO agent_episodes;
ALTER TABLE agent_episodes ADD COLUMN trigger_kind TEXT;   -- Scheduled/UserMessage/UserInstruction
ALTER TABLE agent_episodes ADD COLUMN thesis_ids TEXT;     -- JSON array (低频查询 ok)
ALTER TABLE agent_episodes ADD COLUMN outcome_summary TEXT;
ALTER TABLE agent_episodes ADD COLUMN parent_episode_id TEXT;  -- 因果链（reflection episode 引用其复盘的 decision episode）

-- 原 simulated_positions 加 thesis 引用
ALTER TABLE simulated_positions ADD COLUMN thesis_id TEXT;
```

### 3.4 删除 / 迁移

- `app_state[KEY_MEMORY]`（InvestorMemory）—— 删，一次性迁移到 principles 表（每条 list entry → 一条 active principle，category 按 list 名映射）
- 旧 `PositionEvent.opened.payload.thesis: String` —— 启动时 backfill：每条创建一个 legacy Thesis（hypothesis = 旧 thesis 文本，state = active 或按当前持仓状态推断）；position 的 `thesis_id` 指向它

### 3.5 不做（Phase 1 明确不建）

- ❌ `episodic_memories` 表 —— 从 thesis_events + agent_episodes 派生视图即可
- ❌ `episode_actions` / `episode_theses` 关联表 —— Phase 1 用 JSON 列足够（episode 量级小）
- ❌ Vector embedding 列 —— Phase 2 决定本地 vs API

---

## 4. Pipeline 改动 (Phase 1)

### 4.1 保留单一 chat pipeline

`pipeline/chat.rs` + `pipeline/agent/loop_.rs` **结构不动**。内部按触发源在 prompt 注入不同 segment：

| 触发源 | Prompt segment | 工具子集 |
|---|---|---|
| `UserMessage` | identity + active principles top-N + 最近 thesis 摘要 | 全部 |
| `UserInstruction` | 同上 + "用户已明确指令，按规则执行不要反复确认" | 全部 |
| `Scheduled (15:30 reflection)` | identity + reflection 规则 + 今日 closed thesis 列表 + 触及 invalidation 的 active thesis 列表 | 只读工具 + `update_principle` + `update_thesis_state` |

`loop_.rs` 加 **round-trip cap**（单次 run 最多 N 次 LLM round-trip，N=20 默认），防工具循环爆 token。

### 4.2 新增 reflection 用例

新文件 `pipeline/agent/reflect.rs`：

```rust
pub async fn run_close_reflection() -> Result<EpisodeId> {
    // 1. 收集今日 closed positions + 触及 invalidation 的 active theses
    // 2. 为每条构造 reflection context（thesis 全文 + 事件链 + 期间行情）
    // 3. 调 loop_.rs 跑一次 agent run（mode = reflection，工具子集受限）
    // 4. agent 必须对照 thesis.invalidation / validation_checks 逐条勾选
    //    输出 principle proposals + thesis state transition
    // 5. 写 agent_episodes(trigger_kind=Scheduled, parent=None)
}
```

scheduler 加一条 tick：每个交易日 15:30 (Asia/Shanghai) 触发。非交易日跳过。

### 4.3 工具变更

**新增工具**（agent 在 reflection / interaction 时调用）：
- `create_thesis(hypothesis, invalidation, validation_checks, conviction, target_codes)`
- `update_thesis_state(thesis_id, new_state, reason)`
- `attach_thesis_feedback(thesis_id, text)`
- `propose_principle(body, category, origin, regime_tags?)` — 写入 state=proposed
- `confirm_principle(principle_id)` — proposed → active（用户复述或 ≥3 hit 时调）
- `retire_principle(principle_id, reason)` — active → retired

**修改工具**：
- `open_position` 加必填 `thesis_id`（或允许 inline 创建：`open_position(..., new_thesis: {...})`）
- `close_position` 自动写一条 thesis_event（不再依赖 agent 自觉）

**删除工具**：
- `update_memory` / `remove_memory`（被 principle 工具替代）

---

## 5. 安全网（Phase 1 必备）

### 5.1 Reflection 反"自我合理化"

`reflect.rs` 的 prompt 强制结构：

```
对于每个被复盘的 thesis：
1. 列出 invalidation 条件（来自 thesis 原文，不要改写）
2. 逐条勾选：✅ 未触发 / ⚠️ 部分触发 / ❌ 已触发
3. 列出 validation_checks（来自 thesis 原文）
4. 逐条勾选：✅ 已验证 / ⏸️ 未发生 / ❌ 反向
5. 基于上述对照，给出归因（不允许引入 thesis 创建时没声明的因素）
6. 提取可学习 principle proposal（必须能反向应用到未来类似场景）
```

不允许 agent 自由叙事"市场非理性""黑天鹅"。

### 5.2 Principle 防膨胀 / 防死循环

- 写入 state = `proposed`，需 ≥3 次 hit_count 或用户复述才升 `active`
- `origin = user_stated` 的 principle，**agent 不能自己给它加 hit_count**，只在用户消息里复述时 +1
- `origin = agent_inferred` 的 principle，30 天未 hit → `dormant`，不再进 prompt
- prompt 注入上限：active principles ≤ 25 条，按 hit_count 降序

### 5.3 Regime 切换

- `domain/quotes` 增加 `current_regime() -> Regime { Bull, Bear, Choppy }` 派生函数（简化版：基于上证指数 20/60 日均线 + 波动率）
- Principle 召回时按 `regime_tags` 过滤；空 tags 视为通用
- 切换 regime（连续 N 个交易日新 regime 稳定）时，所有 active principles 进入"待重新验证"标记，下次 reflection 优先处理

### 5.4 identity.md 硬约束

新 identity.md 必须包含（细化见 § 7）：
- 操盘手身份 + 学习者用户角色（保留 v0）
- **Bull/Bear Steelman 章节**：Decision 时强制先 bear case 再 bull case 再裁决
- **数字纪律**：所有价格、涨跌幅、PnL 计算必须调 tool，不允许自己算
- **口头偏好 vs 行为偏好冲突**：主动指出，不闷头服从

### 5.5 验证：模拟账户收益率（主）+ 机制健康度（辅）

**Agent 有效性的 ground truth = 模拟账户收益率本身。**

模拟盘存在的全部意义就是用真实价格验证或证伪每笔决策。account 模块已经现算 `realized_pnl / unrealized_pnl / total_assets / drawdown / win_rate`，**这就是验证**。绝对值，不需要 baseline 对照（v0 没怎么用过、30 天 PnL 噪音 >> 信号，对照本身没意义）。

主验证指标（从 account 派生，已存在或微调即可）：

| Metric | 来源 | 何时看 |
|---|---|---|
| **累积收益率** | `total_pnl / INITIAL_CAPITAL` | 持续累积 |
| **最大回撤** | `max(peak - trough) / peak` 滑动窗口 | 持续累积 |
| **Win rate** | closed positions 中盈利占比 | ≥10 笔起有意义 |
| **平均持仓周期** | closed positions 的 open→close 时长均值 | 反映决策风格漂移 |
| **Thesis 命中率** | 状态机走到 `validated` 的 thesis / 总 closed thesis | learning loop 的直接指标 |
| **Invalidation 命中后亏损** | 触发 invalidation 后的平均亏损 | 验证止损纪律是否被遵守 |

这些**不需要预先准备**——Phase 1 上线即累积，时间越长信号越强。

机制健康度指标（辅助诊断，不是验证）：

防止"系统在跑但跑歪了"——管道没堵不代表流的是水。从 day 1 可观测：

| Metric | 含义 | 健康阈值 |
|---|---|---|
| Thesis 完整度 | 新建 thesis 带结构化 invalidation + ≥2 validation_checks 的比例 | > 90% |
| Reflection 触发率 | 每个 closed / invalidated thesis 是否都有对应 reflection episode | 100% |
| Reflection 对照率 | reflection 输出里逐条勾选 invalidation/validation_checks 的比例 | > 80% |
| Principle 流动性 | proposed→active 转化率 + active→dormant 淘汰率 | 两边都 > 0 |
| Principle origin 分布 | agent_inferred 占比逐月上升 | agent 真在学的信号 |
| Regime 切换检测 | regime 转换被正确记录 | 历史回放可验证 |

主辅关系：主指标低（亏钱）但辅指标全绿 → agent 机制对、市场认知错，迭代 prompt / principles；主辅都差 → 系统级 bug；主指标好但辅指标差 → 蒙的，下个 regime 大概率失效。

实现：
- 主指标：复用 `infrastructure/account/valuation.rs` + 加几个派生函数
- 辅指标：新 `infrastructure/agent/health_metrics.rs`（不存表，每次现算）
- SettingsPage 加两个面板：「账户表现」+「Agent 机制健康」

### 5.6 Seed principles

启动时如果 principles 表为空，从 identity.md 提取 10 条 hand-written seed 写入（state=active, origin=user_stated）：

```
1. 信息不足时观察 > 交易
2. 偏多/偏空判断必须给后续验证清单
3. 不使用"必涨""稳赚"等表达
4. 触及 invalidation 立即平仓，不论盈亏
5. 单笔仓位 ≤ 总资产 X%（X 见 sizing.rs）
6. 传闻 / 二手转述不构成开仓理由
7. 涨停板出现 = 流动性断点，不在涨停价追入
8. 解禁日 / 财报日附近降低仓位敏感度
9. 主板 ±10% / 创业板 ±20% / 科创 ±20% / 北交所 ±30% 是硬边界
10. 模拟盘里的盈亏归因必须能反推到 thesis 而不是市场情绪
```

---

## 6. 触发模型 (Phase 1)

仅一条自驱 tick：

| Tick | 时间 | 行为 |
|---|---|---|
| Close reflection | 每交易日 15:30 (Asia/Shanghai) | 调 `reflect.rs::run_close_reflection`，复盘当日 closed + 触发 invalidation 的 active thesis |

其他触发源全部用户驱动（chat 消息）。事件加急（涨跌停 / 财报披露 / 北向异动）+ 盘前 09:00 observation tick + 整点巡检 → Phase 2。

**Budget**：
- 单次 LLM run round-trip cap = 20（在 `loop_.rs` 内核）
- Phase 1 没有事件触发，外层频率限流暂不需要

---

## 7. identity.md 改写要点

旧的 identity.md（166 行）改写后预期结构：

```
# GangZiTerminal Agent Identity (v2)

## 1. 你是谁
（保留 v0：操盘手 + 学习者用户）

## 2. 你怎么被唤醒
- 用户消息（最常见）
- 收盘后 15:30 reflection tick（自驱）
- 用户明确触发"立即跑一次决策"

## 3. 决策框架（含 Bull/Bear Steelman）
判断标的时按 7 层（事实/对象/方向/传导/定价/验证/策略）；**给结论前必须在内部先写 bear case + bull case 各 3 条再裁决**（不必输出给用户，但回答中要体现两面权衡）

## 4. Thesis 纪律
- 开仓必须先 create_thesis（或 open_position 内联 new_thesis）
- thesis 必须含 invalidation（结构化条件 + 自然语言）+ validation_checks
- thesis 可独立于持仓存在（只跟踪不建仓）

## 5. Reflection 纪律
- 15:30 tick 调用时，对每个 closed/triggered thesis 必须对照 invalidation/validation_checks 逐条勾选
- 不允许自由叙事"市场非理性"
- 提取 principle proposal 必须能反向应用未来

## 6. Memory 纪律（Principles）
- 用户口头偏好 → propose_principle(origin=user_stated)
- 自己 reflection 学到 → propose_principle(origin=agent_inferred)
- 口头偏好与用户行为指令冲突 → 主动指出，不闷头服从

## 7. 数字纪律
价格 / 涨跌幅 / PnL / 仓位计算必须调 tool。

## 8. 工具一览
（保留 v0 + 新增 thesis/principle 工具）

## 9. 输出风格
（保留 v0：操盘手讲思路，不研报化）
```

---

## 8. 依赖与边界自检

依赖矩阵不变（architecture.md § 1.3）：

- `theses` / `thesis_events` / `thesis_codes` 归 **account BC** → `domain/account/thesis.rs` + `infrastructure/account/thesis_repo.rs`
- `principles` / `agent_episodes` 归 **agent BC** → `domain/agent/principle.rs` + `infrastructure/agent/`
- `account` 不感知 agent ✅（thesis 是 account 概念，agent 只是消费者通过工具调用）
- `agent` 通过 `adapters/agent_tools/` 调 account 工具（与现状一致）
- `ThesisId` newtype 放 `domain/account/thesis.rs`（不进 shared）

---

## 9. 迁移路径（**简化版**：直接重建 DB）

**决策**：用户没有承重的存量数据 → SCHEMA_VERSION 升到 2，启动时若现存 DB 版本 < 2 自动 rename 成 `.legacy-{ts}` 备份，新建空 DB 跑 v2 schema。

```
1. connection 层：检测旧 schema_meta.version < 2 → 备份现存 SQLite 文件
2. migrations.rs：清空所有 upgrade_* / drop_legacy_* helper，只剩 CREATE TABLE
3. 新 schema 包含：theses / thesis_codes / thesis_events / principles / agent_episodes / agent_episode_turns
4. simulated_positions 直接带 thesis_id 列
5. 启动时若 principles 表为空 → seed 10 条手写原则
```

**不做** backfill：legacy Thesis / InvestorMemory 8-list → principles 迁移。数据清零，从零累积。
旧 chat history 一并备份在 `.legacy-{ts}`；如果需要查看可手动打开。

---

## 10. 不做的事（明确）

- ❌ Multi-agent / sub-agent orchestration（坚持单 identity + prompt 内部 bull/bear steelman）
- ❌ Trajectory compression for fine-tuning（不是训模型）
- ❌ 真实券商接入（仍只是模拟）
- ❌ Messaging gateway (Telegram / Slack)
- ❌ 动 quotes / news 接口
- ❌ 旧 briefing / review JSON 契约复活
- ❌ Episodic vector embedding (Phase 2 决定)
- ❌ Multi-mode pipeline 拆分 (Phase 2 决定)
- ❌ 事件触发 + priority queue (Phase 2 决定)
- ❌ Intent tagging 7 类完整版 (Phase 2 决定)

---

## 11. Phase 2 触发条件

Phase 2 不预排期，**满足以下任一**再启动：

- 累积 ≥ 50 条 closed thesis（episodic 召回有意义的最小样本量）
- 累积 ≥ 200 条 agent_episodes
- Phase 1 跑 ≥ 3 个月（覆盖至少一次 regime 切换）
- backtest baseline 显示 Phase 1 agent 在某类场景反复犯错（指出具体改进方向）

Phase 2 候选清单（按重要度）：

1. **双层 reflection**：低层（观察→次日价格因果，watchlist 也学）+ 高层（thesis→outcome，现有）
2. **盘前 09:00 observation tick + 整点巡检**：完整时钟自驱
3. **事件触发**：涨跌停 / 财报披露 / 北向异动 / 停复牌 → 加急 trigger + priority queue + budget
4. **Episodic memory + embedding 召回**：thesis_events + agent_episodes 派生视图 → vector index
5. **Multi-mode pipeline 拆分**：observe.rs / decide.rs / reflect.rs 独立
6. **Intent tagging 7 类完整版**：preference / feedback / correction / instruction / trigger / question / style_pref
7. **Today 页 episode 时间线 UI**：用户围观主屏

---

## 12. Phase 1 工作分解（可执行）

按依赖顺序：

```
Week 1：domain + infrastructure
  ☐ domain/account/thesis.rs: Thesis aggregate + ThesisId newtype + 状态机
  ☐ domain/agent/principle.rs: Principle aggregate + 状态机
  ☐ infrastructure migration: theses/thesis_codes/thesis_events/principles 4 表
  ☐ infrastructure/account/thesis_repo.rs
  ☐ infrastructure/agent/principle_repo.rs
  ☐ simulated_positions.thesis_id 列 + repo 增改
  ☐ agent_runs → agent_episodes rename + 4 列
  ☐ InvestorMemory 迁移脚本 + delete KEY_MEMORY
  ☐ Seed principles 注入逻辑

Week 2：pipeline + adapters
  ☐ pipeline/agent/reflect.rs: 收盘 reflection 用例
  ☐ pipeline/agent/loop_.rs: round-trip cap
  ☐ pipeline/scheduler.rs: 15:30 reflection tick
  ☐ adapters/agent_tools/theses.rs: create_thesis / update_thesis_state / attach_thesis_feedback
  ☐ adapters/agent_tools/principles.rs: propose_principle / confirm_principle / retire_principle
  ☐ open_position 工具加 thesis_id（或 inline new_thesis）
  ☐ close_position 自动写 thesis_event
  ☐ 删除 update_memory / remove_memory 工具

Week 3：identity + 安全网 + 验证
  ☐ identity.md 改写（§ 7 结构）
  ☐ Bull/Bear steelman prompt segment
  ☐ Reflection 对照 invalidation prompt segment
  ☐ Regime 派生函数（domain/quotes 加 current_regime）
  ☐ 主指标（账户表现）+ 辅指标（机制健康）派生函数
  ☐ Backfill 跑通：legacy thesis 创建 + position 关联
  ☐ architecture.md § 2.4 / § 3 / § 4 更新（删旧 spec）
  ☐ cargo check + cargo test 全绿

Week 4：前端（§ 14）
  ☐ Tauri commands：list_theses / get_thesis / list_thesis_events / list_principles / get_principle_stats / list_agent_episodes / trigger_reflection_now / get_account_metrics
  ☐ Events：theses-changed / principles-changed / agent-episode-completed / reflection-tick-completed
  ☐ Theses 页（列表 + 详情，含 invalidation/validation 勾选状态）
  ☐ Principles 页（三列：proposed / active / dormant+retired + 健康度小条）
  ☐ Today 页改造（episode 时间线 + active theses 摘要 + 下次自驱倒计时）
  ☐ SimulationPage 改造（关联 thesis 卡片 + 主指标面板）
  ☐ ChatPage 微调（intent tag + agent 自驱消息视觉区分 + thesis/principle 工具调用内联链接 + 删旧 memory UI）
  ☐ "问 agent" context-aware 跳转按钮（Theses / Positions / Today / News / Principles / Market 触发点）
  ☐ Settings 加 reflection 立即触发按钮
  ☐ 默认首屏改为 Today
  ☐ npm run build 全绿
```

DDD 自检（每周末跑一次）：

```bash
grep -rE "use crate::adapters" src-tauri/src/pipeline src-tauri/src/domain src-tauri/src/infrastructure
grep -rE "use crate::pipeline" src-tauri/src/infrastructure src-tauri/src/domain
grep -rE "use crate::infrastructure" src-tauri/src/domain
grep -rE "use crate::(adapters::agent|pipeline::agent|domain::agent|infrastructure::agent)" \
  src-tauri/src/{domain,infrastructure,pipeline}/{quotes,account,news}
# 任一非空都是 bug
```

---

## 13. 已知风险 + 缓解

| 风险 | 缓解 |
|---|---|
| Reflection 自我合理化 | § 5.1 强制对照 invalidation/validation_checks，禁止自由叙事 |
| Principle 死循环（RLHF reward hacking 本地版）| § 5.2 区分 user_stated / agent_inferred，前者 hit_count 只由用户复述 +1 |
| 牛市学的 principle 熊市用 | § 5.3 regime_tags + 切换时进入待重新验证 |
| 早期 agent 乱开仓 | 接受——模拟盘里乱开仓 = reflection 燃料；seed principles + account 硬规则 + 用户随时干预已是足够护栏 |
| 验证 agent 是否有效 | § 5.5 主：模拟账户收益率（绝对值，无需 baseline）；辅：机制健康度防"系统跑歪" |
| 单 identity 回声室 | § 5.4 identity.md 强制 bull/bear steelman |
| 涨跌停 = 流动性断点 | 工具层校验已有；新增：触板写 thesis_event(kind=validation_check_hit) |
| 摩擦成本 | open_position / close_position 时计算（印花税 0.05% 卖出 + 过户费 0.001% + 滑点 0.1%）|

---

## 14. 前端 spec

### 14.1 核心原则

**用户在前端只观察，不操作 agent 的内部状态。**

身份定位回到 identity.md 原文：用户是**学习者/观察者**，agent 是**操盘手**。前端不能给用户提供"代行 agent 决策"的按钮（abandon thesis / retire principle / force close 等），否则：

1. agent 不知道用户为什么这么改 → 学习信号丢失
2. 持仓事件链的"为什么"会被点击信号截断 → reflection 无法归因
3. 用户被推回审批员角色 → 违反 agent 自驱设定

任何"想让 agent 做的事"——提问 / 分析 / 反馈 / 命令操作 / 改主意——**全部走 chat 唯一输入通道**。chat agent run 内部可以触发任意工具调用（read / write / thesis / principle 全套），这是现有机制保留不变。

### 14.2 五个 surface + 性质

| Surface | 性质 | 用户能做什么 |
|---|---|---|
| **Today** | 主屏，read-only | 围观 agent 今日活动 / 账户横条 / active theses 摘要 |
| **Theses** | read-only（新） | 看 thesis 详情 / 事件链 / 关联持仓 |
| **Positions** | read-only（改造现 SimulationPage） | 看持仓 / 事件链 / 关联 thesis / 账户主指标 |
| **Chat** | **唯一输入通道** | 提问 / 指令 / 反馈 / 触发任何 agent 行为 |
| **Principles** | read-only（新） | 看 agent 学到了什么 / hit_count / origin / state |

Settings 是 system-level，不算 agent surface。

### 14.3 各 surface 关键要素

**Today（主屏）**：
- 今日 agent 活动时间线（episode 倒序，每条卡片 = trigger + thesis 关联 + action + outcome）
- 当前 active theses 摘要条（3-5 个，hypothesis 一行 + 状态色块）
- 账户横条（今日 PnL / 总资产 / open 持仓数）
- 下一次自驱倒计时（Phase 1 只有 15:30 reflection）

**Theses**：
- 列表：按 state 分组（drafted / active / drifted / closed），关键字段 = hypothesis 摘要 + conviction + 关联股票 + invalidation 勾选进度 + 关联 position
- 详情：hypothesis 原文 + invalidation/validation_checks 逐条带 ✅⏸️❌ 勾选 + thesis_events 时间线 + reflection lesson（如已 close）

**Positions**：
- 每个 position 卡片显眼显示关联 thesis 摘要 + state（点跳 Theses 详情）
- 持仓详情：事件链（已有）+ thesis 全文 + 复盘结论
- 顶部账户主指标面板（§ 5.5 主指标）

**Chat**：
- 用户消息上方显示 intent tag（instruction / feedback / question 三类，Phase 1 简化版）
- agent 自驱播报消息（reflection tick）在 chat 流里视觉区分（不同背景或前缀）
- agent 调 thesis / principle 工具时，chat 内联可点击链接（跳到 Theses / Principles 详情）
- 删掉旧的 `memoryUpdates / memoryRemovals` UI

**Principles**：
- 三列 / 三组：proposed / active / dormant + retired
- 每条显示：body / category / origin（🧑 user_stated / 🤖 agent_inferred）/ hit_count / last_applied_at / regime_tags
- 顶部健康度小条：active 总数 / 本月新增 / 本月淘汰 / agent_inferred 占比走势

### 14.4 Context-aware "问 agent" 入口

每个 read-only surface 都有"问 agent"按钮，**跳到 chat 并预填上下文**（用户可继续编辑后发送）。本质是预填好的 chat 消息——agent 视角和用户手敲完全一致。

| 触发点 | 预填示例 |
|---|---|
| Theses 详情 → "问 agent 关于这个 thesis" | `[关于 thesis #123 "光模块算力需求兑现"]: ` |
| Theses 详情 → "让 agent 重新评估" | `重新评估 thesis #123，当前 invalidation 触发了 2/4，你怎么看？` |
| Position 详情 → "问 agent 这笔的判断" | `[关于持仓 600519]: ` |
| Position 详情 → "让 agent 复盘这笔" | `这笔 600519 已经平了，帮我复盘当时的 thesis 和实际走势的差距` |
| Today episode 卡片 → "让 agent 展开" | `关于今天 15:30 的 reflection #ep_xxx，详细讲讲你的归因` |
| News 卡片 → "让 agent 解读" | `这条新闻 [标题]，你觉得影响哪些板块？` |
| Principles 列表项 → "问 agent 关于这条" | `关于 principle "信息不足时观察>交易"，最近你是怎么应用的？` |
| Market 大盘 → "让 agent 扫一遍" | `现在大盘 X%，扫一下市场看有没有机会` |

### 14.5 系统级按钮边界（明确）

| 允许 | 理由 |
|---|---|
| Settings 里 `agentEnabled` 开关 | 系统生命周期，不是 agent 决策 |
| Settings 里"立即跑一次 reflection" | trigger 类，叫醒 agent，不代它做决定 |
| Settings 里"重置模拟账户" | 系统级，已存在 |
| 各 surface 的"问 agent" 跳转按钮 | 只是预填 chat，没绕过 agent |

| 不允许 | 替代方式 |
|---|---|
| Abandon / 改 thesis 状态按钮 | chat："我觉得 thesis #123 不成立了" |
| Retire / confirm principle 按钮 | chat："不要再用 X 这条原则了" |
| Force close position 按钮 | chat："平掉 600519" |
| 调止损 / 加减仓按钮 | chat："把 600519 止损调到 X" |

### 14.6 Agent 主动消息通知

Phase 1 自驱只有 15:30 reflection。完成时：

- ✅ chat 流推一条 agent 消息（视觉区分自驱 vs 应答）
- ✅ Today 时间线显示 episode 卡片
- ❌ 不弹系统级 toast / 不改标题栏未读数

理由：Phase 2 加事件触发后通知频率会高得多，强通知会变骚扰；保留克制风格让用户主动来看，符合"操盘手做事，学习者围观"的关系。

### 14.7 默认首屏

应用启动默认进 **Today**（不是 Chat）。这是认知转换的第一步——主屏是 agent 工作全景，chat 是干预通道。当前默认 Chat 改掉。

### 14.8 Phase 1 前端工作边界

**Phase 1 做**：
- Theses 页（列表 + 详情）—— **新建，重点**
- Positions 页改造（加 thesis 关联 + 主指标面板）
- Today 页改造（episode 时间线 + theses 摘要 + 下次自驱倒计时）
- Chat 页微调（intent tag + agent 自驱消息视觉区分 + 工具调用内联链接 + 删旧 memory UI）
- Principles 页（列表，只读，不强求精美）
- "问 agent" 跳转按钮 接入主要触发点（Theses / Positions / Today / News）
- Settings 加 reflection 立即触发按钮

**Phase 1 不做**：
- News / Market 大改造（保留现状）
- KlineChart 重做
- 移动端适配
- 高级可视化（thesis 状态机图 / principle hit_count 走势图）—— 先用表格
- 主题 / 暗色细调

### 14.9 Hook / 数据流影响

新建：
- `useTheses()` —— 列表 + 详情，订阅 `theses-changed` 事件
- `usePrinciples()` —— 列表 + 状态变更订阅
- `useAgentEpisodes()` —— Today 时间线数据源（替代或扩展现 `useAgentEventStream`）

修改：
- `useChatMessageStream` —— 解析 agent 自驱消息标记；解析 thesis/principle 工具调用并产出内联链接 props
- `useAccountSnapshot` —— 加主指标（drawdown / win_rate / avg_holding_days / thesis_hit_rate）

### 14.10 Tauri command 影响

新增 commands（adapters/）：
- `list_theses(filter?)` / `get_thesis(id)` / `list_thesis_events(thesis_id)`
- `list_principles(state?)` / `get_principle_stats()`（健康度小条数据）
- `list_agent_episodes(date_range?)` —— Today 时间线
- `trigger_reflection_now()` —— Settings 按钮触发
- `get_account_metrics()` —— 主指标面板派生数据

事件 emit：
- `theses-changed` / `principles-changed` / `agent-episode-completed`
- `reflection-tick-completed`（特殊事件，前端用来推 chat 自驱消息）
