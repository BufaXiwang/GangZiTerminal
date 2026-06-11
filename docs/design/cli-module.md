# CLI 模块 Spec

> 本文档定义 `gangzi` 命令行的能力边界：它是**只读**、连到正在运行的 app 的**瘦客户端**，给**人 / 外部脚本**用的旁路。它**不是** Agent 的工具通道（Agent 用进程内 tool，见 [agent-runtime-module.md](agent-runtime-module.md)）。
>
> 本文档只描述最新设计目标，作为实现依据。

## 一句话定位

**`gangzi` CLI 是 app 的只读查询旁路**：把行情 / 资讯 / 账户的查询能力以命令行暴露出去，方便人在终端、或外部脚本拿数据。它本身**不持有业务逻辑、不连 provider、不开 SQLite、不连 TDX**——只把请求转发给正在运行的 GangZi app，再打印结果。

```text
gangzi quote 600519.SH
  -> 连本地端点 (127.0.0.1 / unix socket)
  -> app 内同一套 pipeline 服务（共享暖缓存 / 实时报价 / 单写 SQLite）
  -> 返回 JSON
  -> CLI 打印
```

契约强度：

- 命令集（subcommand 名 + 只读语义 + 输出 schema 引用）是 `Spec-as-source`。
- 本地端点形态（unix socket vs 127.0.0.1 HTTP）、序列化细节、输出美化是 `Spec-anchored`（实现细节）。
- 本模块**可选 / 后续**：不是 Phase 3 必需；Phase 3 先有进程内 tool，CLI 之后按需补。

---

## 1. 责任边界

CLI 负责：

- 暴露**只读**查询子命令：行情、扫描、资讯、账户快照。
- 作为**瘦客户端**把请求转发给运行中的 app 的本地端点。
- 以稳定的 JSON（默认）或人类可读格式打印结果，供 shell 组合 / 脚本消费。
- app 未运行 / 端点不可达时，给出清晰错误并以非零退出码退出。

CLI 不负责：

- **任何写操作**：下单 / 撤单 / 调仓 / 改自选 / 记录判断 —— 一律不提供（见 §3 只读边界）。
- 自己连 TDX / 腾讯 / TuShare / EM，或自己开 SQLite —— 必须走 app（见 §2 瘦客户端）。
- 业务计算（估值 / 信号 / PnL）—— 由 app 算好返回。
- 作为 Agent 取数通道 —— Agent 用进程内领域 tool，不经 CLI（见 §4）。

边界规则：

- CLI 与 Agent tool 是**两条独立路径**，互不依赖。
- CLI 输出的数据**和 GUI 看到的一致**（同一份 app 状态），不是另起一个冷实例的独立视图。

---

## 2. 瘦客户端机制

- app 启动时开一个**本地只读端点**（实现选 `127.0.0.1:<port>` HTTP 或 unix domain socket；只绑本机，不对外）。端点背后是 app 内**同一套 pipeline 服务实例**（共享 `QuotesService` 暖缓存、实时报价、scheduler、单写 SQLite）。
- `gangzi <cmd>` 把命令解析成一个只读请求，发给端点，收到 JSON 后打印。**CLI 进程内无任何业务逻辑 / I/O 到外部数据源**。
- **为什么必须瘦客户端、不冷起独立进程**：独立进程会①重新冷连 TDX（秒级握手、绕开暖缓存）②和 GUI app **抢同一个单写 SQLite**（锁冲突 / 脏读风险）③看不到 app 的实时缓存 / 订阅集。瘦客户端把这些都交给唯一的 app 实例，单一真源、零冲突。
- app 未运行 → 端点连不上 → CLI 报 `app_not_running` 并非零退出（不 fallback 去开 DB / 连 provider）。

### 实现注记（2026-06-11 已落地）

- **端点形态**：`127.0.0.1:0` 随机端口（只绑本机），端口写 `<appData>/cli.port`；CLI 经
  `$GANGZI_CLI_PORT`（显式覆盖）或 portfile 发现。app 退出后 portfile 残留无害（连不上即
  `app_not_running`）。手写最小 HTTP/1.1（仅 `POST /v1/{quotes,news,account}` + `GET /v1/health`，
  `Connection: close`），零新依赖。
- **同源复用**：三条路由直接挂 agent_runtime 的 `QuotesGateway/NewsGateway/AccountGateway::fetch`
  ——与 Agent 读 tool（fetch_quotes/fetch_news/fetch_account）**同一实现**，§1「与 GUI 一致」与
  验收「与 Agent 读 tool 同源」按构造满足。写方法（operate/update_watchlist）无路由，CLI 层不可达。
- **代码位置**：app 侧 `adapters/cli/`（mod=端点+dispatch、args=参数解析、render=table 渲染，
  均 hermetic 测试）；CLI 二进制 `src-tauri/src/bin/gangzi.rs`（std TcpStream，进程内无业务 I/O）。
- **退出码**：0 成功 · 1 app 侧业务错误（透传 ErrorCode）· 2 参数非法 · 3 `app_not_running`。

---

## 3. 命令集（全部只读）

| 子命令 | 作用 | 输出（引用现有 DTO） |
|---|---|---|
| `gangzi quote <ts_code>...` | 取一/多只标的实时报价快照 | `StockQuote` + freshness（[quotes-module.md](quotes-module.md)）|
| `gangzi scan [筛选项]` | 市场扫描 / 列表（涨跌幅、市场宽度等）| scan 读模型（[quotes-module.md#scan_market](quotes-module.md)）|
| `gangzi kline <ts_code> [--period day]` | 取 K 线序列（只读已落库 + facade）| `KlineSeries`（[quotes-module.md](quotes-module.md)）|
| `gangzi news [--query ...] [--source ...]` | 查资讯（FTS / 来源过滤）| `FetchNewsResponse`（[news-module.md](news-module.md)）|
| `gangzi account [--positions] [--orders]` | 模拟账户快照 / 持仓 / 挂单（只读）| `AccountSnapshot` 等（[account-module.md](account-module.md)）|

规则：

- 所有子命令对应 app 侧已有的**查询 use-case**（与 Tauri command / Agent 读 tool 复用同一 pipeline 方法），CLI 不新增业务路径。
- 新增子命令必须先在本表登记，且**只能映射只读 use-case**。
- 默认输出 JSON（`--format json`），可选 `--format table` 给人看。

---

## 4. 与 Agent 的关系

- **Agent 不通过 CLI 取数**：Agent 用进程内领域 tool（`fetch_quotes` / `fetch_news` / `fetch_account`，见 [agent-infra-module.md](agent-infra-module.md) §3.6 `AgentToolName`），结构化、快、共享活状态。
- 让 Agent 走 `run_bash → gangzi` 是反模式（多一层进程 + 文本解析），**禁止**。CLI 纯粹是给人 / 外部脚本的旁路。
- 写操作只在 Agent / GUI 的进程内结构化 tool（带校验 + 审计）；CLI 只读是**有意的安全边界**——把危险的写留在结构化、可审计的通道。

---

## 5. 错误与退出码

| 情况 | 错误 | 退出码 |
|---|---|---|
| app 未运行 / 端点不可达 | `app_not_running` | 非 0 |
| 参数非法（ts_code 格式等）| `invalid_input` | 非 0 |
| app 侧返回业务错误 | 透传 app 的 `ErrorCode` + message | 非 0 |
| 成功 | — | 0 |

- 错误也以 JSON（`{"error":{"code","message"}}`）打印到 stderr，便于脚本解析。

---

## 验收标准

- CLI 任一子命令的输出**可由 app 侧对应 use-case 的 DTO 反序列化**，与 GUI / Agent 读 tool 同源。
- CLI **不含任何写子命令**；尝试任何写操作在 CLI 层即不可达。
- CLI 不直接打开 SQLite、不直接连任何行情 / 资讯 provider。
- app 未运行时 CLI 优雅报错并非零退出，不冷起独立数据路径。
- 与 Agent tool 路径完全解耦：移除 CLI 不影响 Agent 运行。
