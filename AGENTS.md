# AGENTS.md

> 这是给"读代码的 agent"的入口页。保持短——所有详细规约在 `docs/architecture.md` 里。

## Project

A 股研究 + 模拟交易学习终端。Agent 自驱动：从市场数据 + 资讯识别机会 → 在模拟账户里实盘验证 → 沉淀成可审计、可复盘的判断链。

**不连真券商，只做模拟。**

## Read First

- **[docs/architecture.md](docs/architecture.md)** ← **权威设计基线**。模块边界 / 数据模型 / Agent 输出协议 / 数据流 / DDD-lite 结构 / 6 项核心哲学
- [docs/provider-design.md](docs/provider-design.md) — Provider 抽象（wire format 不是厂商）
- [docs/development.md](docs/development.md) — 开发命令 / Tauri runtime / 配置

## Stack

- Tauri v2（Rust 后端 + React/TS 前端）
- SQLite via `rusqlite`（持久化真源）
- 自托管 agent loop（`src-tauri/src/pipeline/agent/`）——支持 Anthropic / OpenAI Responses / OpenAI Chat Completions 三个 wire format
- 行情数据：**TDX**（实时报价主路径）+ **Eastmoney**（BJ / fallback / 分钟 K）+ **TuShare Pro**（历史 / 财务 / 板块 / 基金）

## Architecture Boundary

参见 [architecture.md § 1.3](docs/architecture.md)。简短版：

```
React UI
   ↓ invoke / events
adapters/            Tauri commands + LLM tools
   ↓
pipeline/            use cases: chat / account / refresh / scheduler
   ↓
infrastructure/      SQLite / HTTP / provider / cache / snapshot
   ↓
domain/              pure entities, value objects, rules
```

业务依赖单向。`domain` 不依赖外层；`infrastructure` 不依赖 `pipeline/adapters`；`pipeline` 不暴露 IPC；`adapters` 是唯一 Tauri command surface。Quotes / SimAccount / News **不感知** Agent。

## Code Map（current state）

```
src-tauri/src/
├── main.rs              setup hook + invoke_handler
├── domain/              纯 domain：无 I/O、无 Tauri 依赖
│   ├── shared/          StockCode / PositionId / NewsId / Money / Shares / time
│   ├── account/         Account aggregate + Position / PositionEvent / rules / sizing / snapshot
│   ├── quotes/          Quote/Kline/Indicator 类型 + 纯指标函数 + 交易日历逻辑
│   ├── news/            NewsItem / NewsStatus / NewsError
│   └── agent/           AgentRequest / AgentEvent / ProviderKind / InvestorMemory
├── infrastructure/      I/O 实现：SQLite / HTTP / provider / cache / snapshot
│   ├── account/         repository / snapshot_cache / valuation / watchlist / migration
│   ├── quotes/          TuShare / Eastmoney / TDX / realtime dispatch / cache / scanner
│   ├── news/            NewsNow / RSS / article extractor / repository
│   ├── agent/           ChatProvider + Anthropic / OpenAI Responses / OpenAI Chat
│   ├── app_state/       KV repository
│   └── db/              SQLite connection + migrations
├── pipeline/            Use cases / 用例编排
│   ├── chat.rs          当前唯一 agent 入口：用户消息 → agent loop → assistant 消息
│   ├── agent/           loop / prompt / context / compact / observer / config / tools (Tool trait + Registry 抽象)
│   ├── account/         AccountService + close / subscriptions
│   ├── news/refresh.rs  资讯刷新用例
│   ├── market/          refresh / overview / universe / kline_warm
│   ├── history.rs · memory.rs · context.rs · quotes_fetch.rs · events.rs · stocks.rs · chat_attachments.rs · util.rs  跨 use case 复用的 helper
│   └── scheduler.rs     news / market / account / kline warm 后台 loop
├── adapters/            边界层
│   ├── *_commands.rs    唯一 Tauri IPC surface
│   └── agent_tools/     具体 LLM 工具实现（实现 pipeline::agent::tools::Tool；adapter 在 chat command 里构造 registry 注入 pipeline）
└── infrastructure/logging.rs

src/
├── App.tsx              render shell
├── components/          UI: ChatPage / SimulationPage / TodayPage / NewsPage / SettingsPage / MarketOverview / KlineChart / SecondaryView
├── hooks/               useAppState / chat stream / agent events / account / market / news / quotes / watchlist
├── lib.ts               defaultWatchlist
├── lib/                 UI 派生 state: simulation / format / news / market
└── types.ts             前端类型契约
```

> 当前后端已按 `domain / infrastructure / pipeline / adapters` 四层落地。新增业务能力优先放进对应 bounded context；不要在 `adapters` 或 React 里写业务规则。

## Core Commands

```bash
npm install
npm run build                                       # frontend (tsc + vite)
cargo check --manifest-path src-tauri/Cargo.toml    # rust check
cargo test  --manifest-path src-tauri/Cargo.toml    # rust tests (200+)
npm run tmux:start                                  # vite + tauri dev session
npm run tmux:logs / restart / stop
```

## Key Rules (Non-Negotiable)

参见 architecture.md § 1 完整 6 条。简短版：

1. **Agent Notify Mode** — 决策即执行，chat 是事后通知
2. **派生 over 存储** — cash / PnL 现算不存
3. **模块边界单向** — Quotes / SimAccount / News 不感知 Agent
4. **持久化先 event 后 state** — append-only audit
5. **Snapshot-first 数据访问** — Agent 读 snapshot，不在热路径直接 fetch
6. **不背历史包袱**（快速迭代期）— 删干净 vs deprecate

## Handoff Checklist

```bash
npm run build && \
cargo check --manifest-path src-tauri/Cargo.toml && \
cargo test  --manifest-path src-tauri/Cargo.toml
```

新增 / 修改 spec 内容 → 先改 `docs/architecture.md` 再改代码。

---

**This file**：本文档 ~100 行作为入口页。**任何架构 / 接口 / 数据模型规约都不写在这里**——写到 architecture.md。
