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

契约强度：

- `AgentRun`、`AgentMessage`、`ToolCall`、`DecisionEpisode`、`EvidenceRef`、`TradeIntent`、`StrategyCard`、`DecisionReview`、Agent events、Agent tool schema 是 `Spec-as-source`。
- provider wire-format mapping、上下文压缩顺序、策略注入规则是 `Spec-anchored`。
- 自动调参、自动策略晋升、多 agent 协作不属于第一阶段。

共享类型见 [shared-types.md](shared-types.md)。

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
| `DecisionEpisode` | 一次可复盘的投资判断记录，可由用户、新闻、账户事件或定时任务触发 | `episode_id` |
| `AgentEvent` | 前端可见的流式事件 | `event_id` 或流内序号 |
| `AgentMessage` | 用户 / assistant / system 消息 | `message_id` |
| `ToolCall` | 一次工具调用审计 | `tool_call_id` |
| `TradeIntent` | `operate_account` 调用的持久化意图 / 审计快照 | `intent_id` |
| `StrategyCard` | 动态注入到 Agent 上下文的策略卡 | `strategy_id` |
| `DecisionReview` | 对一次决策、交易或触发事件的复盘 | `review_id` |
| `ProviderChannel` | 模型渠道配置，适配不同 wire format | `channel_id` |

### 不变量

- 每次 Agent run 必须有 `run_id`、trigger、状态、开始时间和结束状态。
- 一个 Agent run 可以产生 0..N 个 `DecisionEpisode`；纯聊天 / 设置解释 / 普通知识问答可以只有 run，不产生 episode。
- 只要 Agent 对标的、组合、新闻或账户触发形成投资判断，就必须产生 `DecisionEpisode`，即使结论是 `no_action`。
- 每次产生 `TradeIntent` / `operate_account` 调用的 run 必须先关联一个 `DecisionEpisode`。
- 所有交易写动作必须通过 Account 的 `operate_account`，不能直接写订单、持仓或现金。
- 行情、账户、新闻工具结果必须带时间戳；过期事实不能作为当前下单依据。
- 同一 Account trigger 必须幂等处理，不能重复触发交易动作。
- Agent 可以建议策略调整，但第一阶段不会自动改写 active `StrategyCard`。
- `StrategyCard` 是动态注入上下文的一部分；调整入口必须保留，但生效动作由显式更新流程完成。
- 所有工具调用必须在前端可见：开始、参数摘要、结束、耗时、错误状态。
- 后台 run 也必须产生事件流；如果形成投资判断，必须产生 episode，不能静默消费触发事件。

### `AgentRun`

```ts
type AgentRun = {
  runId: string;
  episodeIds?: string[];
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
  startedAt: OccurredAt;
  endedAt?: OccurredAt;
  error?: string;
};
```

规则：

- `run_id` 是事件流、工具调用、模型 turn、episode 的关联键。
- `episodeIds` 是该 run 产生的投资判断集合；不是所有 run 都有 episode。
- 用户对话和后台触发共用同一套 loop，只是 trigger、上下文和最大预算不同。
- 运行失败也必须落状态和错误原因。

### `DecisionEpisode`

```ts
type DecisionEpisode = {
  episodeId: string;
  runId: string;
  triggerKind: "user_chat" | "news_batch" | "account_trigger" | "scheduled_review" | "manual_replay";
  symbols: TsCode[];
  thesis: string;
  action:
    | "no_action"
    | "add_watchlist"
    | "remove_watchlist"
    | "place_order"
    | "cancel_order"
    | "open_position"
    | "scale_position"
    | "adjust_position"
    | "close_position"
    | "adjust_protection";
  confidence?: number; // 0..1
  riskPlan?: {
    maxPositionRatio?: Ratio;
    stopLoss?: Price;
    takeProfit?: Price;
    invalidation?: string;
    reviewAfter?: OccurredAt;
  };
  strategyIds: string[];
  evidenceRefs: EvidenceRef[];
  createdAt: OccurredAt;
};
```

规则：

- Episode 是可复盘的最小决策单元，不等同于一条聊天消息。
- Episode 只属于 Agent 决策域；News / Quotes / Account 不创建也不持有 `DecisionEpisode`。
- `no_action` 也是有效 episode，用于记录“为什么不行动”，避免新闻或账户触发被静默消费。
- `DecisionEpisode.action` 是决策摘要分类，面向复盘和前端时间线；`OperateAccountInput.action` 是 Account 可执行命令。
- `DecisionEpisode.action` 是 `OperateAccountInput.action` 的上层超集，可包含 `no_action`、观察、自选维护或更粗粒度的 `adjust_position`。
- 一个 episode 不一定产生账户写动作；只有需要写 Account 时，才生成 `TradeIntent.accountInput`。
- `thesis` 必须描述为什么做或为什么不做。
- `symbols` 必须使用标准 `TsCode`，不能保存自由股票名或 6 位代码。
- `confidence` 取值范围为 0..1；缺失表示模型未给出可审计置信度，不等于 0。
- 交易动作必须绑定 `strategyIds` 或明确说明是临时判断。
- `evidenceRefs` 引用当时使用的新闻、行情、账户事件、工具结果和策略卡，并携带最小快照，避免上游清理或字段变化后无法复盘。

### `AgentMessage`

```ts
type AgentMessageRole = "system" | "user" | "assistant" | "tool";

type AgentMessageBlock =
  | { type: "text"; text: string }
  | { type: "image"; mimeType: string; dataRef: string }
  | { type: "thinking"; text: string; provider?: string }
  | { type: "tool_use"; toolCallId: string; name: string; inputSummary: JsonSummary }
  | { type: "tool_result"; toolCallId: string; outputSummary: JsonSummary; isError: boolean };

type AgentMessage = {
  messageId: string;
  runId?: string;
  role: AgentMessageRole;
  blocks: AgentMessageBlock[];
  createdAt: OccurredAt;
};
```

规则：

- `AgentMessage` 是对话和 provider 上下文的持久化视图，不是投资判断。
- 图片使用 `dataRef` 指向本地附件或缓存，不把大二进制直接塞进长期消息表。
- thinking 是否持久化取决于 provider 支持和配置；跨 provider 不保证恢复。
- 工具调用审计以 `ToolCall` 为准，message block 只保存对话上下文所需摘要。

### `ToolCall`

```ts
type ToolCall = {
  toolCallId: string;
  runId: string;
  episodeId?: string;
  name: "fetch_quotes" | "fetch_news" | "fetch_account" | "operate_account" | "update_watchlist" | string;
  inputSummary: JsonSummary;
  outputSummary?: JsonSummary;
  isError: boolean;
  errorCode?: ErrorCode;
  startedAt: OccurredAt;
  endedAt?: OccurredAt;
  durationMs?: number;
};
```

规则：

- 所有 local tool 和 server-side tool 都必须记录 `ToolCall`。
- `operate_account` / `update_watchlist` 成功或拒绝都必须记录；拒绝时 `isError` 可为 false，执行结果由 Account response 表达。
- `episodeId` 只在该工具调用支撑某个投资判断时填写。
- 完整 payload 可以进入详情表或日志，但 `inputSummary` / `outputSummary` 必须可前端展示。

### `EvidenceRef`

```ts
type EvidenceRef =
  | { kind: "news"; id: string; snapshot: EvidenceNewsSnapshot }
  | { kind: "quote"; id: string; snapshot: EvidenceQuoteSnapshot }
  | { kind: "account_snapshot"; id: string; snapshot: EvidenceAccountSnapshot }
  | { kind: "position"; id: string; snapshot: EvidencePositionSnapshot }
  | { kind: "order"; id: string; snapshot: EvidenceOrderSnapshot }
  | { kind: "account_trigger"; id: string; snapshot: EvidenceAccountTriggerSnapshot }
  | { kind: "strategy"; id: string; snapshot: EvidenceStrategySnapshot }
  | { kind: "tool_call"; id: string; snapshot: ToolCallEvidenceSnapshot };

type EvidenceSnapshotBase = {
  schemaVersion: 1;
  capturedAt: OccurredAt;
  source?: string;
  freshness?: Freshness;
};

type EvidenceNewsSnapshot = EvidenceSnapshotBase & {
  newsId: string;
  title: string;
  summary?: string;
  url?: string;
  publishedAt?: OccurredAt;
  articleExcerpt?: string;
};

type EvidenceQuoteSnapshot = EvidenceSnapshotBase & {
  tsCode: TsCode;
  name?: string;
  price?: Price;
  changePercent?: Percent;
  volume?: Volume;
  amount?: Amount;
  peTtm?: number;
  pb?: number;
};

type EvidenceAccountSnapshot = EvidenceSnapshotBase & {
  cash: Money;
  totalAssets: Money;
  marketValue: Money;
  totalPnl: Money;
  openPositionCount: number;
  pendingOrderCount: number;
};

type EvidencePositionSnapshot = EvidenceSnapshotBase & {
  positionId: string;
  tsCode: TsCode;
  quantity: Shares;
  sellableQuantity: Shares;
  avgCost: Price;
  marketPrice?: Price;
  unrealizedPnl?: Money;
  protection?: { stopLoss?: Price; takeProfit?: Price; timeStopAt?: OccurredAt; enabled: boolean };
};

type EvidenceOrderSnapshot = EvidenceSnapshotBase & {
  orderId: string;
  tsCode: TsCode;
  side: "buy" | "sell";
  orderType: "market" | "limit";
  limitPrice?: Price;
  quantity: Shares;
  filledQuantity: Shares;
  status: string;
};

type EvidenceAccountTriggerSnapshot = EvidenceSnapshotBase & {
  triggerId: string;
  triggerType: string;
  tsCode?: TsCode;
  positionId?: string;
  orderId?: string;
  occurredAt: OccurredAt;
};

type EvidenceStrategySnapshot = EvidenceSnapshotBase & {
  strategyId: string;
  name: string;
  description: string;
  status: "active" | "paused";
  configSummary: string;
};

type ToolCallEvidenceSnapshot = {
  schemaVersion: 1;
  name: string;
  inputSummary: string;
  outputSummary?: string;
  isError: boolean;
  capturedAt: OccurredAt;
};
```

规则：

- `id` 用于跳转和重新查询；`snapshot` 才是 episode / review 的长期证据。
- Evidence snapshot 是持久化审计 schema，不直接存 `Packet*` 运行时投影；写入时由当前 packet / tool result 映射成 `Evidence*Snapshot`。
- 每个 snapshot 必须带 `schemaVersion`。同一 version 只能新增可选字段，不能重命名或删除已有字段；破坏性变更必须 bump version 并保留旧 version 反序列化。
- 被 episode 引用的重要新闻必须保存 `EvidenceNewsSnapshot`，避免源内容变更、远端不可用或后续正文重抽取影响复盘。
- 快照应是最小可复盘视图，不保存无限长正文；长正文使用摘要或 excerpt。
- 行情、账户、工具调用证据也保存当时的 source / freshness / timestamp。

### `StrategyCard`

```ts
type StrategyCard = {
  strategyId: string;
  version: number;
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
  createdAt: OccurredAt;
  updatedAt: OccurredAt;
};
```

规则：

- `StrategyCard` 是 Agent 决策时动态注入的策略上下文。
- 首次启动若没有 active strategy，系统必须 seed 一个内置 active baseline 策略卡（例如 `baseline_a_share_risk_control`），覆盖仓位上限、freshness、止损和不确定时不交易等基础纪律。
- 如果策略表为空或 seed 失败，Agent 仍可回答和读取数据，但交易写动作默认禁用，直到至少一个 active strategy 可注入。
- Agent 运行时读取 active strategy，不直接把复盘建议写回 active strategy。
- 策略卡可以被外部显式调整；调整后下一次 Agent run 自动使用新版本内容。
- 每次策略卡调整必须递增 `version`，并保留旧版本可追溯。
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
    pnl?: Money;
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
  evidenceRefs: EvidenceRef[];
  createdAt: OccurredAt;
};
```

规则：

- Review 只记录复盘和建议，不自动修改策略。
- 单次交易结果不能证明策略有效或无效。
- 建议必须能追溯到 episode、账户结果、行情或新闻证据；证据必须带最小快照，不能只保存易失 ID。

### `AgentEvent`

```ts
type JsonSummary =
  | string
  | number
  | boolean
  | null
  | JsonSummary[]
  | { [key: string]: JsonSummary };

type AgentEvent =
  | { type: "run_start"; runId: string; trigger: string; model: string }
  | { type: "text_delta"; runId: string; delta: string }
  | { type: "thinking_delta"; runId: string; delta: string }
  | { type: "tool_start"; runId: string; toolCallId: string; name: string; inputSummary: JsonSummary }
  | { type: "tool_end"; runId: string; toolCallId: string; name: string; outputSummary: JsonSummary; isError: boolean; durationMs: number }
  | { type: "episode_created"; runId: string; episodeId: string; action: string }
  | { type: "review_created"; runId: string; reviewId: string; episodeId: string }
  | { type: "compacted"; runId: string; tier: "micro_clear" | "summarize" | "drop" | "reactive_retry"; droppedMessages: number; estimatedTokensSaved?: number }
  | { type: "usage"; runId: string; inputTokens: number; outputTokens: number; cacheReadTokens?: number; cacheWriteTokens?: number }
  | { type: "done"; runId: string; stopReason: string; turns: number }
  | { type: "error"; runId: string; message: string };
```

前端展示原则：

- 用户必须能看到 Agent 正在读取什么、调用什么工具、是否产生交易意图、是否执行账户动作。
- 工具输入和输出可以摘要展示，完整 payload 可进入详情。
- 后台触发的 run 也要进入时间线或 episode 列表。

### `ProviderChannel`

Channel reference：

- [Anthropic Messages](references/agent/anthropic-messages.md)
- [OpenAI Responses](references/agent/openai-responses.md)
- [OpenAI Chat Completions](references/agent/openai-chat-completions.md)

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
- 本 spec 定义 Agent canonical loop、事件和工具契约；具体 stream event、tool call、thinking、web_search 和错误映射写在 channel reference。
- 主交易 Agent 渠道必须同时支持 streaming 和 local tools；不支持 tools 的渠道只能用于非交易问答或禁用。

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
  -> persist messages / optional DecisionEpisode / tool calls / usage
```

规则：

- 用户可以要求解释、复盘、修改偏好、请求交易建议。
- 纯聊天、设置解释、普通问答可以只落 `AgentRun`、messages、tool calls 和 usage，不产生 episode。
- 如果用户请求投资判断或交易动作，Agent 仍需读取实时账户和行情，再生成 `DecisionEpisode`；需要写 Account 时再生成 `TradeIntent`。
- 交易写入必须经过 Account 校验。

### News 驱动分析流

```text
AgentRun(trigger=news_batch)
  -> fetch_news
  -> identify related instruments
  -> fetch_quotes / fetch_account
  -> inject relevant StrategyCard
  -> decide no_action / watchlist / order / position action
  -> Agent records DecisionEpisode when it forms an investment judgment
  -> operate_account only when justified
```

规则：

- News 只提供事实；相关股票、重要性和交易影响由 Agent 判断。
- `DecisionEpisode` 由 Agent 持久化；News 不创建、不更新、不持有 episode。
- News 触发的 run 如果完成了影响判断，即使结论是 `no_action`，也必须记录一个 episode。
- 大多数新闻应输出 `no_action` 或加入观察，不应强行交易。
- 若影响已有仓位，优先评估保护条件、撤单、减仓或平仓。

### Account 触发复盘流

```text
AgentRun(trigger=account_trigger)
  -> fetch_account(trigger/position/order)
  -> fetch_quotes(symbols)
  -> optional fetch_news
  -> decide response action
  -> Agent records DecisionEpisode for the response judgment
  -> operate_account if needed
  -> write DecisionReview
  -> optionally record suggested strategy change
```

规则：

- Account 只触发事件，不决定 Agent 后续行为。
- Agent 必须处理止损、止盈、订单成交、订单拒绝、订单过期、时间止损等事件。
- 同一 `trigger_id` 的交易响应必须幂等。
- Agent 必须持久化 `trigger_id -> run_id / episode_id / trade_intent_id` 映射；已完成映射不得再次产生交易写动作。
- Account 触发的 run 完成响应判断后必须记录 episode；选择继续持有或不操作也是 `no_action` episode。
- 复盘可以产生 `suggestedChange`，但不会自动更新 active strategy。

### 定时巡检流

```text
AgentRun(trigger=scheduled_review)
  -> fetch_account + fetch_quotes + fetch_news as needed
  -> review open positions / pending orders / watchlist
  -> create DecisionReview and/or DecisionEpisode when a judgment is formed
  -> operate_account only when action is justified
```

规则：

- 定时 run 用于巡检持仓、挂单、自选和最近 episode。
- 定时 run 不是每次都必须交易。
- 纯健康检查可以只落 run summary；如果对标的、仓位或组合形成判断，则必须记录 episode。
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
  baseVersion?: number;
  name: string;
  description: string;
  status: "active" | "paused";
  config: StrategyCard["config"];
  reason: string;
};
```

用途：

- 显式创建或调整策略卡。
- 调整后的 active 策略卡从下一次 Agent run 开始注入。
- Agent 的 `DecisionReview.suggestedChange` 不会自动调用该接口。
- `baseVersion` 用于乐观并发；版本冲突必须拒绝。
- `reason` 进入策略审计记录。

### Agent 使用的模块工具

Agent 消费其他模块时，工具应收敛为少数高层工具。

| 工具 | 来源模块 | 能力 |
|---|---|---|
| `fetch_quotes` | Quotes | 行情、K 线、分时、指标、基本面、扫描 |
| `fetch_news` | News | 新闻列表、全文、关键词搜索 |
| `fetch_account` | Account | 账户总览、仓位、订单、自选、事件、触发 |
| `operate_account` | Account | 挂单、撤单、开仓、调仓、平仓、调整保护条件 |
| `update_watchlist` | Account | 添加 / 删除自选、更新自选备注 |

规则：

- Agent 不直接使用碎片化模块接口。
- 读工具可以并发；`operate_account` 必须串行执行。
- 交易写能力只存在于 `operate_account`。
- `update_watchlist` 是非交易写能力，可由 Agent 用于维护观察列表。
- 工具返回值必须带时间戳和来源摘要，供 Agent 判断 freshness。
- Agent 工具 schema 属于 Agent 模块；执行模块只需要提供自己的领域接口。

工具 schema 使用 Agent 自己的 packet 视图：

```ts
type RealtimeDecisionPacket = {
  runId: string;
  trigger: AgentRun["trigger"];
  account?: PacketAccount;
  quotes?: PacketQuotes;
  news?: PacketNews;
  strategies: StrategyCard[];
  recentEpisodes: PacketEpisodeSummary[];
  userPreferences: PacketUserPreference[];
};

type PacketAccount = {
  snapshot: PacketAccountSnapshot;
  positions: PacketPosition[];
  openOrders: PacketOrder[];
  watchlist: PacketWatchlistItem[];
  triggers: PacketAccountTrigger[];
  freshness: Freshness;
  warnings?: WarningCode[];
};

type PacketAccountSnapshot = {
  capturedAt: OccurredAt;
  cash: Money;
  availableCash: Money;
  frozenCash: Money;
  marketValue: Money;
  totalAssets: Money;
  realizedPnl: Money;
  unrealizedPnl: Money;
  totalPnl: Money;
  pricedPositionCount: number;
  unpricedPositionCount: number;
  valuationFreshness: Freshness;
  openPositionCount: number;
  pendingOrderCount: number;
  warnings?: WarningCode[];
};

type PacketPosition = {
  positionId: string;
  tsCode: TsCode;
  name: string;
  status: "open" | "closed";
  quantity: Shares;
  sellableQuantity: Shares;
  avgCost: Price;
  marketPrice?: Price;
  marketValue?: Money;
  quoteFreshness?: Freshness;
  unrealizedPnl?: Money;
  realizedPnl: Money;
  protection?: {
    stopLoss?: Price;
    takeProfit?: Price;
    timeStopAt?: OccurredAt;
    enabled: boolean;
  };
  openedAt: OccurredAt;
  closedAt?: OccurredAt;
  warnings?: WarningCode[];
};

type PacketOrder = {
  orderId: string;
  tsCode: TsCode;
  side: "buy" | "sell";
  orderType: "market" | "limit";
  limitPrice?: Price;
  quantity: Shares;
  filledQuantity: Shares;
  status: "pending" | "partially_filled" | "filled" | "cancelled" | "rejected" | "expired";
  intent: string;
  positionId?: string;
  createdAt: OccurredAt;
  expiresAt?: OccurredAt;
};

type PacketWatchlistItem = {
  tsCode: TsCode;
  name?: string;
  addedAt: OccurredAt;
  note?: string;
  quote?: PacketQuoteItem;
};

type PacketAccountTrigger = {
  triggerId: string;
  triggerType: "stop_loss" | "take_profit" | "time_stop" | "order_filled" | "order_rejected" | "order_expired" | "invalidated";
  tsCode?: TsCode;
  positionId?: string;
  orderId?: string;
  price?: Price;
  threshold?: Price | OccurredAt | string;
  quoteFreshness?: Freshness;
  warnings?: WarningCode[];
  occurredAt: OccurredAt;
  handled: boolean;
};

type PacketQuotes = {
  snapshotAt: OccurredAt;
  freshness: Freshness;
  items: PacketQuoteItem[];
  klines?: PacketKlineSeries[];
  indicators?: PacketIndicatorSnapshot[];
  scan?: PacketScanResult;
};

type PacketQuoteItem = {
  tsCode: TsCode;
  name: string;
  category: InstrumentCategory;
  price?: Price;
  change?: Price;
  changePercent?: Percent;
  volume?: Volume;
  amount?: Amount;
  turnoverRate?: Percent;
  volumeRatio?: number;
  peTtm?: number;
  pb?: number;
  source: string;
  freshness: Freshness;
};

type PacketKlineSeries = {
  tsCode: TsCode;
  period: "minute" | "1m" | "5m" | "15m" | "30m" | "60m" | "day" | "week" | "month";
  adjust?: "none" | "qfq" | "hfq";
  source: string;
  fetchedAt: OccurredAt;
  points: Array<{
    time: OccurredAt | TradeDate;
    open: Price;
    high: Price;
    low: Price;
    close: Price;
    volume?: Volume;
    amount?: Amount;
  }>;
};

type PacketIndicatorSnapshot = {
  tsCode: TsCode;
  basis: { period: string; adjust?: string; fetchedAt: OccurredAt };
  values: Partial<Record<IndicatorName, number | null>>;
};

type PacketScanResult = {
  generatedAt: OccurredAt;
  criteria: string[];
  items: Array<PacketQuoteItem & { rank?: number }>;
};

type PacketNews = {
  snapshotAt: OccurredAt;
  freshness: Freshness;
  items: PacketNewsItem[];
};

type PacketNewsItem = {
  id: string;
  source: string;
  title: string;
  summary?: string;
  url?: string;
  publishedAt?: OccurredAt;
  articleExcerpt?: string;
  fetchedAt?: OccurredAt;
};

type PacketEpisodeSummary = {
  episodeId: string;
  createdAt: OccurredAt;
  symbols: TsCode[];
  action: DecisionEpisode["action"];
  thesis: string;
  outcome?: string;
};

type PacketUserPreference = {
  key: string;
  value: string;
  updatedAt: OccurredAt;
};
```

规则：

- 该对象是运行时投影，不是长期存储真源。
- Packet 类型是 Agent 工具输出和上下文组装 schema，不是 Agent 长期领域模型。
- Packet 类型是面向 Agent 的瘦身视图，不要求等同于上游完整 DTO，但字段必须稳定、可渲染、可审计。
- 指标名使用 Quotes spec 定义的固定 `IndicatorName` 集合和小写 snake_case；新增指标必须先扩展 Quotes spec，不能用自由字符串临时塞值。
- 每次 run 重新构造，不能把旧工具结果当作实时事实复用。
- Chat 历史只作为交互上下文；交易判断主要依赖这个实时决策包。

```ts
type FetchQuotesToolInput = {
  tsCodes?: TsCode[];
  scan?: {
    filter?: string;
    conditions?: JsonSummary[];
    sortBy?: string;
    limit?: number;
  };
  include?: {
    quote?: boolean;
    intraday?: boolean;
    klines?: Array<"day" | "week" | "month">;
    minuteKlines?: Array<"1m" | "5m" | "15m" | "30m" | "60m">;
    indicators?: true | IndicatorName[];
    profile?: boolean;
    dailyBasic?: boolean;
    events?: boolean;
  };
  limit?: JsonSummary;
};

type FetchQuotesToolOutput = PacketQuotes & {
  warnings?: WarningCode[];
  errors?: ErrorCode[];
};

type FetchNewsToolInput = {
  ids?: string[];
  query?: string;
  includeArticle?: boolean;
  limit?: number;
};

type FetchNewsToolOutput = PacketNews & {
  warnings?: WarningCode[];
  errors?: ErrorCode[];
};

type FetchAccountToolInput = {
  include?: {
    snapshot?: boolean;
    positions?: boolean;
    orders?: boolean;
    watchlist?: boolean;
    events?: boolean;
    triggers?: boolean;
  };
  positionStatus?: "open" | "closed" | "all";
  orderStatus?: "open" | "pending" | "partially_filled" | "filled" | "cancelled" | "rejected" | "expired" | "all";
  limit?: number;
  offset?: number;
};

type FetchAccountToolOutput = PacketAccount;

type UpdateWatchlistToolInput = UpdateWatchlistInput;

type UpdateWatchlistToolOutput = UpdateWatchlistResponse;

type OperateAccountToolInput = OperateAccountInput;

type OperateAccountToolOutput = {
  accepted: boolean;
  reason?: ErrorCode;
  orderId?: string;
  positionId?: string;
  triggerId?: string;
  snapshot?: PacketAccountSnapshot;
  warnings?: WarningCode[];
};
```

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

### 上下文压缩策略

压缩目标：

```text
保留当前事实入口和交易审计
清理过期市场数据
把长期对话沉淀成可续接摘要
避免 provider context-too-long 失败
```

触发条件：

| Trigger | 条件 | 动作 |
|---|---|---|
| time-based micro clear | 距上一条 assistant 消息超过约 60 分钟 | 清理旧易腐工具结果，保留最近若干条 |
| soft limit | 估算 token 超过 `context_soft_limit_tokens` | 先 MicroClear；仍过大时进入 Drop 兜底 |
| summarize threshold | MicroClear 后仍超过 `context_summarize_threshold` | 调 compact 模型生成摘要边界 |
| manual compact | Agent 调用 `compact_now(reason)` | 下一轮强制 Summarize |
| provider rejection | provider 返回 context-too-long | Reactive 丢弃最老 API round 后重试一次 |
| hard limit | 尽力压缩后仍超过 `context_hard_limit_tokens` | 中止 run，返回明确错误 |

丢弃 / 压缩顺序：

```text
1. MicroClear 易腐工具结果
2. Summarize 尾窗外历史对话
3. Drop 最旧消息 / API round
4. Reactive retry
5. HardLimit fail closed
```

易腐工具结果：

- `fetch_quotes`
- `fetch_news`
- server-side `web_search`

原则：

- 所有可重新读取的数据工具结果都属于可清理对象，包括行情、K 线、分时、扫描、新闻、搜索结果、账户读模型快照。
- 交易写工具、策略写入、账户确认结果不属于可清理对象。

规则：

- 易腐工具结果替换成 stub，必须保留 `tool_use_id` / call id，不能破坏 provider 的 tool_use ↔ tool_result 配对。
- MicroClear 白名单只保留最新统一工具名；不为历史碎工具名做兼容。
- Summarize 输出必须是中文结构化摘要，至少覆盖：关注标的、已建立判断、未决问题、风险纪律、用户偏好、上一轮上下文。
- Summarize 摘要是续接上下文，不是事实真源；当前行情和账户状态仍必须重新读取。
- Drop 只作为兜底；优先保留最近 `compact_keep_last_n_turns` 轮真实消息。
- 每次 compact 必须 emit `AgentEvent.compacted`，记录 tier、丢弃消息数和估算节省 token。
- 压缩后必须 sanitize 孤儿 tool_use / tool_result，避免 provider 拒收。

### 策略动态注入

```text
active StrategyCard
  -> select active cards by deterministic injection rule
  -> injected into RealtimeDecisionPacket
  -> used by Agent to form thesis and risk plan
  -> review may produce suggestedChange
```

规则：

- 策略不是硬编码 prompt，也不是 Account 规则。
- 策略卡调整后，下一次 run 使用最新 active 内容。
- 第一阶段默认注入全部 `status = active` 的策略卡；如果超过 `agent.strategy.max_active_cards`，必须保留 baseline 策略卡，再按 `applicableRegimes` 匹配度和 `updatedAt desc` 截断，并在 packet warning 中列出被省略的 strategy id。
- Agent 可以引用策略卡，也可以说明为什么本次不适用。
- 第一阶段只保留策略调整入口和建议记录，不实现自动生效。

### 账户动作纪律

所有交易写动作都必须产生：

- `DecisionEpisode`
- `operate_account` 工具调用
- `TradeIntent`
- Account 返回的执行结果

`TradeIntent` 是 `operate_account` 工具调用的持久化意图 / 审计快照，不是独立于 Account tool 的第二套命令模型。字段应从 `OperateAccountInput` 派生，避免和 Account 写接口分叉。

```ts
type TradeIntent = {
  intentId: string;
  episodeId: string;
  toolCallId?: string;
  accountInput: OperateAccountInput;
  reason: string;
  strategyIds: string[];
  status: "proposed" | "submitted" | "accepted" | "rejected" | "executed";
  accountResultRef?: AccountResultRef;
  createdAt: OccurredAt;
};

type AccountResultRef = {
  orderId?: string;
  positionId?: string;
  accountEventIds?: string[];
  triggerId?: string;
  rejectionEventId?: string;
};
```

规则：

- Agent 生成 `operate_account` input；`TradeIntent` 只是把本次调用意图、理由和执行结果持久化，供复盘和前端追溯。
- `accountResultRef` 指向 Account 返回或写入的稳定 ID：订单用 `orderId`，仓位变更用 `positionId`，审计链用 `accountEventIds`；拒单若写入事件则填 `rejectionEventId`。
- Account 决定是否可执行。
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
