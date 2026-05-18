# AGENTS.md

> 这是给"读代码的 agent"的入口页。保持短——所有详细规约在 `docs/architecture.md` 里。

## Project

A 股研究 + 模拟交易学习终端。Agent 自驱动：从市场数据 + 资讯识别机会 → 在模拟账户里实盘验证 → 沉淀成可审计、可复盘的判断链。

**不连真券商，只做模拟。**

## Read First

- **[docs/architecture.md](docs/architecture.md)** ← **权威设计基线**。模块边界 / 数据模型 / Agent 输出协议 / 数据流 / DDD-lite 结构 / 7 项核心哲学
- [docs/provider-design.md](docs/provider-design.md) — Provider 抽象（wire format 不是厂商）
- [docs/development.md](docs/development.md) — 开发命令 / Tauri runtime / 配置

## Stack

- Tauri v2（Rust 后端 + React/TS 前端）
- SQLite via `rusqlite`（持久化真源）
- 自托管 agent loop（`src-tauri/src/pipeline/agent/`）——支持 Anthropic / OpenAI Responses / OpenAI Chat Completions 三个 wire format
- 行情数据：**TDX**（实时报价主路径）+ **Eastmoney**（BJ / fallback / 分钟 K）+ **TuShare Pro**（历史 / 财务 / 板块 / 基金）

## 🧭 DDD-Driven Development（**所有新开发必须遵守**）

后端按 **4 层 × 4 个 bounded context** 组织。任何新功能都按下面的流程走，不允许跳层、不允许跨 BC 反向依赖。

### 4 层（依赖单向，由上到下）

```
adapters/        Tauri commands + LLM 工具具体实现（唯一外部协议边界）
   ↓ 调用
pipeline/        application use cases：chat / account / refresh / scheduler
   ↓ 调用
infrastructure/  I/O 实现：SQLite / HTTP / provider / cache / snapshot
   ↓ 使用
domain/          纯类型 + 规则（无 I/O、无 Tauri、无 SQLite、无网络）
```

### 4 个业务模块（bounded context）

| 模块 | 职责 | 允许依赖 |
|---|---|---|
| **quotes** | 行情数据获取 / 技术指标 / 全市场扫描（只读） | 无（独立） |
| **account** | T+1 模拟交易 / 持仓事件溯源 / 派生估值 | quotes（仅 valuation 读 snapshot） |
| **news** | 多源资讯拉取 + 全文提取 | 无（独立） |
| **agent** | LLM 决策 loop + 长期记忆 + 工具调用 | quotes / account / news（通过 `adapters/agent_tools` 反腐译码） |

**硬约束**：`quotes` / `account` / `news` **绝对不能 import agent 代码**。Agent 是消费者，三个执行模块不知道它存在。

### 新功能"该放哪"判断流程

```
1. 这是什么业务概念？
   └─ 行情/指标/扫描 → quotes
      持仓/交易/规则 → account
      资讯/抽取        → news
      LLM/工具/记忆    → agent

2. 它该放哪一层？
   ├─ 纯类型 / 纯规则 / 纯计算（无 I/O）        → domain/<bc>/
   ├─ DB / HTTP / 外部 provider / 内存 snapshot → infrastructure/<bc>/
   ├─ 编排多个 infra 调用 / 后台 loop / use case → pipeline/<bc>/
   └─ Tauri command / LLM 工具实现              → adapters/

3. 检查依赖方向（用 grep 在 PR 前自验）：
   - domain/        不允许 use tauri | rusqlite | reqwest | infrastructure | pipeline | adapters
   - infrastructure/ 不允许 use pipeline | adapters
   - pipeline/      不允许 use adapters
   - {quotes,account,news} 任何一层 不允许 use crate::*agent*
```

完整规则见 [architecture.md § 1.3](docs/architecture.md) 跨模块依赖矩阵。

## Code Map（current state）

```
src-tauri/src/
├── main.rs              setup hook + invoke_handler
├── domain/              纯 domain：无 I/O、无 Tauri 依赖
│   ├── shared/          StockCode / PositionId / NewsId / Money / Shares / time
│   ├── account/         Account aggregate + Position / PositionEvent / rules / sizing / snapshot / cash
│   ├── quotes/          Quote/Kline/Indicator 类型 + 纯指标函数 + 交易日历逻辑
│   ├── news/            NewsItem / NewsStatus / NewsError
│   └── agent/           AgentRequest / AgentEvent / ProviderKind / InvestorMemory / Block / ToolDef
├── infrastructure/      I/O 实现：SQLite / HTTP / provider / cache / snapshot
│   ├── account/         repository / snapshot_cache / valuation / watchlist / migration
│   ├── quotes/          TuShare / Eastmoney / TDX / realtime dispatch / cache / scanner
│   ├── news/            NewsNow / RSS / article extractor / repository
│   ├── agent/           ChatProvider + Anthropic / OpenAI Responses / OpenAI Chat + repository
│   ├── app_state/       KV repository
│   ├── db/              SQLite connection + migrations
│   └── logging.rs
├── pipeline/            Use cases / 用例编排
│   ├── chat.rs          当前唯一 agent 入口：用户消息 → agent loop → assistant 消息
│   ├── agent/           loop / prompt / context / compact / observer / config / tools (Tool trait + Registry 抽象)
│   ├── account/         AccountService + close / subscriptions
│   ├── news/refresh.rs  资讯刷新用例
│   ├── market/          refresh / overview / universe / kline_warm
│   ├── history.rs · memory.rs · context.rs · quotes_fetch.rs · events.rs · stocks.rs · chat_attachments.rs · util.rs  跨 use case 复用的 helper
│   └── scheduler.rs     news / market / account / kline warm 后台 loop
└── adapters/            边界层
    ├── *_commands.rs    唯一 Tauri IPC surface (app / app_state / chat / account / market / news / quotes / agent / proxy)
    └── agent_tools/     具体 LLM 工具实现（implement pipeline::agent::tools::Tool）
                         chat_commands 在每次 run 启动时构造 registry 注入 pipeline
```

```
src/
├── App.tsx              render shell
├── components/          UI: ChatPage / SimulationPage / TodayPage / NewsPage / SettingsPage / MarketOverview / KlineChart / SecondaryView
├── hooks/               useAppState / chat stream / agent events / account / market / news / quotes / watchlist
├── lib.ts               defaultWatchlist
├── lib/                 UI 派生 state: simulation / format / news / market
└── types.ts             前端类型契约
```

## Core Commands

```bash
npm install
npm run build                                       # frontend (tsc + vite)
cargo check --manifest-path src-tauri/Cargo.toml    # rust check
cargo test  --manifest-path src-tauri/Cargo.toml    # rust tests (224)
npm run tmux:start                                  # vite + tauri dev session
npm run tmux:logs / restart / stop
```

## Key Rules (Non-Negotiable)

参见 architecture.md § 1 完整 7 条。简短版：

1. **DDD-driven** — 新代码先选 BC + 选层；不允许跨层 / 反向依赖（本节顶部判断流程）
2. **Agent Notify Mode** — 决策即执行，chat 是事后通知
3. **派生 over 存储** — cash / PnL 现算不存
4. **模块边界单向** — Quotes / SimAccount / News 不感知 Agent
5. **持久化先 event 后 state** — append-only audit
6. **Snapshot-first 数据访问** — Agent 读 snapshot，不在热路径直接 fetch
7. **不背历史包袱**（快速迭代期）— 删干净 vs deprecate

## Handoff Checklist

```bash
# 1. 依赖方向自查（任一非空都是 bug）
grep -rE "use crate::adapters" src-tauri/src/pipeline src-tauri/src/domain src-tauri/src/infrastructure
grep -rE "use crate::pipeline" src-tauri/src/infrastructure src-tauri/src/domain
grep -rE "use crate::infrastructure" src-tauri/src/domain
grep -rE "use crate::(adapters::agent|pipeline::agent|domain::agent|infrastructure::agent)" \
  src-tauri/src/{domain,infrastructure,pipeline}/{quotes,account,news}

# 2. 门禁
npm run build && \
cargo check --manifest-path src-tauri/Cargo.toml && \
cargo test  --manifest-path src-tauri/Cargo.toml
```

新增 / 修改 spec 内容 → 先改 `docs/architecture.md` 再改代码。

---

**This file**：本文档保持 ~150 行作为入口页。**任何架构 / 接口 / 数据模型规约都不写在这里**——写到 architecture.md。
