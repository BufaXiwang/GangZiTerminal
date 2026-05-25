# Agent Runtime Spec

> 本文档定义 Agent 在本产品里的业务运行期：跨模块事件路由、后台任务编排、AgentRun 生命周期、run profile、工具使用策略、实时决策 packet、投资判断记录和复盘审计。
>
> Agent 的底层模型渠道、消息、工具注册协议、context compaction 和 canonical loop 见 [agent-infra-module.md](agent-infra-module.md)。
>
> 模块级领域契约仍以 `docs/design/*-module.md` 为准。

## 一句话定位

**Agent Runtime 是产品里的 Agent 应用层**：它决定什么时候唤起 Agent、每类 run 能用哪些工具、如何联合 Quotes / News / Account、如何把模型判断沉淀为可审计的投资记录。

它解决的问题：

```text
News / Account / Quotes 只表达“发生了什么”
Agent Runtime 决定“谁应该被唤起、何时唤起、用哪些工具、如何去重”
Agent Infra 负责“把一次 LLM loop 稳定跑完”
Agent Runtime 负责“把结果变成产品内可复盘的判断链”
```

契约强度：

- `AgentRun`、`AgentRunProfile`、`DecisionEpisode`、`EvidenceRef`、`TradeIntent`、`StrategyCard`、`DecisionReview`、Realtime Packet、跨模块 event consumption 是 `Spec-as-source`。
- 默认 tick 频率、watchdog 超时、退避参数、具体 runtime 文件组织是 `Spec-anchored`。
- 自动策略晋升、自动调参、多 agent 协作不属于第一阶段。

共享类型见 [shared-types.md](shared-types.md)。

---

## 1. 责任边界

Agent Runtime 负责：

- 启动和管理后台 loop。
- 监听应用内事件，如 `news-refreshed`、`account-triggered`、`market-quotes-refreshed`。
- 把用户消息、模块事件和定时任务路由成 `AgentRun`。
- 为每类 `AgentRun` 选择 run profile、allowed tools、context scope 和交易权限。
- 构造 `RealtimeDecisionPacket`，联合 Quotes / News / Account / Strategy / Recent Episodes。
- 调用 Agent Infra 执行 canonical loop。
- 根据模型输出和工具结果记录 `DecisionEpisode`、`EvidenceRef`、`TradeIntent`、`DecisionReview`。
- 定时触发 News refresh、Quotes refresh、Account trigger evaluation、K 线预热、Agent scheduled review。
- 从 Account 获取 subscribed codes，并注入 Quotes refresh scope。
- 维护跨模块任务的 in-flight lock、退避、重试、幂等和 heartbeat。
- 向前端 emit 可观测运行状态。

Agent Runtime 不负责：

- Provider wire format 适配。
- context window 压缩细节。
- 行情 provider、指标计算、市场 universe。
- 新闻 provider、正文抽取、新闻去重。
- 账户现金、持仓、市值、成本、盈亏、成交模拟。
- 绕过 Agent 做交易决策。
- 绕过 Account 直接写订单、持仓或现金。
- 真实券商交易。

边界规则：

- Agent Runtime 位于 `pipeline/` 或由 adapter 启动的 runtime wiring 中。
- Agent Runtime 可以调用各模块公开 use case / facade。
- Agent Runtime 不属于 Quotes / News / Account 任一 BC；它是 Agent 应用层。
- Quotes / News / Account 不知道 Agent Runtime 存在，只暴露事实、事件和 facade。
- 复杂业务判断交给 Agent 或对应 BC，不放进 Runtime 的调度代码。

---

## 2. 领域模型

### 核心概念

| 概念 | 含义 | 身份 |
|---|---|---|
| `AgentRun` | 一次产品语义上的 Agent 运行 | `run_id` |
| `AgentRunProfile` | 某类 run 的工具、权限、上下文和优先级策略 | `profile_id` |
| `RealtimeDecisionPacket` | 本次 run 的实时决策上下文投影 | run 内临时对象 |
| `DecisionEpisode` | 一次可复盘的投资判断记录 | `episode_id` |
| `EvidenceRef` | 投资判断依赖的证据快照 | `kind + id` |
| `TradeIntent` | `operate_account` 调用的持久化意图 / 审计快照 | `intent_id` |
| `StrategyCard` | 运行时注入 Agent 上下文的策略卡 | `strategy_id` |
| `DecisionReview` | 对一次决策、交易或触发事件的复盘 | `review_id` |
| `InvestorMemory` | 用户偏好、长期约束和可注入记忆 | `memory_key` |
| `AgentRuntimeEventConsumption` | 跨模块事件消费幂等记录 | `event_type + event_key + consumer` |

### 不变量

- 每次 Agent run 必须有 `run_id`、trigger、profile、状态、开始时间和结束状态。
- 纯聊天 / 设置解释 / 普通知识问答可以只有 run，不产生 episode。
- 只要 Agent 对标的、组合、新闻或账户触发形成投资判断，就必须产生 `DecisionEpisode`，即使结论是 `no_action`。
- 每次产生 `TradeIntent` / `operate_account` 调用的 run 必须先关联一个 `DecisionEpisode`。
- 所有交易写动作必须通过 Account 的 `operate_account`，不能直接写订单、持仓或现金。
- 行情、账户、新闻工具结果必须带时间戳；过期事实不能作为当前下单依据。
- 同一 Account trigger 必须幂等处理，不能重复触发交易动作。
- Agent 可以建议策略调整，但第一阶段不会自动改写 active `StrategyCard`。
- 后台 run 如果形成投资判断，必须产生 episode，不能静默消费触发事件。
- Runtime 不得在没有 run profile 授权时暴露 `operate_account`。

### `AgentToolName`

```ts
type AgentToolName =
  | "fetch_quotes"
  | "fetch_news"
  | "fetch_account"
  | "operate_account"
  | "update_watchlist"
  | "record_decision_episode"
  | "record_decision_review";

type AgentRuntimeToolSpec = Omit<ToolSpec, "name"> & {
  name: AgentToolName;
};
```

规则：

- `AgentToolName` 是本产品的 canonical local tool name union。
- `AgentRunProfile.allowedTools` 和 Runtime 注册给 Infra 的 `AgentRuntimeToolSpec.name` 必须使用 `AgentToolName`。
- 新增 Agent local tool 必须先扩展本 union 和本 spec，不能只在实现里注册自由字符串。
- `record_decision_episode` / `record_decision_review` 是 Agent 业务审计写工具，归 Agent Runtime 拥有，不调用 Quotes / News / Account。

### `AgentRun`

```ts
type AgentRunTrigger =
  | { kind: "user_chat"; messageId: string }
  | { kind: "news_batch"; newsIds: string[] }
  | { kind: "account_trigger"; triggerId: string }
  | { kind: "scheduled_review"; reason: string }
  | { kind: "manual_replay"; refId: string };

type AgentRun = {
  runId: string;
  profileId: string;
  episodeIds?: string[];
  trigger: AgentRunTrigger;
  provider: string;
  wireFormat: "messages" | "responses" | "chat_completions";
  model: string;
  status: "queued" | "running" | "completed" | "failed" | "cancelled";
  startedAt?: OccurredAt;
  endedAt?: OccurredAt;
  error?: string;
};
```

规则：

- `run_id` 是事件流、工具调用、模型 turn、episode、trade intent 的关联键。
- `episodeIds` 是该 run 产生的投资判断集合；不是所有 run 都有 episode。
- 一次 run 可以产生多个 episode；`news_batch` 可以按相关标的 / 主题拆成多个 episode，单个账户 trigger 通常只产生一个 episode。
- 用户对话和后台触发共用同一套 Infra loop，只是 trigger、profile、上下文和预算不同。
- 单个 run 使用单个 provider channel / model；需要切换模型必须启动新的 run，并通过 `causationId` / `manual_replay` 关联。
- 运行失败也必须落状态和错误原因。
- 如果事件无需触发 Agent run，例如空 news batch 或重复 trigger，不创建 `AgentRun`，只在 `AgentRuntimeEventConsumption.status = "ignored"` 中记录。

### `AgentRunProfile`

```ts
type AgentRunProfile = {
  profileId:
    | "user_chat"
    | "news_analysis"
    | "account_trigger_response"
    | "scheduled_review"
    | "manual_replay";
  priority: "P0" | "P1" | "P2" | "P3";
  allowedTools: AgentToolName[];
  allowServerSideTools: boolean;
  allowTradingWrite: boolean;
  requiredPacketSections: Array<"account" | "quotes" | "news" | "strategies" | "recent_episodes" | "user_preferences">;
  maxTurns: number;
  maxRuntimeMs: number;
};
```

默认 profile：

| Profile | Trigger | allowed tools | 交易写 |
|---|---|---|---|
| `user_chat` | 用户消息 | `fetch_quotes`、`fetch_news`、`fetch_account`、`update_watchlist`、`record_decision_episode`、`record_decision_review`、`operate_account` | 允许，但必须先记录 episode 并通过 Account 校验 |
| `news_analysis` | news batch | `fetch_news`、`fetch_quotes`、`fetch_account`、`update_watchlist`、`record_decision_episode`、`record_decision_review`、`operate_account` | 允许，但必须有 episode 和新鲜行情 |
| `account_trigger_response` | account trigger | `fetch_account`、`fetch_quotes`、`fetch_news`、`update_watchlist`、`record_decision_episode`、`record_decision_review`、`operate_account` | 允许 |
| `scheduled_review` | 定时巡检 | `fetch_account`、`fetch_quotes`、`fetch_news`、`update_watchlist`、`record_decision_episode`、`record_decision_review`、按配置 `operate_account` | 默认关闭，可配置开启 |
| `manual_replay` | 人工复盘 | `fetch_account`、`fetch_quotes`、`fetch_news`、`record_decision_review` | 禁止 |

默认 required packet sections：

| Profile | requiredPacketSections |
|---|---|
| `user_chat` | `strategies`、`recent_episodes`、`user_preferences`；账户 / 行情 / 新闻按工具调用实时读取 |
| `news_analysis` | `news`、`quotes`、`account`、`strategies`、`recent_episodes`、`user_preferences` |
| `account_trigger_response` | `account`、`quotes`、`news`、`strategies`、`recent_episodes`、`user_preferences` |
| `scheduled_review` | `account`、`quotes`、`news`、`strategies`、`recent_episodes`、`user_preferences` |
| `manual_replay` | `account`、`quotes`、`news`、`strategies`、`recent_episodes` |

规则：

- Runtime 根据 trigger 选择 profile，并把 `allowedTools` 注册进 Infra `ToolRegistry`。
- `operate_account` 只有在 `allowTradingWrite = true` 且 `allowedTools` 包含它时才可暴露。
- `user_chat` 不做独立的预意图分类闸门；交易写仍必须先调用 `record_decision_episode`，并由 Strategy 纪律、freshness 校验和 Account fail-closed 共同约束。
- `scheduled_review.allowTradingWrite` 默认由 runtime settings KV 控制；未配置时为 false，不从 `StrategyCard` 或模型输出隐式开启。
- `update_watchlist` 是非交易写工具，可以由用户消息、新闻分析或定时巡检使用。
- `manual_replay` 永远不能写 Account。
- Profile 是 Runtime 策略，不属于 Infra。
- Profile 可以被配置覆盖，但不能突破 Account 模块的 actor / 交易规则。

### `DecisionEpisode`

```ts
type DecisionEpisode = {
  episodeId: string;
  runId: string;
  triggerKind: AgentRunTrigger["kind"];
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
    | "adjust_protection"
    | "record_invalidation_signal";
  actionStatus: "no_action" | "intended" | "submitted" | "blocked" | "deferred";
  blockedReason?: ErrorCode | WarningCode | string;
  confidence?: number;
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
- Episode 只属于 Agent Runtime；News / Quotes / Account 不创建也不持有 `DecisionEpisode`。
- `no_action` 是有效 episode，用于记录“为什么不行动”。
- `DecisionEpisode.action` 表示 Agent 的决策意图，不表示 Account 已实际执行。
- `actionStatus` 表示该意图的执行状态：`no_action` 表示明确不行动；`intended` 表示形成动作意图但尚未提交；`submitted` 表示 Runtime 已创建 `TradeIntent` 并开始提交；`blocked` 表示因 stale quote、risk limit、insufficient cash 或 Account 拒绝等原因未能执行；`deferred` 表示等待更多信息或下次 review。
- 一个 episode 不一定产生账户写动作；只有需要写 Account 时，才生成 `TradeIntent.accountInput`。
- `record_decision_episode` 只能创建 episode，不能修改已有 episode；同一 run 内需要记录新的判断时必须创建新 episode。
- 交易类动作在 `record_decision_episode` 时应使用 `actionStatus = "intended"`；Runtime 接受 `operate_account` 后自动把该 episode 推进为 `submitted`，并关联 `TradeIntent`。
- run 结束时，`action` 为交易类动作且 `actionStatus = "submitted"` 的 episode 必须关联 `TradeIntent`；否则 Runtime 必须把它推进到 `blocked` 或 `deferred` 并记录原因。
- `action` 为交易类动作但未生成 `TradeIntent` 时，`actionStatus` 必须是 `blocked` 或 `deferred`，并在 `blockedReason` 或 `thesis` 中写清原因。
- `action = "no_action"` 时，`actionStatus` 必须是 `no_action`。
- `thesis` 必须描述为什么做或为什么不做。
- `symbols` 必须使用标准 `TsCode`，不能保存自由股票名或 6 位代码。
- `confidence` 取值范围为 0..1；缺失表示模型未给出可审计置信度，不等于 0。
- 交易动作必须绑定 `strategyIds` 或明确说明是临时判断。
- `evidenceRefs` 必须引用当时使用的新闻、行情、账户事件、工具结果和策略卡。

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
  protection?: {
    stopLoss?: Price;
    takeProfit?: Price;
    timeStopAt?: OccurredAt;
    invalidationSignals?: string[];
    enabled: boolean;
    revision: number;
  };
};

type EvidenceOrderSnapshot = EvidenceSnapshotBase & {
  orderId: string;
  tsCode: TsCode;
  side: "buy" | "sell";
  orderType: "market" | "limit";
  limitPrice?: Price;
  quantity: Shares;
  filledQuantity: Shares;
  status: OrderStatus;
};

type EvidenceAccountTriggerSnapshot = EvidenceSnapshotBase & {
  triggerId: string;
  triggerType: AccountTriggerType;
  tsCode?: TsCode;
  positionId?: string;
  orderId?: string;
  occurredAt: OccurredAt;
};

type EvidenceStrategySnapshot = EvidenceSnapshotBase & {
  strategyId: string;
  version: number;
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
- Evidence snapshot 是持久化审计 schema，不直接存 `Packet*` 运行时投影。
- 写入时由当前 packet / tool result 映射成 `Evidence*Snapshot`。
- `evidenceRefs` 必须由 `record_decision_episode` / `record_decision_review` 显式声明；Runtime 不自动把本 run 的所有 tool calls 都挂到 episode 或 review 上。
- Runtime 必须校验声明的 evidence 是否来自本 run 的 packet、工具结果、active strategy、recent episode / review 或指定 replay ref；校验失败时拒绝记录。
- `OrderStatus` 和 `AccountTriggerType` 引用 Account 模块的 canonical enum，不能降级成任意字符串。
- 每个 snapshot 必须带 `schemaVersion`。
- 同一 version 只能新增可选字段，不能重命名或删除已有字段；破坏性变更必须 bump version 并保留旧 version 反序列化。
- 被 episode 引用的重要新闻必须保存 `EvidenceNewsSnapshot`，避免源内容变更、远端不可用或后续正文重抽取影响复盘。
- 快照应是最小可复盘视图，不保存无限长正文；长正文使用摘要或 excerpt。

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

- `StrategyCard` 是 Runtime 注入 Agent 上下文的策略上下文。
- 首次启动若没有 active strategy，Runtime 必须 seed 一个内置 active baseline 策略卡，例如 `baseline_a_share_risk_control`。
- Baseline 至少覆盖仓位上限、freshness、止损和不确定时不交易等基础纪律。
- 如果策略表为空或 seed 失败，Agent 仍可回答和读取数据，但交易写动作默认禁用。
- Agent run 读取 active strategy，不直接把复盘建议写回 active strategy。
- 策略卡可以被外部显式调整；调整后下一次 Agent run 自动使用新版本内容。
- 每次策略卡调整必须递增 `version`，并保留旧版本可追溯。
- 第一阶段不要求策略版本晋升、shadow run、自动调参、回测引擎。

### `TradeIntent`

```ts
type TradeIntent = {
  intentId: string;
  runId: string;
  episodeId: string;
  toolCallId?: string;
  accountInput: OperateAccountInput;
  reason: string;
  strategyIds: string[];
  status: "proposed" | "submitted" | "accepted" | "rejected" | "executed";
  accountResultRef?: AccountResultRef;
  createdAt: OccurredAt;
  updatedAt: OccurredAt;
};

type AccountResultRef = {
  orderId?: string;
  fillIds?: string[];
  positionId?: string;
  accountEventIds?: string[];
  triggerId?: string;
  rejectionEventId?: string;
  message?: string;
};
```

规则：

- `TradeIntent` 是 `operate_account` 工具调用的持久化意图 / 审计快照，不是第二套账户命令模型。
- 字段应从 `OperateAccountInput` 派生，避免和 Account 写接口分叉。
- Agent 生成 `operate_account` input；Account 决定是否可执行。
- Account 拒绝后，Agent 只能记录或重新判断，不能绕过 Account。
- `accountResultRef` 指向 Account 返回或写入的稳定 ID。
- 行情 freshness 不足时，Runtime 不应提交交易写工具。
- Runtime 必须维护 `orderId -> intentId / episodeId / runId` 的反查索引；Account trigger 只携带 `orderId` 时，Runtime 通过该索引把订单终态 review 归因回原始 episode。Account payload 不携带 Agent 的 `intentId`，避免 Account 反向感知 Agent。
- 状态机：
  - `proposed`：Runtime 已记录交易意图，但尚未调用 `operate_account`。
  - `submitted`：`operate_account` 调用已开始，等待 Account response；若 tool 超时或 provider 中断，保持 `submitted` 并依靠 `toolCallId` / Account 审计 ID 做恢复核对。
  - `accepted`：Account 返回 `accepted = true`，但本次交易意图仍有后续订单终态要等待；典型为 limit pending 或部分成交。
  - `executed`：Account 返回 `accepted = true` 且本次动作已即时完成，不再等待订单终态；典型为 market 即时成交、撤单成功、保护条件调整成功、watchlist 以外的账户写动作完成。
  - `rejected`：Account 返回 `accepted = false`，或提交前校验 fail closed。
- 合法转换：`proposed -> submitted -> accepted | executed | rejected`；`proposed -> rejected`；`submitted -> rejected`。
- `rejected`、`accepted`、`executed` 是终态；不得回退或复用同一个 `intentId` 重新提交。
- 后续 limit 订单成交 / 过期 / 拒绝由 Account trigger 和新的 run / review 记录，不回写旧 `TradeIntent.status`。
- 启动恢复时，Runtime 必须扫描 `status = "submitted"` 的 `TradeIntent`：若已有 `accountResultRef` 或可通过 `toolCallId` 读到 Account result，则按结果补到 `accepted` / `executed` / `rejected`；若无法证明 Account 已产生任何持久事实，则标记为 `rejected`，`message = "submission_unknown_no_account_effect"`，并写可观测事件。

### `DecisionReview`

```ts
type DecisionReview = {
  reviewId: string;
  episodeId: string;
  trigger:
    | "position_closed"
    | "stop_loss"
    | "take_profit"
    | "time_stop"
    | "invalidated"
    | "order_filled"
    | "order_rejected"
    | "order_expired"
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
  warnings?: WarningCode[];
  createdAt: OccurredAt;
};
```

规则：

- Review 只记录复盘和建议，不自动修改策略。
- 单次交易结果不能证明策略有效或无效。
- 建议必须能追溯到 episode、账户结果、行情或新闻证据。
- 每次保护条件触发、订单终态、平仓或定时复盘可以生成 `DecisionReview`。
- 找不到原始 episode / order 映射时，review 必须带 `mapping_missing` warning，并挂到当前 account trigger run 产生的 episode 上。

---

## 3. Realtime Decision Packet

Packet 是 Runtime 传给 Infra / 模型的运行时投影，不是长期存储真源。

```ts
type RealtimeDecisionPacket = {
  runId: string;
  trigger: AgentRunTrigger;
  account?: PacketAccount;
  quotes?: PacketQuotes;
  news?: PacketNews;
  strategies: StrategyCard[];
  recentEpisodes: PacketEpisodeSummary[];
  userPreferences: PacketUserPreference[];
};

type PacketAccount = {
  snapshot?: PacketAccountSnapshot;
  positions?: PacketPosition[];
  orders?: PacketOrder[];
  watchlist?: PacketWatchlistItem[];
  triggers?: PacketAccountTrigger[];
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
    invalidationSignals?: string[];
    enabled: boolean;
    revision: number;
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
  status: OrderStatus;
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
  triggerType: AccountTriggerType;
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

- Packet 类型是 Runtime 的上下文组装 schema，不是 Agent 长期领域模型。
- Packet 是面向 Agent 的瘦身视图，不要求等同于上游完整 DTO，但字段必须稳定、可渲染、可审计。
- 每次 run 重新构造，不能把旧工具结果当作实时事实复用。
- Chat 历史只作为交互上下文；交易判断主要依赖本次 packet 和本次工具调用。
- 非 `user_chat` run 默认不注入完整聊天历史，只注入策略、用户偏好、相关 recent episodes / reviews 和实时 packet；`manual_replay` 可按 `refId` 注入指定历史。
- 用户偏好和长期记忆来自 `InvestorMemory`；Runtime 只按 profile 和相关性注入摘要，不把未筛选的全部 memory 塞进上下文。
- `PacketEpisodeSummary.outcome` 由最近一条 `DecisionReview.conclusion` 和可用的 `DecisionReview.result` 派生；没有 review 时为空，Runtime 不得凭空生成 outcome 文案。
- 指标名使用 Quotes spec 定义的固定 `IndicatorName` 集合；新增指标必须先扩展 Quotes spec，不能用自由字符串临时塞值。
- 构建交易相关 packet 时，Runtime 必须请求账户决策所需的完整字段，并将缺失集合规范化为空数组。
- 当 Runtime 只需要待处理账户触发时，`fetch_account` 应使用 `include.triggers = true` 且保持 `triggerHandled` 缺省或显式传 `false`。

---

## 4. Agent 工具策略

Agent 消费其他模块时，Runtime 将工具收敛为少数高层工具，并注册给 Infra。

| 工具 | 来源模块 | 能力 | side effect |
|---|---|---|---|
| `fetch_quotes` | Quotes | 行情、K 线、分时、指标、基本面、扫描 | none |
| `fetch_news` | News | 新闻列表、全文、关键词搜索 | none |
| `fetch_account` | Account | 账户总览、仓位、订单、自选、事件、触发 | none |
| `operate_account` | Account | 挂单、撤单、开仓、调仓、平仓、调整保护条件 | trading_write |
| `update_watchlist` | Account | 添加 / 删除自选、更新自选备注 | non_trading_write |
| `record_decision_episode` | Agent Runtime | 记录一次可复盘投资判断 | non_trading_write |
| `record_decision_review` | Agent Runtime | 记录一次决策 / 交易 / 触发复盘 | non_trading_write |

规则：

- Runtime 决定本次 run 注册哪些工具；Infra 只执行注册表内工具。
- Agent 不直接使用碎片化模块接口。
- 读工具可以并发；`operate_account` 必须串行执行。
- 交易写能力只存在于 `operate_account`。
- `update_watchlist` 是非交易写能力，可由 Agent 用于维护观察列表。
- `record_decision_episode` / `record_decision_review` 是 Agent Runtime 审计写能力，不调用 Quotes / News / Account。
- 工具返回值必须带时间戳和来源摘要，供 Agent 判断 freshness。
- 执行模块只需要提供自己的领域接口；Runtime / adapter 负责反腐译码成 Agent tool。

工具 schema：

```ts
type FetchQuotesToolInput = {
  tsCodes?: TsCode[];
  scan?: ScanMarketRequest;
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

type FetchNewsToolInput = FetchNewsRequest;

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
  orderActive?: boolean;
  orderStatusIn?: OrderStatus[];
  triggerHandled?: boolean | "all";
  limit?: number;
  offset?: number;
};

type FetchAccountToolOutput = PacketAccount;

type UpdateWatchlistToolInput = {
  episodeId?: string;
  accountInput: UpdateWatchlistInput;
};

type UpdateWatchlistToolOutput = UpdateWatchlistResponse & {
  episodeId?: string;
};

type RecordDecisionEpisodeToolInput =
  Omit<DecisionEpisode, "episodeId" | "runId" | "triggerKind" | "createdAt">;

type RecordDecisionEpisodeToolOutput = {
  accepted: boolean;
  episodeId?: string;
  reason?: ErrorCode;
  message?: string;
};

type RecordDecisionReviewToolInput =
  Omit<DecisionReview, "reviewId" | "createdAt">;

type RecordDecisionReviewToolOutput = {
  accepted: boolean;
  reviewId?: string;
  reason?: ErrorCode;
  message?: string;
};

type OperateAccountToolInput = {
  episodeId: string;
  accountInput: OperateAccountInput;
};

type OperateAccountToolOutput = {
  accepted: boolean;
  reason?: ErrorCode;
  message?: string;
  orderId?: string;
  fillIds?: string[];
  positionId?: string;
  triggerId?: string;
  rejectionEventId?: string;
  accountEventIds: string[];
  snapshot: PacketAccountSnapshot;
  warnings?: WarningCode[];
};
```

规则：

- `ScanMarketRequest` 引用 [Quotes `scan_market`](quotes-module.md#scan_market) canonical DSL；Runtime 不重定义 `filter` / `conditions` / `sortBy` 自由字符串。
- `FetchNewsToolInput` 引用 [News `fetch_news`](news-module.md#fetch_news) canonical request，保留 `ids`、`query`、`sources`、`publishedFrom`、`publishedTo`、`includeArticle`、`limit`、`offset` 的组合语义。
- `UpdateWatchlistToolInput.accountInput`、`OperateAccountInput`、`OrderStatus` 引用 Account 模块 canonical 类型；Runtime 只把 `accountInput` 传给 Account。
- `record_decision_episode` 是模型产出 `DecisionEpisode` 的唯一写路径；`no_action`、`add_watchlist`、交易意图被阻断等不调用 Account 的判断也必须通过该工具持久化。
- `record_decision_review` 是模型产出 `DecisionReview` 的唯一写路径；它只记录复盘，不自动修改 `StrategyCard`。
- `record_decision_episode` / `record_decision_review` 必须显式提交 `evidenceRefs`；Runtime 只校验和持久化声明的 evidence，不自动 attach 全部 tool calls。
- `update_watchlist` 如果是 Agent 基于行情 / 新闻 / 账户形成判断后的动作，必须携带同一 run 内已接受的 `episodeId`；纯用户指令或系统维护型自选更新可以不携带 episode。
- `update_watchlist.episodeId` 若存在，必须指向同一 run 的 `DecisionEpisode`；`accountInput.action = "add"` 时 episode action 应为 `add_watchlist`，`accountInput.action = "remove"` 时 episode action 应为 `remove_watchlist`。
- `operate_account` 工具外层携带 `episodeId`，内层 `accountInput` 原样使用 Account canonical `OperateAccountInput`；Runtime 只把 `accountInput` 传给 Account。
- `operate_account.episodeId` 必须指向同一 run 中已接受的 `DecisionEpisode`，且该 episode 的 `actionStatus` 必须是 `"intended"` 或 `"submitted"`；否则工具必须返回 rejected，不调用 Account。
- `operate_account` 创建 `TradeIntent(status = "proposed")` 后，Runtime 必须把对应 episode 推进到 `actionStatus = "submitted"`。
- `operate_account` 工具输出必须保留 Account 返回的审计 ID，尤其是 `accountEventIds`、`fillIds` 和 `rejectionEventId`。
- Runtime 必须在提交 `operate_account` 前创建 `TradeIntent(status = "proposed")` 或确保同一事务可补写。
- Account 接受 / 拒绝后，Runtime 根据工具结果更新 `TradeIntent.status` 和 `accountResultRef`。
- `operate_account` 返回 `accepted = false` 时，Runtime 必须把对应 episode 推进到 `actionStatus = "blocked"`，并设置 `blockedReason = OperateAccountToolOutput.reason`；模型不需要、也不应通过再次调用 `record_decision_episode` 覆盖同一 episode。
- 只有一个模拟账户时，`operate_account` 必须按账户全局串行执行；未来支持多账户时，串行粒度改为 `accountId`。

---

## 5. 编排流

### 用户消息 -> Agent

```text
send_agent_message
  -> create AgentMessage(user)
  -> select AgentRunProfile(user_chat)
  -> build RealtimeDecisionPacket as needed
  -> register allowed tools
  -> Agent Infra run_agent_loop
  -> persist assistant message / tool calls / optional episode / trade intent
```

规则：

- command 只启动 run，内容通过 `agent-event` 流式返回。
- 支持图片输入；图片作为 message block 进入 canonical request。
- 同一用户会话默认只允许一个前台交互 run。
- 纯聊天、设置解释、普通问答可以只落 `AgentRun`、messages、tool calls 和 usage，不产生 episode。
- 如果用户请求投资判断或交易动作，Runtime 必须读取实时账户和行情，再允许生成 `DecisionEpisode`。
- 如果模型形成投资判断，必须调用 `record_decision_episode`；仅输出自然语言解释不能替代 episode 记录。

### News -> Agent

```text
Agent Runtime tick
  -> News.refresh_news()
  -> News emits news-refreshed
  -> Agent Runtime buffers pending newIds
  -> trigger when pending count >= M or oldest pending age >= N
  -> create AgentRun(trigger=news_batch, profile=news_analysis)
  -> Agent Infra run_agent_loop
```

规则：

- News 只刷新和 emit，不启动 Agent。
- Runtime 负责维护待分析 news buffer，buffer item 必须至少包含 `newsId`、进入 buffer 时间和来源 `batchId`。
- 当 `pending news count >= news_agent_batch_size` 时，Runtime 必须触发一次 `news_analysis` run。
- 如果在 `news_agent_max_wait_secs` 内没有因为数量阈值触发分析，且 buffer 非空，Runtime 必须触发一次 `news_analysis` run。
- 触发 run 时，Runtime 从 buffer 中取出本批 `newsIds`，创建 `AgentRun(trigger=news_batch, profile=news_analysis)`；已进入本批的 news 在该 run 完成、失败或被恢复逻辑接管前不得重复进入另一批。
- Runtime 仍必须使用 `agent.news_batch` in-flight lock 和 throttle，避免多个 news batch run 并发或过度密集。
- News batch run 成功消费后，本批 `newsIds` 从 buffer 移除；可恢复失败时必须回到 buffer 并保留 retry metadata；不可恢复失败时标记对应 consumption record 为 `failed` 或 `ignored`，不得静默丢失。
- Agent 负责新闻分析、关联标的、交易影响判断。
- News 触发的 run 如果完成了影响判断，即使结论是 `no_action`，也必须记录 episode。
- 大多数新闻应输出 `no_action` 或加入观察，不应强行交易。

### Account -> Agent

```text
Account emits account-triggered
  -> Agent Runtime reads trigger_id
  -> Agent Runtime dedupe(trigger_id)
  -> create AgentRun(trigger=account_trigger, profile=account_trigger_response)
  -> Agent Infra run_agent_loop
  -> mark Account trigger handled only after terminal runtime outcome
```

规则：

- Account 只判断订单终态或保护条件是否需要通知，不决定响应动作。
- Runtime 负责按 `has_more` / `next_cursor` 继续调度评估批次，不能把大账户一次性阻塞在单个 tick 内。
- Runtime 负责同一 `trigger_id` 只路由一次。
- Agent 收到触发后读取 Account / Quotes / News，再决定是否操作账户。
- Runtime 的 event consumption record 是 trigger 投递、processing、failed、retry 的权威状态；Account 的 `handled` 只作为最终确认位。
- 只有当 Agent run 完成消费、Runtime 按策略显式忽略、或事件被判定不可恢复放弃时，Runtime 才能调用 `Account.mark_trigger_handled`。
- 仅启动 Agent run 不得标记 handled。
- 订单终态 trigger 若包含 `orderId`，Runtime 必须通过 `orderId -> intentId / episodeId` 反查索引找到原始 episode，再把后续 `DecisionReview` 挂回该 episode；找不到映射时仍可处理 trigger，但必须为当前 trigger run 产生 episode，并把 review 挂到当前 episode 且带 `mapping_missing` warning。
- 订单终态 review 可以引用原始 episode 已记录的 evidence；Runtime 校验 evidenceRefs 时必须允许来自该原始 episode 的 evidence snapshot，不要求它们都来自当前 account trigger run。

### Account -> Quotes 订阅行情

```text
Agent Runtime quote tick
  -> Account.subscribed_codes()
  -> add Quotes.core_indexes()
  -> Quotes.refresh_market_quotes({ scope: subscribed, purpose: "intraday" })
  -> Quotes emits market-quotes-refreshed
```

规则：

- Account 拥有自选、持仓、挂单，因此暴露 subscribed codes。
- 核心指数列表归 Quotes 定义并通过 `core_indexes()` 暴露；Runtime 只调用该方法，不内嵌指数代码列表。
- Quotes 负责按 scope 刷新行情 snapshot。
- Account 只消费 Quotes 已有 snapshot / query facade，不直接触发 Quotes refresh。
- 收盘后 Runtime 触发 `purpose=close` 的 quote refresh；该任务按交易日加锁，并在启动时补做缺失的最新已完成交易日 close snapshot。

### Quotes -> Account snapshot

```text
Quotes emits market-quotes-refreshed
  -> Agent Runtime schedules Account.rebuild_account_snapshot()
  -> Account emits account-updated
```

规则：

- Account snapshot 可因行情变化重新派生。
- 这不是交易决策，只是估值和前端展示更新。

### Scheduled Agent Review

```text
Agent Runtime scheduled tick
  -> create AgentRun(trigger=scheduled_review, profile=scheduled_review)
  -> Agent Infra run_agent_loop
```

规则：

- 用于巡检持仓、挂单、自选和最近 episode。
- 定时 review 不是强制交易。
- 若同时存在高优先级 account trigger，优先处理 account trigger。
- 纯健康检查可以只落 run summary；如果对标的、仓位或组合形成判断，则必须记录 episode。

### 第一阶段学习闭环

```text
DecisionEpisode
  -> Account result / trigger
  -> DecisionReview
  -> suggestedChange
  -> explicit StrategyCard update
  -> next run injection
```

规则：

- Review 可以产生策略调整建议，但不会自动修改 active strategy。
- 策略调整必须经过显式 `upsert_strategy_card`。
- 下一次 run 注入最新 active strategy。

---

## 6. 应用事件模型

命名规则：

- 跨 BC / 应用事件使用 `kebab-case`，并表达已发生事实，例如 `news-refreshed`、`account-triggered`。
- BC 内部领域事件枚举使用 `snake_case`，并表达已发生事实，例如 `order_placed`、`position_closed`。
- 需要跨 BC 路由的共享 payload 使用 `<PascalCase>Payload`，并只在 [shared-types.md](shared-types.md) 定义一次。
- Event type 一旦对外使用，不复用为其他语义；破坏性变更必须新增事件名。

| Event | Payload | Producer | Consumer | 含义 |
|---|---|---|---|---|
| `news-refreshed` | `NewsRefreshedPayload` | News refresh use case | Agent Runtime / UI | 新闻本地读模型发生变化 |
| `market-quotes-refreshed` | `MarketQuotesRefreshedPayload` | Quotes refresh use case | Agent Runtime / Account snapshot / UI | 行情 snapshot 更新 |
| `account-updated` | `AccountUpdatedPayload` | Account write / trigger use case | Agent Runtime / UI | 账户状态发生变化 |
| `account-triggered` | `AccountTriggeredPayload` | Account write / trigger use case | Agent Runtime / UI | 订单或仓位条件命中，需要下游决策方感知 |
| `agent-run-started` | Agent 自有最小 payload | Agent Runtime | UI / observability | Agent run 开始 |
| `agent-run-finished` | Agent 自有最小 payload | Agent Runtime | UI / observability | Agent run 结束 |

规则：

- Event 只表达事实，不携带业务决策。
- Producer 不知道 consumer。
- Consumer 必须做幂等处理。
- Event envelope 必须包含 `eventId` 和 `occurredAt`，可携带 `correlationId` / `causationId`。
- 模块事件必须先成为可查询事实，再被 Runtime 消费；不能只依赖进程内瞬时回调。
- UI 事件可以是 transient；跨模块路由事件必须有 durable consumption record。

---

## 7. 调度优先级

| 优先级 | Trigger | 说明 |
|---:|---|---|
| P0 | `account-triggered` | 止损、止盈、拒单、成交等账户事件 |
| P1 | `user_chat` | 用户前台交互 |
| P2 | `news-refreshed` / news batch | 新闻驱动分析 |
| P3 | scheduled account / strategy review | 定时巡检和复盘 |
| P4 | quotes universe / kline warm / enrichment | 数据维护任务 |

规则：

- 同一账户同一标的的 P0 run 应串行。
- P0 可以打断或延后低优先级后台任务。
- P2 news batch 允许攒批，不要求每条新闻立即启动 Agent。
- 数据维护任务失败不应阻塞用户交互，但必须记录 heartbeat。
- P0 不直接杀死已提交的 Account 写操作；只允许取消尚未提交 provider / tool 的低优先级 Agent run，或延后其后续 turn。

---

## 8. 幂等和可靠性

### In-flight lock

每类后台 run 至少有进程级锁：

| Task | Lock Key |
|---|---|
| news batch Agent run | `agent.news_batch` |
| account trigger Agent run | `agent.account_trigger:{trigger_id}` |
| scheduled review | `agent.scheduled_review` |
| quote subscribed refresh | `quotes.subscribed_refresh` |
| universe refresh | `quotes.universe_refresh` |
| close snapshot refresh | `quotes.close_snapshot:{trade_date}` |

### 事件消费记录

```ts
type AgentRuntimeEventConsumption = {
  eventType: string;
  eventKey: string;
  consumer: string;
  status: "processing" | "consumed" | "ignored" | "failed";
  runId?: string;
  error?: string;
  createdAt: OccurredAt;
  updatedAt: OccurredAt;
};
```

规则：

- `(eventType, eventKey, consumer)` 是消费幂等键。
- `account-triggered` 使用 `trigger_id` 做 `event_key`。
- `news-refreshed` 默认使用 `batchId` 做 `event_key`；如果需要合并多批，使用排序后的 `newIds` hash。
- 已 `consumed` 的 event 不重复触发 Agent run。
- `ignored` 表示 Runtime 已明确判定该 event 无需触发 Agent run 或无需进一步处理；它是终态，不能被 watchdog 当作失败重试。
- `processing` 超时可被 watchdog 回收。
- `Account.mark_trigger_handled` 必须发生在 consumption record 进入 `consumed` / `ignored` 等终态之后；`processing` / `failed` 状态不得标记 Account handled。

### 失败策略

- 单次工具 / provider 失败不让整个 runtime 崩溃。
- 连续失败进入退避。
- 可恢复任务保留 pending 状态等待下次重试。
- 不可恢复错误写入失败状态和 UI 可见事件。
- 进程启动时，`AgentRun.status = "running"` 的旧 run 必须标记为 `failed`，`error = "interrupted_by_restart"`；`queued` run 可按 profile 和 lock 状态重新调度。
- 进程启动时必须扫描未进入终态的消费记录（`processing` 超时、`failed` 可重试）并恢复。
- 进程启动时必须扫描 `TradeIntent.status = "submitted"` 并按 `TradeIntent` 状态机恢复规则核对 Account 结果。
- 启动时必须从模块读模型补扫仍未 handled 的 Account trigger 和仍待路由的 news batch，避免停机期间事件永久丢失。
- 错过的盘后任务必须在下次启动或下个 scheduler tick 补偿执行，不能永久丢失。

---

## 9. 对外接口

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

#### `fetch_agent_state`

```ts
type FetchAgentStateRequest = {
  include?: {
    messages?: boolean;
    runs?: boolean;
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

#### `cancel_agent_run`

```ts
type CancelAgentRunRequest = {
  runId: string;
  reason?: string;
};

type CancelAgentRunResponse = {
  accepted: boolean;
  runId: string;
  status: "cancelled" | "completed" | "failed" | "not_found";
};
```

规则：

- 只能取消 `queued` 或尚未提交当前 tool call 的 `running` run。
- “已提交当前 tool call”以 Infra 调用 `dispatch_tool_call` 的 handler 后为界；handler 已开始执行后，取消请求不得中断该 tool 的副作用，只能阻止后续 turn。
- 已提交给 Account 的 `operate_account` 不得被 Runtime 撤销；需要撤单必须走新的 `operate_account(cancel_order)`。
- 取消成功后 `AgentRun.status = "cancelled"`，并 emit `agent-run-finished`。

#### `fetch_strategy_cards`

```ts
type FetchStrategyCardsRequest = {
  status?: "active" | "paused";
};
```

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

规则：

- 显式创建或调整策略卡。
- 调整后的 active 策略卡从下一次 Agent run 开始注入。
- Agent 的 `DecisionReview.suggestedChange` 不会自动调用该接口。
- `baseVersion` 用于乐观并发；版本冲突必须拒绝。
- `reason` 进入策略审计记录。

### 恢复结果

```ts
type RecoverySummary = {
  scanned: number;
  recovered: number;
  failed: number;
  ignored: number;
  details?: Array<{
    id: string;
    kind: "agent_run" | "trade_intent" | "event_consumption" | "news_batch";
    status: "recovered" | "failed" | "ignored";
    message?: string;
  }>;
};
```

### 内部 Runtime API

```rust
send_agent_message(request) -> SendAgentMessageResponse;
cancel_agent_run(run_id, reason) -> CancelAgentRunResponse;
run_agent_from_news(news_ids) -> AgentRunResult;
run_agent_from_account_trigger(trigger_id) -> AgentRunResult;
run_scheduled_agent_review(reason) -> AgentRunResult;
build_realtime_decision_packet(trigger, profile) -> RealtimeDecisionPacket;
select_run_profile(trigger) -> AgentRunProfile;
register_tools_for_profile(profile) -> AgentRuntimeToolSpec[];
record_decision_episode(run_id, episode) -> EpisodeId;
record_trade_intent(run_id, episode_id, account_input) -> IntentId;
record_decision_review(episode_id, review) -> ReviewId;
recover_interrupted_agent_runs(now) -> RecoverySummary;
recover_submitted_trade_intents(now) -> RecoverySummary;
```

规则：

- Runtime API 可以调用 Infra `run_agent_loop`，但 Infra 不反向调用 Runtime。
- `build_realtime_decision_packet` 只读 Quotes / News / Account facade，不读取内部表。
- `register_tools_for_profile` 必须按 profile 裁剪工具，不得默认全量暴露。

---

## 10. 可观测性

Runtime 每个 loop 必须有 heartbeat：

```ts
type SchedulerHeartbeat = {
  loopName: string;
  lastOkAt?: string;
  lastErrorAt?: string;
  lastError?: string;
  consecutiveFailures: number;
};
```

前端 Settings / Diagnostics 可展示：

- News refresh 状态。
- Quotes subscribed refresh 状态。
- Quotes universe refresh 状态。
- Account trigger evaluation 状态。
- Agent news batch 状态。
- Agent account trigger routing 状态。
- 最近 AgentRun、DecisionEpisode、TradeIntent、DecisionReview。

---

## 11. 实现映射

推荐代码位置：

```text
pipeline/agent_runtime/
  runs.rs            AgentRun / profile / run lifecycle
  packet.rs          RealtimeDecisionPacket builder
  tools.rs           profile -> ToolRegistry wiring
  decisions.rs       DecisionEpisode / EvidenceRef / TradeIntent / DecisionReview
  events.rs          AppEventEnvelope / constants / emit helpers
  router.rs          event listeners -> use case dispatch
  locks.rs           in-flight lock / watchdog
  heartbeat.rs       loop health

pipeline/scheduler.rs
  legacy entry; may delegate to pipeline/agent_runtime
```

规则：

- 如果某个 loop 需要构造 adapter-only tool registry，可以由 adapter 启动，但它仍应遵循本 spec。
- Runtime 只编排和记录业务运行期事实，不内嵌交易 / 新闻 / 行情底层规则。

---

## 12. 验收标准

- News spec 中 `news-refreshed` 不直接触发 Agent；Runtime 监听并路由。
- Account spec 中 `account-triggered` 不直接触发 Agent；Runtime 幂等路由。
- Account 的 subscribed codes 由 Runtime 注入 Quotes refresh scope。
- Quotes refresh 完成后，Runtime 触发 Account snapshot 重建。
- 同一 `trigger_id` 不会导致重复 Agent 交易动作。
- 每类 run 的 allowed tools 由 `AgentRunProfile` 决定；Infra 不默认暴露所有工具。
- 禁止交易写的 profile 不能调用 `operate_account`。
- 模型形成投资判断必须通过 `record_decision_episode` 持久化；`no_action` 和自选变更也不能只停留在自然语言输出。
- Agent 基于投资判断发起的自选变更必须通过 `update_watchlist.episodeId` 关联对应 episode。
- 每次交易写动作都有 `DecisionEpisode`、`TradeIntent` 和 `operate_account` 调用记录。
- 每次保护条件触发、订单终态、平仓或定时复盘可以生成 `DecisionReview`。
- `DecisionReview` 可以包含策略调整建议，但不会自动修改 active `StrategyCard`。
- 策略卡作为 Runtime 上下文注入；显式 `upsert_strategy_card` 后下一次 run 使用最新内容。
- 后台任务失败有 heartbeat 和日志，不会静默失效。
- Quotes / News / Account 任一层不 import Agent 代码。
- Runtime 不拥有交易判断逻辑，不绕过 Agent 和 Account。
