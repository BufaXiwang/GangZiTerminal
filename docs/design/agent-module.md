# Agent 模块 Spec

> 本文档是 Agent bounded context 的领域模型契约。模块边界 / 依赖方向以 `docs/design/architecture.md` 为准。
>
> 本文档只描述最新设计目标，作为实现依据。

## 一句话定位

**自驱动投资学习 Agent**：Agent 通过 Quotes、News、Account 三个模块获取事实、执行模拟交易、观察结果并更新策略，目标是在模拟账户中持续迭代出更稳定、更高质量的交易策略。

Agent 是决策者和学习者。Quotes / News / Account 是数据与执行模块，不知道 Agent 存在。

---

## 1. 责任边界

Agent 负责：

- 与用户进行流式对话，支持用户询问、纠错、下达偏好或操作建议。
- 消费 News / Quotes / Account 数据，识别机会、风险和待跟踪标的。
- 决定是否加入自选、挂单、开仓、调仓、平仓、调整保护条件。
- 响应 Account 触发事件，如止损、止盈、订单成交、订单拒绝、时间止损。
- 维护策略、记忆、复盘、episode、工具调用审计。
- 管理上下文预算，必要时压缩历史、丢弃过期工具结果或派 Sub Agent。
- 将所有模型输出、工具调用、状态变化以流式事件展示给前端。

Agent 不负责：

- 维护行情 provider、K 线缓存、市场 universe。
- 维护新闻 provider、新闻正文抽取、新闻 DB。
- 计算账户现金、仓位市值、成本、盈亏、订单成交。
- 直接访问 Quotes / News / Account 内部实现。
- 真实券商交易。

---

## 2. 领域模型

### 核心概念

| 概念 | 含义 | 主键 / 身份 |
|---|---|---|
| `AgentRun` | 一次 Agent loop 执行 | `run_id` |
| `AgentEpisode` | 一次可审计决策 episode，可由 chat/news/account/schedule 触发 | `episode_id` |
| `AgentTurn` | 一个模型 turn，含 token、工具调用和 stop reason | `(run_id, turn_index)` |
| `AgentMessage` | 用户 / assistant / system 对话消息 | `message_id` |
| `AgentEvent` | 前端可见的流式运行事件 | `event_id` 或流内序号 |
| `ToolCall` | 模型请求调用工具的记录 | `tool_call_id` |
| `Strategy` | Agent 当前采用的可复用交易策略 | `strategy_id` |
| `MemoryItem` | 用户偏好、长期观察、反复有效/无效的经验 | `memory_id` |
| `Review` | 对交易、触发事件或策略表现的一次复盘 | `review_id` |
| `SubAgentRun` | 主 Agent 派出的受限子 Agent run | `run_id` |
| `ProviderChannel` | 模型渠道配置，适配不同 wire format | `channel_id` |

### 不变量

- Agent 可以消费 Quotes / News / Account 的对外契约，不能 import 三者内部实现细节。
- Agent 是唯一能通过工具触发 Account 交易写操作的模块。
- 所有 Agent run 必须流式输出 `AgentEvent`，前端可见运行过程。
- 所有工具调用必须在前端可见：开始、参数摘要、结束、耗时、错误状态。
- 所有交易动作必须通过 Account 的 `operate_account`，不能绕过 Account 直接写仓位。
- News / Account 触发的后台 run 也必须产生 episode，不能静默修改状态。
- Agent 的长期记忆和策略更新必须可审计：来源、证据、应用结果、更新时间。
- 过期行情、旧新闻和旧工具结果不能作为当前交易事实；需要最新事实时重新调用工具。
- Sub Agent 只做受限任务，默认只读；不能拥有交易写能力。

### `AgentRun`

```ts
type AgentRun = {
  runId: string;
  episodeId: string;
  trigger:
    | { kind: "user_chat"; messageId: string }
    | { kind: "news_batch"; newsIds: string[] }
    | { kind: "account_trigger"; triggerId: string }
    | { kind: "scheduled_review"; reason: string }
    | { kind: "manual_replay"; refId: string };
  provider: string;
  wireFormat: "messages" | "responses" | "chat_completions";
  model: string;
  status: "running" | "completed" | "failed" | "cancelled";
  startedAt: string;
  endedAt?: string;
};
```

规则：

- `run_id` 是流式事件、工具调用、episode turn 的关联键。
- 后台触发和用户对话使用同一套 Agent loop，只是 trigger 和 prompt 不同。
- 每个 run 必须有最终状态，即使模型或工具失败也要落失败原因。

### `AgentEvent`

```ts
type AgentEvent =
  | { type: "run_start"; runId: string; trigger: string; model: string }
  | { type: "text_delta"; runId: string; delta: string }
  | { type: "thinking_delta"; runId: string; delta: string }
  | { type: "tool_start"; runId: string; toolCallId: string; name: string; input: unknown }
  | { type: "tool_end"; runId: string; toolCallId: string; name: string; output: unknown; isError: boolean; durationMs: number }
  | { type: "subagent_start"; runId: string; subRunId: string; agentType: string; task: string }
  | { type: "subagent_end"; runId: string; subRunId: string; report: string; isError: boolean }
  | { type: "compacted"; runId: string; tier: "micro_clear" | "summarize" | "drop" | "reactive"; summary?: string }
  | { type: "usage"; runId: string; inputTokens: number; outputTokens: number; cacheReadTokens?: number; cacheWriteTokens?: number }
  | { type: "done"; runId: string; stopReason: string; turns: number }
  | { type: "error"; runId: string; message: string };
```

前端展示原则：

- 用户必须能看到 Agent 正在做什么：读取新闻、拉行情、查账户、下单、复盘、压缩上下文、派子 Agent。
- 工具输入可以摘要展示，但不能完全隐藏。
- 工具输出默认摘要展示；完整 payload 可在详情里展开。
- 后台 run 也要进入可查看的事件流 / episode 记录，不允许无声执行。

### `ProviderChannel`

```ts
type ProviderChannel = {
  channelId: string;
  provider: string;
  wireFormat: "messages" | "responses" | "chat_completions";
  baseUrl?: string;
  model: string;
  stream: true;
  supportsTools: boolean;
  supportsVision: boolean;
  supportsThinking: boolean;
  supportsServerSideSearch: boolean;
};
```

规则：

- Agent 内部使用 canonical `AgentRequest` / `AgentEvent`。
- Provider 只负责 canonical request 和厂商 wire format 的互转。
- 必须支持流式输出；不支持流式的 provider 不能作为主 Agent 渠道。
- 支持三类 wire format：
  - Anthropic `/messages`
  - OpenAI `/responses`
  - OpenAI-compatible `/chat/completions`
- provider 层不执行 tool loop，不管理上下文，不写业务状态。

### `Strategy`

```ts
type Strategy = {
  strategyId: string;
  name: string;
  thesis: string;
  entryRules: string[];
  exitRules: string[];
  riskRules: string[];
  applicableRegimes?: string[];
  status: "draft" | "active" | "paused" | "retired";
  metrics?: {
    trades: number;
    winRate?: number;
    avgReturn?: number;
    maxDrawdown?: number;
    profitFactor?: number;
  };
  createdAt: string;
  updatedAt: string;
};
```

规则：

- Strategy 是 Agent 自迭代的核心产物，不是单次聊天结论。
- 策略更新必须基于 episode / trade / review 证据。
- 高收益是优化目标，不是保证；评价必须同时看收益、回撤、胜率、样本数和可复现性。

### `MemoryItem`

```ts
type MemoryItem = {
  memoryId: string;
  kind: "user_preference" | "market_lesson" | "strategy_rule" | "risk_note";
  content: string;
  evidenceRefs: string[];
  status: "active" | "superseded" | "retired";
  confidence?: number;
  createdAt: string;
  updatedAt: string;
};
```

规则：

- 用户明确纠错、偏好、风险约束可以进入 memory。
- 单次偶然交易结果不能直接固化为长期策略。
- 被新证据推翻的 memory 不删除，标记为 `superseded` 或 `retired`。

---

## 3. 数据流

### 用户交互流

```text
UI chat input
  -> AgentRun(trigger=user_chat)
  -> build context from memory + account + quotes summary
  -> provider stream
  -> tool loop
  -> AgentEvent stream to UI
  -> persist messages / episode / tool calls / usage
```

规则：

- 用户可以问原因、要求复盘、提供偏好、请求建议。
- 用户可以要求 Agent 执行交易动作，但实际写入仍通过 Account `operate_account`。
- Agent 不需要用户审批才能行动；但行动过程必须可见、可审计。

### News 驱动分析流

```text
News refresh
  -> Agent claims unanalyzed news
  -> fetch_news
  -> identify related instruments
  -> fetch_quotes / fetch_account
  -> decide: no_action | add_watchlist | place_order | open/scale/close | adjust_protection
  -> persist episode + decisions
```

规则：

- News 只提供原始资讯和正文；关联股票、重要性、交易影响由 Agent 判断。
- 大多数新闻应是 `no_action` 或加入观察，不应强行交易。
- 若新闻影响已有仓位，优先评估是否调整保护条件、撤单、减仓或平仓。
- 若新闻涉及新标的，先判断是否加入自选，再判断是否挂单 / 开仓。

### Account 触发复盘流

```text
Account emits account-triggered
  -> AgentRun(trigger=account_trigger)
  -> fetch_account(trigger/position/order)
  -> fetch_quotes(ts_code)
  -> optional fetch_news / delegate
  -> decide response action
  -> operate_account if needed
  -> write review / update strategy or memory
```

规则：

- Account 只触发事件，不决定后续动作。
- Agent 必须处理止损、止盈、订单成交、订单拒绝、订单过期、时间止损等事件。
- 触发事件处理必须幂等；同一 `trigger_id` 不重复执行交易动作。
- 如果触发暴露策略缺陷，Agent 应生成 review，并考虑更新策略 / memory。

### 定时自迭代流

```text
scheduler tick
  -> AgentRun(trigger=scheduled_review)
  -> fetch_account + fetch_quotes + fetch_news
  -> review open positions / pending orders / watchlist
  -> compare strategy metrics
  -> propose strategy adjustments
  -> operate_account when action is justified
```

规则：

- 定时 run 用于复盘、巡检、策略更新和机会扫描。
- 定时 run 不是每次都必须交易。
- 关键输出是可审计的 episode：看了什么、判断什么、做了什么、为什么没做。

---

## 4. 对外接口

### 前端展示接口

前端需要两类 Agent command：

#### `send_agent_message`

```ts
type SendAgentMessageRequest = {
  content: string;
  images?: string[];
};

type SendAgentMessageResponse = {
  messageId: string;
  runId: string;
};
```

约束：

- command 只启动 run，实际内容通过 `agent-event` 流式返回。
- 支持图片输入；图片作为 message block 进入 AgentRequest。
- 同一时间同一用户会话默认只允许一个交互 run，避免多 run 争用上下文。

#### `fetch_agent_state`

```ts
type FetchAgentStateRequest = {
  include?: {
    messages?: boolean;
    episodes?: boolean;
    strategies?: boolean;
    memory?: boolean;
    toolCalls?: boolean;
    providerStatus?: boolean;
  };
  limit?: number;
  offset?: number;
};
```

用途：

- 加载聊天历史。
- 查看 Agent 最近 episode、工具调用、策略、记忆、运行成本。
- 支持前端展示类似“技能 / 内置技能 / 阅读文件 / 调用工具”的运行时间线。

### Agent 使用的模块工具

Agent 消费其他模块时，工具应收敛为少数高层工具。

| 工具 | 来源模块 | 能力 |
|---|---|---|
| `fetch_quotes` | Quotes | 行情、K 线、分时、指标、基本面、事件、扫描 |
| `fetch_news` | News | 新闻列表、全文、搜索、按标的过滤 |
| `fetch_account` | Account | 账户总览、仓位、订单、自选、事件、触发 |
| `operate_account` | Account | 挂单、撤单、开仓、调仓、平仓、调整保护条件、自选维护 |
| `delegate` | Agent | 派 Sub Agent 做受限研究 / 反方审查 |
| `compact_now` | Agent | 主动压缩上下文 |

规则：

- Agent 不直接使用碎片化模块接口。
- 交易写能力只存在于 `operate_account`。
- `delegate` 默认不能使用 `operate_account`。

### 内部 Rust API

内部 API 以 run orchestration 为主：

```rust
send_agent_message(request) -> SendAgentMessageResponse;
run_agent_from_news(news_ids) -> AgentRunResult;
run_agent_from_account_trigger(trigger_id) -> AgentRunResult;
run_scheduled_agent_review(reason) -> AgentRunResult;
build_agent_request(trigger, context) -> AgentRequest;
run_agent(provider, registry, request, context, event_tx) -> RunSummary;
compact_context(messages, budget) -> CompactResult;
```

---

## 5. 模块独有功能

### Agent Loop

```text
provider.stream(AgentRequest)
  -> text/thinking deltas
  -> assistant tool_use
  -> ToolRegistry dispatch
  -> tool_result appended as user message
  -> compact if needed
  -> next provider.stream
  -> final assistant message
```

约束：

- 每次 run 有最大 turn 数，防止无限工具循环。
- 工具调用有超时；超时作为 tool error 返回给模型，而不是阻塞 run。
- server-side tools 和 local tools 都要进入统一事件流。
- provider 返回 context too long 时，可以触发 reactive compact 后重试一次。

### 上下文管理

上下文由四类内容构成：

| 类型 | 内容 | 生命周期 |
|---|---|---|
| Identity / System | Agent 身份、运行纪律、工具规则 | 长期，适合 cache |
| Dynamic Context | 账户摘要、市场摘要、触发事件、用户当前输入 | 每次 run 重建 |
| Structured History | chat messages、tool_use/tool_result、episode 摘要 | 跨 run，需压缩 |
| Long-term Memory | 用户偏好、策略、复盘经验 | 独立存储，按需注入 |

压缩策略：

- `micro_clear`：清理过期工具结果，尤其是行情、K 线、新闻、搜索。
- `summarize`：用低成本模型把早期对话压成摘要边界。
- `drop`：仍超预算时丢弃最旧消息。
- `reactive`：provider 拒绝超长上下文时兜底压缩。

规则：

- 易腐工具结果不能长期保留为事实。
- 交易写工具结果不能随意清理到让 Agent 怀疑是否执行过；应保留操作确认或 episode 摘要。
- Agent 能感知当前 context 使用量，并可主动调用 `compact_now`。

### Sub Agent

Sub Agent 用于降低主上下文负担和减少确认偏误。

| 类型 | 作用 | 工具权限 |
|---|---|---|
| `researcher` | 深度研究标的 / 板块 / 主题，输出简报 | 只读 Quotes / News / Account |
| `bear_advocate` | 反方审查交易提案，找风险和反证 | 只读 Quotes / News |

规则：

- Sub Agent 不继承主 Agent 的交易写能力。
- Sub Agent 有独立上下文预算和最大工具调用限制。
- Sub Agent 的结论作为 `delegate` 工具结果返回主 Agent。
- 重要 Sub Agent 结果应进入 episode，可供前端展开查看。

### 自迭代评估

Agent 通过模拟账户结果更新策略。

评估维度：

- 收益率。
- 最大回撤。
- 胜率。
- 盈亏比。
- 持仓周期。
- 触发纪律：是否按计划止损 / 止盈 / 调仓。
- 新闻驱动交易的后验效果。
- 策略样本数是否足够。

规则：

- 单笔交易不能直接证明策略有效。
- 策略升级 / 降级 / 暂停必须写 review 和证据引用。
- 用户纠错优先进入 memory，再影响后续决策。

---

## 6. 验收标准 / 例子

- Agent 支持 `/messages`、`/responses`、`/chat/completions` 三类 wire format，并统一成 canonical `AgentRequest` / `AgentEvent`。
- 所有 provider 主通道必须支持 streaming；前端能看到 `run_start`、文本增量、工具开始 / 结束、压缩、usage、done / error。
- 用户发送消息后，UI 能实时看到 Agent 调用了 `fetch_quotes` / `fetch_news` / `fetch_account` / `operate_account` 等工具。
- News 新增后，Agent 可以批量分析新闻，识别相关股票，并决定 no_action / add_watchlist / operate_account。
- Account 发出 `account-triggered(stop_loss)` 后，Agent 会产生一次 episode，读取账户和行情，再决定是否平仓、减仓、调整保护条件或继续持有。
- Agent 交易写动作只通过 `operate_account`，不直接写 Account 表。
- Agent 能读取 Quotes、News、Account，但 Quotes / News / Account 不 import Agent 代码。
- 长对话接近上下文预算时，Agent 会触发 compact，并把 compact 事件展示到前端。
- Sub Agent 可以执行只读研究任务；Sub Agent 无法调用 `operate_account`。
- 每次 run 都有 episode 和 turn 记录，包含 token、工具调用、模型、provider、错误或 stop reason。
- 策略更新必须引用 episode / trade / news / account trigger 证据。

---

## 7. 模块边界外

这些能力不属于 Agent 模块：

- 行情 provider 接入。
- 新闻 provider 接入。
- 账户估值和成交模拟。
- 真券商交易。
- 对历史收益的保证。
- 绕过 Account 的直接仓位写入。
