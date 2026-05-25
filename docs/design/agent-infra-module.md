# Agent Infra 模块 Spec

> 本文档定义 Agent 的执行基础设施：模型渠道、消息格式、上下文管理、工具注册 / 调用协议、流式事件和基础 loop。
>
> Agent 在本产品里的业务运行方式、允许使用哪些工具、如何记录投资判断和复盘，见 [agent-runtime-module.md](agent-runtime-module.md)。
>
> 本文档只描述最新设计目标，作为实现依据。

## 一句话定位

**Agent Infra 是 LLM Agent 的执行底座**：它把不同 provider 的 wire format 统一成一套 canonical loop，负责消息、上下文、工具注册、工具调用审计、stream event 和 context compaction。

它不决定“该不该交易”，也不拥有投资判断记录。Runtime 启动一次 Agent run 后，Infra 只负责可靠执行：

```text
canonical request
  -> provider stream
  -> tool_use
  -> ToolRegistry dispatch
  -> tool_result
  -> continue / finalize
  -> usage / error / compact event
```

契约强度：

- `AgentMessage`、`ToolSpec`、`ToolCall`、`AgentEvent`、`ProviderChannel`、context compaction 顺序是 `Spec-as-source`。
- provider wire-format mapping、server-side tool 映射、token 估算策略是 `Spec-anchored`。
- `AgentRun`、`DecisionEpisode`、`EvidenceRef`、`TradeIntent`、`StrategyCard`、`DecisionReview` 属于 Agent Runtime。

共享类型见 [shared-types.md](shared-types.md)。

---

## 1. 责任边界

Agent Infra 负责：

- 统一 Anthropic Messages、OpenAI Responses、OpenAI-compatible Chat Completions 等 provider wire format。
- 表达和持久化对话消息、tool_use / tool_result 摘要和可选 thinking。
- 构造 provider 可接受的 canonical request。
- 管理 context window、压缩、易腐工具结果清理和 context-too-long retry。
- 提供 `ToolRegistry`，注册、校验、分发和超时控制 local tools。
- 记录所有 local tool 和 provider server-side tool 的 `ToolCall` 审计。
- 把 provider stream 和 tool lifecycle 转成统一 `AgentEvent` 给前端 / Runtime 消费。
- 执行基础 Agent loop，限制最大 turn 数，避免无限工具循环。

Agent Infra 不负责：

- 监听 News / Account / Quotes 事件并决定何时启动 Agent run。
- 决定某类 run 允许使用哪些工具。
- 构造投资决策 packet。
- 判断新闻重要性、是否交易、是否调仓。
- 记录 `DecisionEpisode`、`TradeIntent`、`DecisionReview`。
- 管理策略卡生命周期或策略注入规则。
- 直接调用 Quotes / News / Account 内部实现。
- 直接写账户、持仓、订单、新闻或行情数据。

边界规则：

- Infra 只认识通用 `ToolSpec` / `ToolCall`，不内嵌具体业务工具策略。
- 具体业务工具由 Runtime 或 adapter 注册进 `ToolRegistry`。
- Infra 可以拒绝未注册 local tool，但不能自己决定“本次 run 可否交易”。
- Provider 层不执行 tool loop，不写业务状态。
- Quotes / News / Account 不 import Agent Infra。

---

## 2. 领域模型

### 核心概念

| 概念 | 含义 | 身份 |
|---|---|---|
| `AgentMessage` | provider 上下文和聊天历史的持久化消息 | `message_id` |
| `ToolSpec` | 一个可注册工具的协议描述 | `tool_name` |
| `ToolCall` | 一次工具调用审计 | `tool_call_id` |
| `AgentEvent` | Agent loop 的统一流式事件 | 流内序号或 `event_id` |
| `ProviderChannel` | 模型渠道配置，适配不同 wire format | `channel_id` |
| `ContextBundle` | Runtime 交给 Infra 的上下文包 | `context_id` 或 run 内临时 ID |

### 不变量

- 每次 provider 调用必须使用 canonical request，不把业务 DTO 直接塞给 provider adapter。
- 所有 local tool 调用必须先通过 `ToolRegistry` 校验。
- 所有 local tool 和 server-side tool 都必须记录 `ToolCall`。
- 未注册 local tool 必须拒绝，不能被动态字符串绕过。
- 工具超时、provider 错误、context-too-long 必须转换为统一 `AgentEvent.error` 或 tool error。
- Infra 不能绕过 Runtime 的 run policy 调用工具。
- 交易写工具结果、账户确认结果不属于易腐内容，context 压缩时不能丢失审计摘要。
- 易腐工具结果可压缩成 stub，但必须保留 provider 需要的 tool_use / tool_result 配对。

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

- `AgentMessage` 是 provider 上下文和聊天历史，不是投资判断。
- 图片使用 `dataRef` 指向本地附件或缓存，不把大二进制直接塞进长期消息表。
- 图片附件的持久化和生命周期由 Runtime / adapter 负责；Infra 只持有可读取的 `dataRef`。
- thinking 是否持久化取决于 provider 支持和配置；跨 provider 不保证恢复。
- 工具调用审计以 `ToolCall` 为准，message block 只保存对话上下文所需摘要。
- role 和 block 的允许组合必须按下表校验，避免 provider canonical 转换出现歧义：

| role | 允许 block.type |
|---|---|
| `system` | `text` |
| `user` | `text`、`image` |
| `assistant` | `text`、`thinking`、`tool_use` |
| `tool` | `tool_result` |

### `ToolSpec` / `ToolRegistry`

```ts
type ToolSpec = {
  name: string;
  description: string;
  inputSchema: JsonSchema;
  outputSchema?: JsonSchema;
  source: "local_tool";
  timeoutMs: number;
  sideEffect: "none" | "non_trading_write" | "trading_write";
};

type ToolRegistrySnapshot = {
  tools: ToolSpec[];
  registeredAt: OccurredAt;
};
```

规则：

- Infra 只定义注册协议，不规定产品里必须有哪些工具。
- 具体产品必须在 Runtime spec 中把 `ToolSpec.name` 缩窄成自己的 canonical tool name union；本项目使用 Agent Runtime 的 `AgentToolName`。
- Runtime 决定每类 run 的 `allowedTools`，并把对应 `ToolSpec` 注册进本次 loop。
- `sideEffect = "trading_write"` 的工具必须由 Runtime 显式允许，Infra 默认不得暴露。
- 同名 local tool 只能注册一次；重复注册必须 fail closed。
- Tool input 必须按 `inputSchema` 校验；校验失败作为 tool error 返回模型，不调用工具实现。
- Tool output 必须转换成可摘要的 `JsonSummary`，供 stream 和审计展示。

### `ToolCall`

```ts
type ToolCall = {
  toolCallId: string;
  runId: string;
  name: string;
  source: "local_tool" | "server_side_tool";
  inputSummary: JsonSummary;
  inputPayloadRef?: string;
  outputSummary?: JsonSummary;
  outputPayloadRef?: string;
  isError: boolean;
  errorCode?: ErrorCode;
  startedAt: OccurredAt;
  endedAt?: OccurredAt;
  durationMs?: number;
};
```

规则：

- `source = "local_tool"` 时，`name` 必须是本次 `ToolRegistry` 中已注册工具名。
- `source = "server_side_tool"` 时，`name` 可以是 provider 原生工具名，例如 `web_search`。
- Provider 原生工具不得绕过本地工具注册表调用 Quotes / Account / News 能力。
- 工具被业务决策引用时，Runtime 可把 `ToolCall` 转成 `EvidenceRef`；Infra 不决定证据归属。
- 拒绝型业务结果不一定是 `isError = true`，例如 Account 拒单应由工具 output 表达业务原因。
- `inputSummary` / `outputSummary` 是可前端展示的摘要，不是恢复算法的真源。
- 需要恢复副作用或审计精确结果的 local tool 必须持久化结构化 input / output payload，并通过 `inputPayloadRef` / `outputPayloadRef` 关联；例如 `operate_account` 必须能通过 `toolCallId` 读回 Account response 的完整结构。
- 只读工具可以只保存摘要；完整 payload 过大时可进入详情表或对象存储，但 ref 必须稳定可读。

### `AgentEvent`

```ts
type JsonSummary =
  | string
  | number
  | boolean
  | null
  | JsonSummary[]
  | { [key: string]: JsonSummary };

type AgentStopReason =
  | "completed"
  | "max_turns"
  | "cancelled"
  | "provider_stop"
  | "tool_error"
  | "context_limit"
  | "error";

type AgentEvent =
  | { type: "run_start"; runId: string; trigger: string; model: string }
  | { type: "text_delta"; runId: string; delta: string }
  | { type: "thinking_delta"; runId: string; delta: string }
  | { type: "tool_start"; runId: string; toolCallId: string; name: string; inputSummary: JsonSummary }
  | { type: "tool_end"; runId: string; toolCallId: string; name: string; outputSummary: JsonSummary; isError: boolean; durationMs: number }
  | { type: "compacted"; runId: string; tier: "micro_clear" | "summarize" | "drop" | "reactive_retry"; droppedMessages: number; estimatedTokensSaved?: number }
  | { type: "usage"; runId: string; inputTokens: number; outputTokens: number; cacheReadTokens?: number; cacheWriteTokens?: number }
  | { type: "done"; runId: string; stopReason: AgentStopReason; turns: number }
  | { type: "error"; runId: string; message: string };
```

规则：

- `AgentEvent` 是 loop 执行事件，不是业务领域事件。
- Runtime 可以监听 `tool_end`、`done`、`error` 来更新 `AgentRun` 状态和业务审计记录。
- 后台 run 也必须产生事件流；前端可选择折叠展示。
- 工具输入 / 输出可以摘要展示，完整 payload 可进入详情。

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
  supportsServerSideTools?: string[];
};
```

规则：

- Agent 内部使用 canonical request / event。
- Provider adapter 只负责 canonical request 和厂商 wire format 的互转。
- 主 Agent 渠道必须支持 streaming；不支持 streaming 的 provider 不能作为主渠道。
- 主交易 Agent 渠道必须支持 local tools；不支持 tools 的渠道只能用于非交易问答或禁用。
- 具体 stream event、tool call、thinking、web_search 和错误映射写在 channel reference。

### `ContextBundle`

```ts
type ContextBundle = {
  runId: string;
  systemParts: ContextPart[];
  realtimeParts: ContextPart[];
  chatParts: ContextPart[];
  memoryParts: ContextPart[];
};

type ContextPart = {
  kind: "system" | "realtime" | "chat" | "memory" | "tool_stub";
  content: string | JsonSummary;
  freshness?: Freshness;
  tokenEstimate?: number;
  droppable: boolean;
};
```

规则：

- Runtime 负责提供业务上下文内容；Infra 负责排序、压缩和 provider format 转换。
- Infra 不维护聊天历史；Runtime 每次 run 必须把需要续接的 `AgentMessage[]` 转换成 `chatParts` 注入。
- 当前交易事实必须来自 Runtime 本次提供的 realtime context 或本次工具调用。
- 历史聊天和 summary 只能作为交互上下文，不能替代实时行情 / 账户读取。
- `droppable = false` 的内容只允许在 hard failure 前保留；如果超限仍无法发送，必须 fail closed。
- Context compaction 只影响本次或后续 provider request 的上下文投影，不修改已经持久化的 `AgentMessage`、`ToolCall`、`DecisionEpisode` 或 evidence snapshot。

---

## 3. Agent Loop

```text
Runtime builds AgentRunRequest
  -> Infra builds canonical provider request
  -> provider.stream()
  -> text / thinking deltas
  -> assistant tool_use
  -> ToolRegistry dispatch
  -> append tool_result
  -> continue or finalize
  -> emit usage / done / error
```

约束：

- 每次 run 必须有最大 turn 数，防止无限工具循环。
- 工具有超时；超时作为 tool error 返回给模型。
- local tools 和 server-side tools 都进入统一事件流和 `ToolCall` 审计。
- Provider 返回 context-too-long 时，可以触发一次压缩后重试。
- Infra 不在 loop 内创建 `DecisionEpisode` 或 `TradeIntent`；这些由 Runtime 根据模型输出和工具结果记录。

---

## 4. 上下文管理

上下文由四类内容构成：

| 类型 | 内容 | 生命周期 |
|---|---|---|
| Identity / System | Agent 身份、运行纪律、工具规则 | 长期，适合 cache |
| Realtime Packet | trigger、账户、行情、新闻、策略、近期 episode 摘要 | 每次 run 重建 |
| Chat Context | 用户最近对话、当前问题 | 只服务交互 |
| Review / Memory | 用户偏好、复盘建议、策略说明 | 独立存储，按需注入 |

规则：

- Infra 只负责装配和压缩，不判断业务事实是否足够交易。
- `Chat Context` 只用于需要对话续接的 run；非交互后台 run 默认由 Runtime 提供 `Realtime Packet` 和 `Review / Memory`，不要求恢复完整聊天历史。
- 易腐工具结果不能长期保留为事实。
- 交易写工具结果应保留操作确认摘要。
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
| manual compact | Runtime 请求 compact | 下一轮强制 Summarize |
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

- 行情、K 线、分时、扫描。
- 新闻、正文、搜索结果。
- 账户读模型快照。
- provider server-side `web_search`。

原则：

- 所有可重新读取的数据工具结果都属于可清理对象。
- 交易写工具、策略写入、账户确认结果不属于可清理对象。
- 易腐工具结果替换成 stub 时，必须保留 call id，不能破坏 provider 的 tool_use / tool_result 配对。
- Compact 模型可以独立配置；未配置时使用当前 run 的 provider channel / model。
- Summarize 输出必须是中文结构化摘要，至少覆盖关注标的、已建立判断、未决问题、风险纪律、用户偏好、上一轮上下文。
- Summary 是续接上下文，不是事实真源；当前行情和账户状态仍必须重新读取。
- 每次 compact 必须 emit `AgentEvent.compacted`。

---

## 5. 对外接口

### Infra Loop API

```rust
run_agent_loop(request, registry, context, event_tx) -> RunSummary;
estimate_context_tokens(context, channel) -> TokenEstimate;
compact_context(context, policy) -> ContextBundle;
```

规则：

- `request` 必须包含 `runId`、provider channel、model、max turns、server-side tool 允许列表。
- `registry` 只包含 Runtime 本次允许的 local tools。
- `context` 由 Runtime 构造；Infra 不主动读取 Quotes / News / Account。
- `event_tx` 接收统一 `AgentEvent`，供 Runtime 和 UI 订阅。

### Tool Registry API

```rust
register_tool(spec, handler) -> Result<()>;
validate_tool_input(tool_name, input) -> Result<()>;
dispatch_tool_call(run_id, tool_name, input) -> ToolCallResult;
```

规则：

- handler 位于 adapter 或 Runtime wiring，不放在 provider adapter 中。
- `dispatch_tool_call` 必须记录 `ToolCall` 开始和结束。
- 任何工具执行失败都必须返回结构化错误，不 panic 终止 loop。

### 前端消息入口

用户消息入口由 Runtime 拥有；Infra 只要求用户消息最终转换为 `AgentMessage` 和 `ContextBundle` 后进入 loop。

---

## 6. 验收标准 / 例子

- 同一套 Infra loop 支持 `/messages`、`/responses`、`/chat/completions` 三类 wire format。
- 主渠道支持 streaming；前端能看到 `run_start`、文本增量、工具开始 / 结束、usage、done / error。
- 未注册 local tool 被拒绝，不会被任意字符串调用。
- Runtime 限制本次 run 不允许 `operate_account` 时，Infra 不会暴露该工具给 provider。
- Provider context-too-long 后，Infra 能按顺序压缩并重试一次；仍失败则 fail closed。
- 易腐工具结果被压缩后，provider tool_use / tool_result 配对仍合法。
- Quotes / News / Account 不 import Agent Infra 代码。

---

## 7. 不纳入范围

- 投资判断记录和复盘模型。
- 策略卡生命周期。
- 触发 Agent run 的调度逻辑。
- 业务工具选择策略。
- 行情 provider 接入。
- 新闻 provider 接入。
- 账户估值、成交模拟、T+1 和现金校验。
- 真券商交易。
- 自动保证收益。
