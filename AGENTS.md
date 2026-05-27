# AGENTS.md

> 给"读代码的 agent"的入口页。**本文件只规定"怎么工作"，不规定"做什么"**——业务契约全部以 `docs/design/` 下的 spec 为准。保持短。

## Project

A 股研究 + 模拟交易学习终端。Agent 自驱动：从市场数据 + 资讯识别机会 → 在模拟账户里实盘验证 → 沉淀成可审计、可复盘的判断链。

**不连真券商，只做模拟。**

---

## 🔒 Source of Truth：Specs

**`docs/design/` 下的 spec 是真源。代码是 spec 的物化产物，不是反过来。**

但 spec 是**设计规范**，不是**完美规范**——可能不完整、有歧义、有冲突、甚至有设计缺陷。这种情况**不是 agent 自由发挥的许可**，而是**强制反馈给用户**的信号。

### 不可破坏的纪律

1. **任何行为变更，spec 先行**
   - 用户改 spec → agent 改代码 → code diff 必须能对应到 spec diff
   - **禁止**："感觉 spec 不合理就直接改代码"——直接停下来跟用户对齐
2. **实现必须可追溯到 spec**
   - 每个模块 / 关键函数顶部用注释标锚：`// Spec: quotes-module.md §3.2`
   - 未来审计"代码是否还符合 spec"靠这些锚
3. **新增能力必有 spec 落点**
   - 没有 spec 不允许写新代码
   - 用户口头需求 → 先落到 spec → 再实现
4. **drift 审计是 agent 的本职**
   - 每个有意义的实现迭代结束，自查"代码做了 spec 没说的事 / spec 说了代码没做的事"
   - 主动汇报漂移点

### 🚨 实现中遇到 spec 问题的协议（最重要）

实现过程中只要触发以下任一情况，**立即停下来反馈给用户**，不要自行决断：

| 触发条件 | 报告格式 |
|---|---|
| spec 没说清某种情况怎么办 | "spec §X 在 Y 场景下没规定，可能的合理处理是 A / B / C，请定" |
| spec 两处说法冲突 | "spec §X 和 §Y 对 Z 的描述不一致，需要明确哪个是真意" |
| spec 描述的方案在实现层不可行 | "spec §X 要求 Z，但因为 [技术约束] 做不到，建议调整为 W，或保留 Z 并接受 [代价]" |
| spec 描述的方案能做但有明显设计缺陷 | "spec §X 现在能实现，但 [具体问题]，建议考虑调整" |
| 实现需要新概念 / 新状态，spec 里没有 | "需要引入 [概念]，spec 没覆盖，建议在 §X 补一段" |

**用户收到反馈 → 决定要不要改 spec → 改完 spec → agent 继续实现**。这是双方对齐的唯一通道。

### 对齐目标

> **用户和 agent 的协作锚是 spec。**
> 用户只调整 spec 文档，agent 根据 spec 更新代码。
> 代码不该让用户读才能理解系统行为——读 spec 就够。

---

## Read First（按顺序）

1. **[docs/design/architecture.md](docs/design/architecture.md)** — 整体设计入口 + 模块关系
2. **[docs/design/spec-guidelines.md](docs/design/spec-guidelines.md)** — spec 写作规范
3. **[docs/design/shared-types.md](docs/design/shared-types.md)** — 跨模块共享领域类型
4. **`docs/design/*-module.md`** — 各模块领域契约。写实现前 / 改实现前必读对应文件
5. **[docs/design/frontend-design.md](docs/design/frontend-design.md)** — 前端体验 / 视觉系统
6. **[docs/design/references/](docs/design/references/)** — 外部协议 / provider wire format 参考

---

## 技术栈（已锁定）

| 层 | 选型 | 备注 |
|---|---|---|
| 桌面壳 | **Tauri v2** | Rust 后端 + WKWebView，IPC 通信 |
| 前端 | **React 19 + Vite + TypeScript** | UI 层，**不持有业务真源** |
| K 线图 | **lightweight-charts** | TradingView 出品；技术指标自绘 |
| 后端 | **Rust 2021** | 全部业务逻辑 + I/O |
| 持久化 | **SQLite via `rusqlite`** | 嵌入式，单文件 |
| Schema 迁移 | **`rusqlite_migration`**（推荐）或 `refinery` | 一个改动一个 `.sql`，启动自动 apply |
| Rust↔TS 类型同步 | **`tauri-specta` + `specta`** | Rust 注解 → 自动生成 TS 函数 + 类型，编译期类型安全 |
| 前端状态 | **zustand** | UI state（panel / scroll / hover）。不要 Redux |
| 行情协议 | **`infrastructure/quotes/tdx`** | TDX 协议子模块（pytdx / mootdx wire compatible） |

---

## 后端架构

### DDD 分层（4 层，单向依赖）

```
adapters/        Tauri command / Agent tool / 外部协议 DTO（入站边界）
   ↓ 调用
pipeline/        use case + 后台任务 + 跨 infra 编排
   ↓ 调用
infrastructure/  I/O 实现：SQLite / HTTP / provider / 外部 crate adapter
   ↓ 使用
domain/          纯类型 + 规则（无 I/O、无 Tauri、无 SQLite、无网络）
```

为什么 4 层不是 3 层：同一个 use case（例如 `refresh_news`）会被 Tauri command、Agent tool、scheduler 三种入口触发；把 adapters 单独拉出来，`pipeline/` 里的 use case 不用关心自己被谁调用，每种入口写一层薄 adapter 即可。

**铁律**：
- `domain/` **不允许** `use tauri | rusqlite | reqwest | byteorder | flate2 | infrastructure | pipeline | adapters`
- `infrastructure/` **不允许** `use pipeline | adapters`
- `pipeline/` **不允许** `use adapters`
- 跨 BC 反向依赖：以 `docs/design/architecture.md` 为准

### Bounded Context

**以 `docs/design/architecture.md` 和各 `*-module.md` 为准。** 本文件不复述 BC 划分——避免和 spec 不同步。

### TDX 协议子模块

- **`src-tauri/src/infrastructure/quotes/tdx/`**：TDX HQ 二进制协议层
  - 暴露 `TdxHqClient` / 原始 `Bar` / `SecurityQuote` / `TdxMarket` 等
  - 内部只依赖 `byteorder` / `flate2` / `encoding_rs` / `thiserror`，不引用项目其他代码
  - 改动等同于改"外部依赖"——只在协议层有 bug / 缺能力时动
- **项目内 provider adapter（位于 `infrastructure/quotes/` 同目录下其他文件）**：把协议原始类型翻译成 domain 类型
  - 处理：ts_code ↔ TdxMarket、北交所 fallback、ohlcv → `Money`/`Price`/`Volume`、连接复用 / 失败重连、async wrapper（`spawn_blocking`）
  - 这一层归 `infrastructure` 层管，遵守 spec

---

## 前端架构

**前端不是独立后端，是 Rust 应用的 UI 层。**

| 数据流 | 机制 |
|---|---|
| 查询 / 命令 | `invoke('cmd', args)` → Rust command。**走 specta 生成的强类型 wrapper**，不裸调 `invoke` |
| 流式数据（LLM token、行情推送、scheduler 心跳） | Rust `app.emit(event, payload)` → 前端 `listen(event)` |
| 取消长任务 | Rust 持有 `CancellationToken`，前端通过另一 command 触发取消 |
| 业务状态（持仓、报价、订阅） | Rust 真源 → event 推送 → zustand 缓存 |
| UI 状态（panel 开关、滚动位置、表单输入） | 留 React |

**红线**：
- 前端**不持有业务真源**——任何业务计算（PnL / 估值 / 信号）必须由 Rust 算好推过来
- 前端**不直接发外部 HTTP**——所有 provider 调用走 Rust
- 视觉系统统一沿用 `docs/design/frontend-design.md`，不混入冲突的 design system

---

## Core Commands

```bash
npm install
npm run build                                       # frontend (tsc + vite)
npm run tauri dev                                   # 开发联调
npm run tauri build                                 # 打包
cargo check --manifest-path src-tauri/Cargo.toml    # rust check
cargo test  --manifest-path src-tauri/Cargo.toml    # rust tests
```

---

## Handoff Checklist

每次有意义的实现产出，agent 自检：

```bash
# 1. 依赖方向自检（任一非空都是 bug）
grep -rE "use crate::adapters"        src-tauri/src/{domain,infrastructure,pipeline}
grep -rE "use crate::pipeline"        src-tauri/src/{domain,infrastructure}
grep -rE "use crate::infrastructure"  src-tauri/src/domain
grep -rE "use (tauri|rusqlite|reqwest|byteorder|flate2)" src-tauri/src/domain

# 2. spec 锚点自检：新增 / 改动的模块文件顶部有没有 `// Spec: ...` 注释

# 3. drift 自检：列出"这次实现做了 spec 没说的事 / spec 说了但没做的事"，主动汇报

# 4. 门禁
npm run build && \
cargo check --manifest-path src-tauri/Cargo.toml && \
cargo test  --manifest-path src-tauri/Cargo.toml
```

---

## 工作流速查

| 场景 | 正确做法 | 禁止做法 |
|---|---|---|
| 用户要加新功能 | 先在 spec 里写清楚 → 再写代码 | 直接开始写代码 |
| 实现时发现 spec 有歧义 | 停下来汇报，等 spec 更新 | 自行解释 spec 并实现 |
| 觉得代码可以更优 | 不动；除非 spec 改了或这是非业务 refactor（命名 / 编译错） | 顺手"优化"业务逻辑 |
| 想新增"小工具函数" | 放 `domain/` 还是 `infrastructure/` 看是否纯计算；命名遵循 spec 词汇表 | 起飞自由命名 |
| 跨 BC 数据需求 | 检查 `architecture.md` 是否允许；走 adapter | 直接 import 另一个 BC 的 internal 模块 |

---

**本文件保持 ~150 行作为入口页。任何业务契约 / 领域模型 / 接口规约都不写在这里，写到 `docs/design/`。**
