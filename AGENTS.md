# AGENTS.md

> 这是给"读代码的 agent"的入口页。保持短——所有详细规约在 `docs/architecture.md` 里。

## Project

A 股研究 + 模拟交易学习终端。Agent 自驱动：从市场数据 + 资讯识别机会 → 在模拟账户里实盘验证 → 沉淀成可学习的判断链。

**不连真券商，只做模拟。**

## Read First

- **[docs/architecture.md](docs/architecture.md)** ← **权威设计基线**。模块边界 / 数据模型 / Agent 输出协议 / 数据流 / DDD-lite 结构 / 6 项核心哲学
- [docs/provider-design.md](docs/provider-design.md) — Provider 抽象（wire format 不是厂商）
- [docs/development.md](docs/development.md) — 开发命令 / Tauri runtime / 配置

## Stack

- Tauri v2（Rust 后端 + React/TS 前端）
- SQLite via `rusqlite`（持久化真源）
- 自托管 agent loop（`src-tauri/src/agent/`）——支持 Anthropic / OpenAI Responses / OpenAI Chat Completions 三个 wire format
- 行情数据：**TuShare Pro**（历史 / 财务 / 板块 / 基金）+ **Eastmoney ulist.np**（实时报价）

## Architecture Boundary

参见 [architecture.md § 1.3](docs/architecture.md)。简短版：

```
Chat → Agent → { Quotes, SimAccount, News }
                SimAccount → Quotes  (for valuation)
```

依赖单向。Quotes / SimAccount / News **不感知** Agent。

## Code Map（current state）

```
src-tauri/src/
├── main.rs              setup hook + invoke_handler
├── agent/               Self-hosted agent loop
│   ├── types.rs         Block / Message / AgentEvent / AgentRequest
│   ├── provider/        ChatProvider trait + anthropic / openai_responses / openai_chat 三家
│   ├── tools/           ToolRegistry + 14+ tools（quotes / scanner / research / funds / account / memory / news / positions）
│   ├── loop_.rs         tool-use 迭代 + 并行执行 + max_turns
│   ├── context.rs       3-tier 压缩
│   ├── compact.rs       summarize 摘要
│   ├── observer.rs      AgentEvent emit + agent_runs 落审计
│   └── config.rs        Provider/model/runtime KV 配置
├── pipeline/            Use cases / 用例编排（briefing/review/chat/refresh）
│   ├── briefing.rs / review.rs / chat.rs / refresh.rs
│   ├── stocks.rs        stocks 表刷新
│   ├── history.rs       chat 历史读取
│   └── runner.rs        briefing/review 共用 helper
├── quotes/              市场数据（TuShare + EM 实时）
│   ├── tushare/         10 个 TuShare 接口（stock_basic / klines / market_scan / index_daily / top_list / moneyflow / fund_*）
│   ├── eastmoney.rs     实时报价 ulist.np + 分时 push2his
│   ├── indicators.rs    技术指标纯函数（MA/EMA/MACD/RSI/KDJ/CCI/BOLL/ATR/OBV）
│   ├── scanner.rs       全市场扫描（filter + 排序 + 缓存）
│   ├── cache.rs / clock.rs / util.rs / validation.rs
│   └── mod.rs
├── account.rs           模拟账户（待按 spec 拆为 account/ 子目录 + 聚合根）
├── news.rs              资讯拉取（待整合到 news/ 子目录）
├── article/             资讯正文抽取
├── scheduler.rs         5 个 Tokio loop（briefing / review / quote refresh / news refresh / stocks refresh）
├── prompt.rs            prompt builder + briefing/review JSON parser
├── identity.md          Agent 身份（include_str! 注入 system block）
├── memory.rs            投资者记忆 merge（80 字 cap）
├── trade.rs             仓位 sizing
├── risk.rs              模拟账户风控校验
├── learning.rs          学习画像派生
├── agent_io.rs          持久化数据类型
├── models.rs            通用数据结构
├── chat_attachments.rs  图片粘贴白名单 + 大小 cap
├── security.rs          外部 URL 校验 + 8MB cap
├── db.rs                SQLite 层 + schema 迁移
└── logging.rs           tracing 初始化

src/
├── App.tsx              render shell
├── components/          UI: ChatPage / SimulationPage / TodayPage / SettingsPage / MarketOverview / KlineChart / SecondaryView
├── hooks/               useAppState / useChatMessageStream / useAgentEventStream / useNewsRefresh / useQuotes / useQuotesFetchStatus
├── lib.ts               defaultWatchlist
├── lib/                 UI 派生 state: learning / simulation / reviewSchedule / format / news / market
└── types.ts             前端类型契约
```

> 注：`account.rs` / `news.rs` 单文件 + `quotes/` 子目录是**当前过渡态**。spec § 9 的目标是 4 层 DDD（domain/infrastructure/application/adapters）。见 architecture.md § 7 Gap 表。

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
2. **派生 over 存储** — cash / PnL / learning_profile 现算不存
3. **模块边界单向** — Quotes / SimAccount / News 不感知 Agent
4. **持久化先 event 后 state** — append-only audit
5. **Snapshot-first 数据访问** — Agent 读 snapshot 不 fetch
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
