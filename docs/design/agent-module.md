# Agent 模块 Spec

> 本文档是 Agent bounded context 的领域模型契约。模块边界 / 依赖方向以 `docs/design/architecture.md` 为准。
>
> 本文档只描述最新设计目标，作为实现依据。

## 一句话定位

**数据驱动、可自我迭代的投资专家 Agent**：Agent 从 News / Quotes / Account 获取事实，构造实时决策上下文，形成投资判断和操作建议，通过 Account 在模拟账户中验证，并把结果沉淀为可复盘、可迭代的策略经验。

Agent 的长期目标是成为围绕模拟账户持续学习的投资专家；第一阶段目标是先跑通稳定闭环：

```text
观察事实 -> 形成判断 -> 执行模拟动作 -> 监控触发 -> 复盘归因 -> 沉淀建议
```

策略是运行时动态注入的上下文，也是 Agent 自我迭代的承载物。第一阶段先保留策略调整入口和复盘建议，不自动让策略更新生效。

---

## 1. 责任边界

Agent 负责：

- 与用户进行流式对话，支持用户询问、纠错、表达风险偏好、请求建议。
- 消费 News / Quotes / Account 的对外契约，识别机会、风险和待跟踪标的。
- 基于实时决策上下文决定是否观察、加入自选、挂单、开仓、调仓、平仓或调整保护条件。
- 响应 Account 触发事件，如止损、止盈、订单成交、订单拒绝、订单过期、时间止损。
- 作为投资专家综合技术面、基本面、消息面、账户约束和历史复盘形成判断。
- 通过复盘沉淀策略经验，为后续 run 提供可注入的迭代上下文。
- 记录 `AgentRun`、`DecisionEpisode`、`ToolCall`、`AgentEvent`、`DecisionReview`。
- 将模型输出、工具调用、状态变化以流式事件展示给前端。
- 将策略卡、用户偏好、最近复盘等内容按需注入上下文。

Agent 不负责：

- 行情 provider、K 线缓存、市场 universe、指标计算。
- 新闻 provider、正文抽取、新闻去重、新闻本地读模型。
- 账户现金、持仓、市值、成本、盈亏、成交模拟。
- 直接修改 Account 状态；所有账户写动作必须调用 Account 对外能力。
- 自动保证策略收益。
- 真实券商交易。

边界规则：

- Quotes / News / Account 不知道 Agent 存在。
- Agent 是三个执行模块的消费者和决策者。
- Agent 不 import Quotes / News / Account 的内部实现，只通过工具层反腐译码。

---

## 2. 领域模型

### 核心概念

| 概念 | 含义 | 身份 |
|---|---|---|
| `AgentRun` | 一次 Agent loop 运行 | `run_id` |
| `DecisionEpisode` | 一次由用户、新闻、账户事件或定时任务触发的完整决策 | `episode_id` |
| `RealtimeDecisionPacket` | 每次 run 临时构造的事实上下文 | `run_id` |
| `AgentEvent` | 前端可见的流式事件 | `event_id` 或流内序号 |
| `AgentMessage` | 用户 / assistant / system 消息 | `message_id` |
| `ToolCall` | 一次工具调用审计 | `tool_call_id` |
| `TradeIntent` | Agent 希望 Account 执行的交易意图 | `intent_id` |
| `StrategyCard` | 动态注入到 Agent 上下文的策略卡 | `strategy_id` |
| `DecisionReview` | 对一次决策、交易或触发事件的复盘 | `review_id` |
| `ProviderChannel` | 模型渠道配置，适配不同 wire format | `channel_id` |

### 不变量

- 每次 Agent run 必须有 `run_id`、trigger、状态、开始时间和结束状态。
- 每次产生交易意图的 run 必须关联一个 `DecisionEpisode`。
- 所有交易写动作必须通过 Account 的 `operate_account`，不能直接写订单、持仓或现金。
- 行情、账户、新闻工具结果必须带时间戳；过期事实不能作为当前下单依据。
- 同一 Account trigger 必须幂等处理，不能重复触发交易动作。
- Agent 可以建议策略调整，但第一阶段不会自动改写 active `StrategyCard`。
- `StrategyCard` 是动态注入上下文的一部分；调整入口必须保留，但生效动作由显式更新流程完成。
- 所有工具调用必须在前端可见：开始、参数摘要、结束、耗时、错误状态。
- 后台 run 也必须产生事件流和 episode 记录，不能静默执行。

### `AgentRun`

```ts
type AgentRun = {
  runId: string;
  episodeId?: string;
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
  error?: string;
};
```

规则：

- `run_id` 是事件流、工具调用、模型 turn、episode 的关联键。
- 用户对话和后台触发共用同一套 loop，只是 trigger、上下文和最大预算不同。
- 运行失败也必须落状态和错误原因。

### `DecisionEpisode`

```ts
type DecisionEpisode = {
  episodeId: string;
  runId: string;
  triggerKind: "user_chat" | "news_batch" | "account_trigger" | "scheduled_review" | "manual_replay";
  symbols: string[];
  thesis: string;
  action:
    | "no_action"
    | "add_watchlist"
    | "place_order"
    | "open_position"
    | "adjust_position"
    | "close_position"
    | "adjust_protection"
    | "cancel_order";
  confidence?: number;
  riskPlan?: {
    maxPositionRatio?: number;
    stopLoss?: string;
    takeProfit?: string;
    invalidation?: string;
    reviewAfter?: string;
  };
  strategyIds: string[];
  evidenceRefs: string[];
  createdAt: string;
};
```

规则：

- Episode 是可复盘的最小决策单元，不等同于一条聊天消息。
- `thesis` 必须描述为什么做或为什么不做。
- 交易动作必须绑定 `strategyIds` 或明确说明是临时判断。
- `evidenceRefs` 引用当时使用的新闻、行情、账户事件、工具结果和策略卡。

### `RealtimeDecisionPacket`

```ts
type RealtimeDecisionPacket = {
  runId: string;
  trigger: AgentRun["trigger"];
  account?: {
    snapshotAt: string;
    cash: number;
    totalAssets: number;
    positions: unknown[];
    openOrders: unknown[];
    watchlist: unknown[];
    triggers?: unknown[];
  };
  quotes?: {
    snapshotAt: string;
    freshness: "fresh" | "stale" | "missing";
    items: unknown[];
    klines?: unknown[];
    indicators?: unknown[];
    scan?: unknown;
  };
  news?: {
    snapshotAt: string;
    items: unknown[];
  };
  strategies: StrategyCard[];
  recentEpisodes: unknown[];
  userPreferences: unknown[];
};
```

规则：

- 该对象是运行时投影，不是长期存储真源。
- 每次 run 重新构造，不能把旧工具结果当作实时事实复用。
- Chat 历史只作为交互上下文；交易判断主要依赖这个实时决策包。

### `StrategyCard`

```ts
type StrategyCard = {
  strategyId: string;
  name: string;
  description: string;
  status: "active" | "paused";
  config: {
    entryRules?: string[];
    exitRules?: string[];
    riskRules?: string[];
    factorWeights?: {
      technical?: number;
      fundamental?: number;
      news?: number;
    };
    applicableRegimes?: string[];
  };
  createdAt: string;
  updatedAt: string;
};
```

规则：

- `StrategyCard` 是 Agent 决策时动态注入的策略上下文。
- Agent 运行时读取 active strategy，不直接把复盘建议写回 active strategy。
- 策略卡可以被外部显式调整；调整后下一次 Agent run 自动使用新版本内容。
- 第一阶段不要求策略版本晋升、shadow run、自动调参、回测引擎。

### `DecisionReview`

```ts
type DecisionReview = {
  reviewId: string;
  episodeId: string;
  trigger:
    | "position_closed"
    | "stop_loss"
    | "take_profit"
    | "order_rejected"
    | "scheduled_review"
    | "manual_review";
  result?: {
    pnl?: number;
    pnlPct?: number;
    maxFavorableExcursion?: number;
    maxAdverseExcursion?: number;
    holdingDays?: number;
  };
  conclusion: string;
  suggestedChange?: {
    strategyId?: string;
    change: string;
    reason: string;
  };
  evidenceRefs: string[];
  createdAt: string;
};
```

规则：

- Review 只记录复盘和建议，不自动修改策略。
- 单次交易结果不能证明策略有效或无效。
- 建议必须能追溯到 episode、账户结果、行情或新闻证据。

### `AgentEvent`

```ts
type AgentEvent =
  | { type: "run_start"; runId: string; trigger: string; model: string }
  | { type: "text_delta"; runId: string; delta: string }
  | { type: "thinking_delta"; runId: string; delta: string }
  | { type: "tool_start"; runId: string; toolCallId: string; name: string; inputSummary: unknown }
  | { type: "tool_end"; runId: string; toolCallId: string; name: string; outputSummary: unknown; isError: boolean; durationMs: number }
  | { type: "episode_created"; runId: string; episodeId: string; action: string }
  | { type: "review_created"; runId: string; reviewId: string; episodeId: string }
  | { type: "usage"; runId: string; inputTokens: number; outputTokens: number; cacheReadTokens?: number; cacheWriteTokens?: number }
  | { type: "done"; runId: string; stopReason: string; turns: number }
  | { type: "error"; runId: string; message: string };
```

前端展示原则：

- 用户必须能看到 Agent 正在读取什么、调用什么工具、是否产生交易意图、是否执行账户动作。
- 工具输入和输出可以摘要展示，完整 payload 可进入详情。
- 后台触发的 run 也要进入时间线或 episode 列表。

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
};
```

规则：

- Agent 内部使用 canonical request / event。
- Provider 只负责 canonical request 和厂商 wire format 的互转。
- 必须支持流式输出；不支持 streaming 的 provider 不能作为主 Agent 渠道。
- 支持三类 wire format：Anthropic `/messages`、OpenAI `/responses`、OpenAI-compatible `/chat/completions`。
- Provider 层不执行 tool loop，不管理业务上下文，不写业务状态。

---

## 3. 数据流

### 用户交互流

```text
UI chat input
  -> AgentRun(trigger=user_chat)
  -> build RealtimeDecisionPacket when needed
  -> provider stream
  -> tool loop
  -> AgentEvent stream to UI
  -> persist messages / episode / tool calls / usage
```

规则：

- 用户可以要求解释、复盘、修改偏好、请求交易建议。
- 如果用户请求交易动作，Agent 仍需读取实时账户和行情，再生成 episode 和 trade intent。
- 交易写入必须经过 Account 校验。

### News 驱动分析流

```text
Runtime Orchestrator receives news-refreshed
  -> AgentRun(trigger=news_batch)
  -> fetch_news
  -> identify related instruments
  -> fetch_quotes / fetch_account
  -> inject relevant StrategyCard
  -> decide no_action / watchlist / order / position action
  -> persist DecisionEpisode
  -> operate_account only when justified
```

规则：

- Runtime Orchestrator 负责监听 `news-refreshed`、攒批、节流、幂等和启动 run。
- News 只提供事实；相关股票、重要性和交易影响由 Agent 判断。
- 大多数新闻应输出 `no_action` 或加入观察，不应强行交易。
- 若影响已有仓位，优先评估保护条件、撤单、减仓或平仓。

### Account 触发复盘流

```text
Runtime Orchestrator receives account-triggered
  -> AgentRun(trigger=account_trigger)
  -> fetch_account(trigger/position/order)
  -> fetch_quotes(symbols)
  -> optional fetch_news
  -> decide response action
  -> operate_account if needed
  -> write DecisionReview
  -> optionally record suggested strategy change
```

规则：

- Runtime Orchestrator 负责监听 `account-triggered`、按 `trigger_id` 去重和启动 run。
- Account 只触发事件，不决定 Agent 后续行为。
- Agent 必须处理止损、止盈、订单成交、订单拒绝、订单过期、时间止损等事件。
- 同一 `trigger_id` 的交易响应必须幂等。
- 复盘可以产生 `suggestedChange`，但不会自动更新 active strategy。

### 定时巡检流

```text
Runtime Orchestrator scheduled tick
  -> AgentRun(trigger=scheduled_review)
  -> fetch_account + fetch_quotes + fetch_news as needed
  -> review open positions / pending orders / watchlist
  -> create DecisionReview or DecisionEpisode
  -> operate_account only when action is justified
```

规则：

- 定时 run 用于巡检持仓、挂单、自选和最近 episode。
- 定时 run 不是每次都必须交易。
- 关键输出是可审计记录：看了什么、判断什么、做了什么、为什么没做。

### 第一阶段学习闭环

```text
DecisionEpisode
  -> Account result / trigger
  -> DecisionReview
  -> suggestedChange
  -> next run injects StrategyCard + relevant reviews
```

规则：

- 第一阶段学习闭环的核心是记录、复盘、建议，而不是自动改策略。
- 策略卡作为动态上下文入口保留，允许后续显式更新。
- Agent 后续决策可以读取相关 review，但旧 review 不能替代当前实时事实。

---

## 4. 对外接口

### 前端展示接口

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

- command 只启动 run，内容通过 `agent-event` 流式返回。
- 支持图片输入；图片作为 message block 进入 canonical request。
- 同一用户会话默认只允许一个前台交互 run。

#### `fetch_agent_state`

```ts
type FetchAgentStateRequest = {
  include?: {
    messages?: boolean;
    episodes?: boolean;
    reviews?: boolean;
    strategies?: boolean;
    toolCalls?: boolean;
    providerStatus?: boolean;
  };
  limit?: number;
  offset?: number;
};
```

用途：

- 加载聊天历史和最近运行状态。
- 查看 Agent episode、review、工具调用、策略卡、运行成本。
- 支持前端展示工具调用时间线和后台 run 记录。

#### `fetch_strategy_cards`

```ts
type FetchStrategyCardsRequest = {
  status?: "active" | "paused";
};
```

用途：

- 前端展示当前可注入策略。
- 用户或管理入口可以查看当前策略内容。

#### `upsert_strategy_card`

```ts
type UpsertStrategyCardRequest = {
  strategyId?: string;
  name: string;
  description: string;
  status: "active" | "paused";
  config: StrategyCard["config"];
};
```

用途：

- 显式创建或调整策略卡。
- 调整后的 active 策略卡从下一次 Agent run 开始注入。
- Agent 的 `DecisionReview.suggestedChange` 不会自动调用该接口。

### Agent 使用的模块工具

Agent 消费其他模块时，工具应收敛为少数高层工具。

| 工具 | 来源模块 | 能力 |
|---|---|---|
| `fetch_quotes` | Quotes | 行情、K 线、分时、指标、基本面、扫描 |
| `fetch_news` | News | 新闻列表、全文、搜索、按标的过滤 |
| `fetch_account` | Account | 账户总览、仓位、订单、自选、事件、触发 |
| `operate_account` | Account | 挂单、撤单、开仓、调仓、平仓、调整保护条件、自选维护 |

规则：

- Agent 不直接使用碎片化模块接口。
- 读工具可以并发；`operate_account` 必须串行执行。
- 交易写能力只存在于 `operate_account`。
- 工具返回值必须带时间戳和来源摘要，供 Agent 判断 freshness。

### 内部 Rust API

内部 API 以 run orchestration 为主：

```rust
send_agent_message(request) -> SendAgentMessageResponse;
run_agent_from_news(news_ids) -> AgentRunResult;
run_agent_from_account_trigger(trigger_id) -> AgentRunResult;
run_scheduled_agent_review(reason) -> AgentRunResult;
build_realtime_decision_packet(trigger) -> RealtimeDecisionPacket;
run_agent(provider, registry, request, context, event_tx) -> RunSummary;
record_decision_episode(run_id, episode) -> EpisodeId;
record_decision_review(episode_id, review) -> ReviewId;
```

---

## 5. 模块独有功能

### Agent Loop

```text
provider.stream(canonical request)
  -> text/thinking deltas
  -> assistant tool_use
  -> ToolRegistry dispatch
  -> tool_result appended
  -> continue or finalize
  -> persist run summary
```

约束：

- 每次 run 有最大 turn 数，防止无限工具循环。
- 工具有超时；超时作为 tool error 返回给模型。
- server-side tools 和 local tools 都进入统一事件流。
- provider 返回 context too long 时，可以触发一次压缩后重试。

### 上下文管理

上下文由四类内容构成：

| 类型 | 内容 | 生命周期 |
|---|---|---|
| Identity / System | Agent 身份、运行纪律、工具规则 | 长期，适合 cache |
| Realtime Packet | trigger、账户、行情、新闻、策略卡、近期 episode | 每次 run 重建 |
| Chat Context | 用户最近对话、当前问题 | 只服务交互 |
| Review / Memory | 用户偏好、复盘建议、策略说明 | 独立存储，按需注入 |

规则：

- 当前交易事实必须来自本次 `RealtimeDecisionPacket` 或本次工具调用。
- 易腐工具结果不能长期保留为事实。
- 交易写工具结果应保留操作确认或 episode 摘要。
- 长上下文压缩时优先丢弃旧行情、旧搜索、旧新闻全文等易腐内容。

### 策略动态注入

```text
active StrategyCard
  -> selected by trigger / symbol / market context
  -> injected into RealtimeDecisionPacket
  -> used by Agent to form thesis and risk plan
  -> review may produce suggestedChange
```

规则：

- 策略不是硬编码 prompt，也不是 Account 规则。
- 策略卡调整后，下一次 run 使用最新 active 内容。
- Agent 可以引用策略卡，也可以说明为什么本次不适用。
- 第一阶段只保留策略调整入口和建议记录，不实现自动生效。

### 账户动作纪律

所有交易写动作都必须产生：

- `DecisionEpisode`
- `TradeIntent`
- `operate_account` 工具调用
- Account 返回的执行结果

`TradeIntent` 至少包含：

```ts
type TradeIntent = {
  episodeId: string;
  action: "place_order" | "cancel_order" | "open_position" | "adjust_position" | "close_position" | "adjust_protection" | "update_watchlist";
  symbols: string[];
  reason: string;
  strategyIds: string[];
  riskPlan?: unknown;
};
```

规则：

- Agent 生成意图；Account 决定是否可执行。
- Account 拒绝后，Agent 只能记录或重新判断，不能绕过 Account。
- 行情 freshness 不足时，Agent 不应执行交易写动作。

---

## 6. 验收标准 / 例子

- Agent 支持 `/messages`、`/responses`、`/chat/completions` 三类 wire format，并统一成 canonical request / event。
- 所有 provider 主通道必须支持 streaming；前端能看到 `run_start`、文本增量、工具开始 / 结束、usage、done / error。
- 用户发送消息后，UI 能实时看到 Agent 读取 Quotes / News / Account 或执行 Account 动作。
- News 新增后，Agent 可以批量分析新闻，识别相关股票，并决定 `no_action` / `add_watchlist` / `operate_account`。
- Account 发出止损、止盈、订单成交、订单拒绝等触发后，Agent 会产生一次 run，读取账户和行情，再决定后续动作。
- 每次交易写动作都有 `DecisionEpisode`、`TradeIntent` 和 `operate_account` 调用记录。
- 每次止损、止盈、平仓或定时复盘可以生成 `DecisionReview`。
- `DecisionReview` 可以包含策略调整建议，但不会自动修改 active `StrategyCard`。
- 策略卡可以作为动态上下文注入；通过 `upsert_strategy_card` 调整 active 策略卡后，下一次 run 使用最新内容。
- Agent 能读取 Quotes、News、Account，但 Quotes / News / Account 不 import Agent 代码。

---

## 7. 不纳入范围

这些能力不属于 Agent 模块：

- 行情 provider 接入。
- 新闻 provider 接入。
- 账户估值、成交模拟、T+1 和现金校验。
- 真券商交易。
- 自动保证收益。
- 绕过 Account 的直接仓位写入。

这些能力不作为第一阶段目标：

- 自动策略晋升。
- 自动调参。
- 回测引擎。
- 多策略 shadow run。
- Sub Agent 协同投研。
- 自动修改 prompt / skill。
