# Agent Infra 模块 Spec

> 本文档定义 Agent 的执行基础设施：模型渠道、消息格式、上下文管理、工具注册 / 调用协议、流式事件和基础 loop。
>
> Agent 在本产品里的业务运行方式、允许使用哪些工具、如何记录投资判断和复盘，见 [agent-runtime-module.md](agent-runtime-module.md)。
>
> 本文档只描述最新设计目标，作为实现依据。

## 一句话定位

**Agent Infra 是 LLM Agent 的执行底座**：它把不同 provider 的 wire format 统一成一套 canonical loop，负责消息、上下文、**Skill 注册与调用协议**、stream event 和 context compaction。

它不决定"该不该交易"，也不拥有投资判断记录。Runtime 启动一次 Agent run 后，Infra 只负责可靠执行：

```text
canonical request
  -> system prompt（含 skill 清单 + 协议）
  -> provider stream（chat text）
  -> SkillCallParser 检测 <use_skill> 闭合
  -> SkillRegistry dispatch
  -> 把 <skill_result> 包成下一轮 user message
  -> continue / finalize
  -> usage / error / compact event
```

**为什么走 Skill 文本协议而不是 provider tool_use**：

- 同一套 Skill 清单 + 系统提示词协议适配所有 chat-completable provider（Anthropic、OpenAI、本地 Llama / Qwen / 豆包等），不用为每家 wire format 维护 tool_use / function_call adapter
- Skill 形态天然支持产品负责人手写 SKILL.md 描述行为，agent 行为透明可读
- Skill 可以引用其他 skill、可以包含 reference data，不受 provider tool schema 限制
- AgentMessage 历史是纯文本，审计 / replay 时 grep 即可，不用解码 tool_use block
- 不需要管 provider server-side tool（web_search 等）—— 业务能力全部由我们的 Skill 提供

契约强度：

- `AgentMessage`、`SkillSpec`、`SkillCall`、`AgentEvent`、`ProviderChannel`、context compaction 顺序、Skill 调用文本协议（`<use_skill>` / `<skill_result>`）是 `Spec-as-source`。
- provider wire-format mapping、token 估算策略、Skill 加载方式是 `Spec-anchored`。
- `AgentRun`、`DecisionEpisode`、`EvidenceRef`、`TradeIntent`、`StrategyCard`、`DecisionReview` 属于 Agent Runtime。

共享类型见 [shared-types.md](shared-types.md)。

---

## 1. 责任边界

Agent Infra 负责：

- 统一 Anthropic Messages、OpenAI Responses、OpenAI-compatible Chat Completions 等 provider wire format（**纯 chat**，不使用各 provider 的 tool_use / function_calling 原生机制）。
- 表达和持久化对话消息、可选 thinking、skill 调用 / 结果文本（以 XML 标签嵌在 chat text 中）。
- 构造 provider 可接受的 canonical request，包括把 enabled `SkillSpec` 集合编译成 system prompt skill 清单。
- 管理 context window、压缩、易腐 skill 结果清理和 context-too-long reactive retry。
- 提供 `SkillRegistry`，注册、校验、分发和超时控制 skill。
- 记录所有 skill 调用的 `SkillCall` 审计（input / output 摘要 + payload ref）。
- 把 provider stream 和 skill lifecycle 转成统一 `AgentEvent` 给前端 / Runtime 消费。
- 实现 `SkillCallParser`：在 stream 中扫描 `<use_skill>` 闭合 → 缓冲 → 解析 → dispatch。
- 实现 `PayloadStore`：持久化 SkillCall 的完整 input / output payload，用于 decision episode replay。
- 执行基础 Agent loop，限制最大 turn 数，避免无限 skill 循环。

Agent Infra 不负责：

- 监听 News / Account / Quotes 事件并决定何时启动 Agent run。
- 决定某类 run 允许使用哪些 skill。
- 构造投资决策 packet。
- 判断新闻重要性、是否交易、是否调仓。
- 记录 `DecisionEpisode`、`TradeIntent`、`DecisionReview`。
- 管理策略卡生命周期或策略注入规则。
- 直接调用 Quotes / News / Account 内部实现。
- 直接写账户、持仓、订单、新闻或行情数据。
- 使用 provider 自带的 server-side tool（web_search / code_interpreter / file_search 等）。

边界规则：

- Infra 只认识通用 `SkillSpec` / `SkillCall`，不内嵌具体业务 skill 策略。
- 具体业务 skill（fetch_quote / news_search / operate_account 等）由 Runtime 或 adapter 注册进 `SkillRegistry`。
- Infra 可以拒绝未注册 skill，但不能自己决定"本次 run 可否交易"。
- Provider 层只负责 chat 请求与流式响应，不参与 skill dispatch、不感知业务工具集。
- Quotes / News / Account 不 import Agent Infra。

---

## 2. 领域模型

### 核心概念

| 概念 | 含义 | 身份 |
|---|---|---|
| `AgentMessage` | provider 上下文和聊天历史的持久化消息 | `message_id` |
| `SkillSpec` | 一个可注册 skill 的协议描述（含 markdown 描述 + input schema + 示例） | `skill_name` |
| `SkillCall` | 一次 skill 调用审计 | `skill_call_id` |
| `AgentEvent` | Agent loop 的统一流式事件 | 流内序号或 `event_id` |
| `ProviderChannel` | 模型渠道配置，适配不同 wire format | `channel_id` |
| `ContextBundle` | Runtime 交给 Infra 的上下文包 | `context_id` 或 run 内临时 ID |
| `PayloadStore` | SkillCall input / output 完整副本存储，用于 replay | `payload_id` |

### 不变量

- 每次 provider 调用必须使用 canonical request，不把业务 DTO 直接塞给 provider adapter。
- 所有 skill 调用必须先通过 `SkillRegistry` 校验。
- 所有 skill 调用都必须记录 `SkillCall`。
- 未注册 skill 必须拒绝（parser 检测到未知 skill name 时返回 `<skill_error>` 给模型，不进入 dispatch）。
- Skill 超时、provider 错误、context-too-long 必须转换为统一 `AgentEvent.error` 或 skill error。
- Infra 不能绕过 Runtime 的 run policy 调用 skill。
- 交易写 skill 结果、账户确认结果不属于易腐内容，context 压缩时不能丢失审计摘要。
- 易腐 skill 结果在上下文中可压缩成 stub，但 `agent_messages` / `agent_skill_calls` 持久化记录不动；stub 必须含 `ref=<payload_id>` 让 replay 能从 `PayloadStore` 拉回原文。
- Provider 请求中不传 `tools` 数组、不解析 `tool_use` / `function_call` block；skill 调用走 §2 定义的 XML 文本协议。

### Skill 调用文本协议

LLM 在 chat 文本中用 XML 标签发起 skill 调用 / 接收结果。**协议固定，不允许扩展或换格式**：

```text
LLM 输出：
  ……一些自然语言推理……
  <use_skill name="fetch_quote">{"tsCode": "600519.SH"}</use_skill>

我们解析后回传（作为下一轮 user message 的 text block）：
  <skill_result name="fetch_quote" call_id="sc_abc123">
  {"price": "1820.5", "freshness": {"status": "fresh", "ageMs": 1200}, ...}
  </skill_result>

失败时回传：
  <skill_error name="fetch_quote" call_id="sc_abc123" code="quote_missing">
  {"message": "no snapshot for 600519.SH"}
  </skill_error>
```

规则：

- 标签名一律小写：`use_skill` / `skill_result` / `skill_error` / `skill_result_stub`。
- `name` 属性必填，值是 `SkillRegistry` 中注册的 skill name。
- `<use_skill>` 内容必须是合法 JSON（即 `SkillSpec.inputSchema` 校验的 input）。
- `<skill_result>` 内容是 JSON；`<skill_error>` 内容是 JSON 且必须带 `code` 属性（取 `ErrorCode` 封闭集合）。
- `call_id` 由 Infra 在 parser 检测到 `<use_skill>` 闭合时生成（`sc_<uuid>`），写入回传标签，供模型在后续推理中显式引用某次结果。
- LLM 输出单 turn 内可以有多个 `<use_skill>`；Infra 按出现顺序串行 dispatch（不并行），逐个回传 `<skill_result>`（支持批量工具调用）。
- **工具调用是本轮文本的逻辑终点（best practice，对齐 native tool-calling 语义）**：
  - 首个 `<use_skill>` **之前**的文本（preamble / 推理）实时 emit 为 `text_delta`。
  - 首个 `<use_skill>` **之后**的文本是模型在「无工具结果」下的推测续写（hallucination）——**不 emit、不作权威输出**（native 协议里模型在工具调用处即 `stop_reason=tool_use` 结束本轮；文本协议下模型可能继续吐字，按此规则丢弃）。
  - 同一 turn 内多个 `<use_skill>` 仍**全部按序收集 + dispatch**；skill 结果在**下一轮** user message 回灌。
  - 因此事件顺序天然是 `text_delta…（preamble）→ skill_start/skill_end…`，无需文本与 skill 交错。
- 模型在自然语言中提到"我想调用 fetch_quote"但**没**输出闭合标签时，**不**触发 dispatch；这是 chat 文本，不是调用。
- 标签嵌套不合法（例如 `<use_skill>` 内出现 `<use_skill>`）→ `<skill_error>` `parse_error`。
- 未闭合标签（流到 turn 结束仍未见 `</use_skill>`）→ 当 turn 文本处理；不 dispatch。

### `AgentMessage`

```ts
type AgentMessageRole = "system" | "user" | "assistant";

type AgentMessageBlock =
  | { type: "text"; text: string }
  | { type: "image"; mimeType: string; dataRef: string }
  | { type: "thinking"; text: string; provider?: string; metadata?: JsonValue };

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
- Skill 调用 / 结果以 XML 标签嵌在 `text` block 中，**不**作为独立 block type；这是审计可读 + provider 通用的关键。
- Skill 调用 audit 真源是 `SkillCall`；chat 历史中的 `<use_skill>` / `<skill_result>` 是给 LLM / 用户看的副本。
- `dataRef` 是图片在 PayloadStore / 本地文件系统的 URI（例如 `payload://pl_abc123` 或 `file:///path/to.png`）；**不是 base64 数据**。Provider adapter 在 build wire 时负责 dereference → 读字节 → base64 编码 → 塞 wire format。Dereference 失败必须返回 `ParseError` 而非静默丢弃。
- thinking 是否持久化取决于 provider 支持和配置；跨 provider 不保证恢复。Anthropic extended thinking 模型要求保留 `signature`；adapter 在写 `thinking` block 时必须保存 provider-specific metadata（如 Anthropic 的 signature / redacted）到 `metadata` 字段。
- role 和 block 的允许组合：

| role | 允许 block.type |
|---|---|
| `system` | `text` |
| `user` | `text`、`image` |
| `assistant` | `text`、`thinking` |

`tool` role 不存在；skill_result 以 `user` role + text block（含 `<skill_result>` XML）形式回写。

### `SkillSpec` / `SkillRegistry`

```ts
type SideEffect = "none" | "non_trading_write" | "trading_write";

type SkillSpec = {
  name: string;             // 例如 "fetch_quote"、"news_search"、"operate_account"
  description: string;      // markdown，一段话说明 skill 用途和典型场景（注入 system prompt）
  inputSchema: JsonSchema;  // skill 调用 input 的 JSON schema（dispatch 前校验）
  examples: string[];       // 至少 1 个完整 `<use_skill ...>{...}</use_skill>` 示例字符串
  sideEffect: SideEffect;
  timeoutMs: number;        // dispatch 超时
};

type SkillRegistrySnapshot = {
  skills: SkillSpec[];
  registeredAt: OccurredAt;
};
```

规则：

- Infra 只定义注册协议，不规定产品里必须有哪些 skill。
- 具体产品在 Runtime spec 中规定 canonical skill name union；本项目使用 Agent Runtime 的 `AgentSkillName`。
- Runtime 决定每类 run 的 enabled skills，把对应 `SkillSpec` 注册进本次 loop。
- `sideEffect = "trading_write"` 的 skill 必须由 Runtime 显式允许，Infra 默认不得注册到非交易 run。
- 同名 skill 只能注册一次；重复注册必须 fail closed。
- Skill input 必须按 `inputSchema` 校验；校验失败包装为 `<skill_error>` 回传，不调用 handler。
- Skill output 必须转换成可摘要的 `JsonSummary`，供 stream / 审计 / chat 历史复用。
- Skill 描述和示例应当能被产品负责人手写为 markdown（例如 `skills/<name>/SKILL.md` 或编译期 `include_str!`），不要塞业务逻辑代码到描述里。

### System Prompt Skill 清单

每次 Agent loop 启动时，Infra 用 `SystemPromptBuilder` 把 enabled `SkillSpec` 集合编译成一段 system prompt 前缀，自动 prepend 到 `ContextBundle.systemParts`：

```text
你可以使用以下 skill。要调用某个 skill，输出 XML 标签
`<use_skill name="...">{...}</use_skill>`，内容是符合该 skill input schema 的 JSON。
每次调用后会以 `<skill_result name="..." call_id="...">` 形式回复给你。

## fetch_quote
获取单只标的实时行情快照。
Input: {"tsCode": "string，6位+.SH/.SZ/.BJ"}
Example: <use_skill name="fetch_quote">{"tsCode": "600519.SH"}</use_skill>

## news_search
...
```

规则：

- 注入顺序：固定 protocol 说明 → skill 列表（按 name 字典序，保证 prompt cache hit 一致）。
- 每个 skill 段：`## <name>` + description + `Input:` schema 摘要 + 至少 1 个 example。
- system prompt 中的 skill 清单部分**不允许由 LLM 修改 / 看不见**；Runtime 注入后只读。

### `SkillCall`

```ts
type SkillCall = {
  skillCallId: string;          // sc_<uuid>，由 Infra 在 parser 检测到 <use_skill> 闭合时生成
  runId: string;
  name: string;
  inputSummary: JsonSummary;    // input 序列化后 ≤ 8KB 时 = 完整 payload；> 8KB 时是截断摘要
  inputPayloadRef?: string;     // PayloadStore ref，> 8KB 时使用
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

- `name` 必须是本次 `SkillRegistry` 中已注册 skill name。
- Skill 被业务决策引用时，Runtime 可把 `SkillCall` 转成 `EvidenceRef`；Infra 不决定证据归属。
- 拒绝型业务结果不一定是 `isError = true`，例如 Account 拒单应由 skill output 表达业务原因（包含 `rejectionReason` 字段）。
- **PayloadStore 双层存储**（解决 LLM 视野 vs 长期审计的张力）：
  - 任何 skill 调用都会**同时**写入：
    1. chat 历史中的 `<skill_result>` text block（LLM 视野，可被 context compaction 替换为 stub）
    2. `agent_payloads` 表中的完整 input / output 副本（持久化，不受 compaction 影响）
  - 当 input / output JSON 序列化后**超过 8KB** 时，`SkillCall` 行的 `inputSummary` / `outputSummary` 只存截断摘要（前 1KB + `"[truncated, see ref]"`），完整数据走 `inputPayloadRef` / `outputPayloadRef`。
  - 当 input / output 小于阈值时，summary 字段 = 完整 payload 内容，ref 字段为空。
- 当 context compaction 把某条 `<skill_result>` 在 chat 历史中替换为 stub 时，stub 文本格式必须为 `<skill_result_stub name="..." call_id="..." ref="..." />`，模型可以读 stub 知道历史发生过这次调用，但 inline 数据已折叠；replay 时通过 ref 从 `agent_payloads` 拉回。
- LLM 视野优先 inline 全文；PayloadStore 是审计 / replay 用的并行存储，**不**给 LLM 当下读，而是给 decision episode 回看 / 用户复盘用。

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
  | "skill_error"
  | "context_limit"
  | "error";

type AgentEvent =
  | { type: "run_start"; runId: string; trigger: string; model: string }
  | { type: "text_delta"; runId: string; delta: string }
  | { type: "thinking_delta"; runId: string; delta: string }
  | { type: "skill_start"; runId: string; skillCallId: string; name: string; inputSummary: JsonSummary }
  | { type: "skill_end"; runId: string; skillCallId: string; name: string; outputSummary: JsonSummary; isError: boolean; durationMs: number }
  | { type: "compacted"; runId: string; tier: "micro_clear" | "summarize" | "drop" | "reactive_retry"; droppedMessages: number; estimatedTokensSaved?: number }
  | { type: "usage"; runId: string; inputTokens: number; outputTokens: number; cacheReadTokens?: number; cacheWriteTokens?: number }
  | { type: "done"; runId: string; stopReason: AgentStopReason; turns: number }
  | { type: "error"; runId: string; code: ErrorCode; message: string };
```

规则：

- `AgentEvent` 是 loop 执行事件，不是业务领域事件。
- Runtime 可以监听 `skill_end`、`done`、`error` 来更新 `AgentRun` 状态和业务审计记录。
- 后台 run 也必须产生事件流；前端可选择折叠展示。
- `text_delta` 只 emit 首个 `<use_skill>` **之前**的 preamble 文本（实时、与原始 LLM 输出顺序一致）；首个 skill 之后的文本按上文「工具调用是本轮文本逻辑终点」规则抑制，不 emit。
- Skill input / output 在 event 内是摘要；完整 payload 通过 `skillCallId` 查 `agent_skill_calls` + `agent_payloads`。
- `compacted.tier = "reactive_retry"` 专用于 §4 描述的 context-too-long 触发的压缩。
- `error.code` 必须取自 `ErrorCode` 封闭集合（shared-types §5）。

### `ProviderChannel`

Channel reference：

- [Anthropic Messages](references/agent/anthropic-messages.md)
- [OpenAI Responses](references/agent/openai-responses.md)
- [OpenAI Chat Completions](references/agent/openai-chat-completions.md)

```ts
type ProviderChannel = {
  channelId: string;
  provider: string;               // 用户添加渠道时输入的「渠道名」，也是展示用 provider name；
                                   // 前端按 `{model} ({provider})` 展示。无单独 provider-id 字段。
  wireFormat: "messages" | "responses" | "chat_completions";
  baseUrl?: string;               // host；快速预设自动填，自定义由用户输入
  apiKey: string;                 // 鉴权 token；持久化但**只写不读**（见下）
  model: string;
  stream: true;
  enabled: boolean;               // 渠道开关，默认 true
  supportsVision: boolean;
  supportsThinking: boolean;
  maxOutputTokens?: number;       // 模型生成上限，写入 provider request
  contextWindowTokens?: number;   // 上下文窗口大小，驱动 soft / hard limit 计算
};
```

规则：

- **抽象轴是 wire format（消息格式），不是厂商**。新增兼容厂商=加一条渠道配置：`chat_completions` 覆盖 OpenAI / DeepSeek / GLM / Moonshot / 本地 OpenAI 兼容服务（改 baseUrl + model 即可），`messages` 覆盖 Anthropic，`responses` 覆盖 OpenAI Responses。不为每家厂商写 provider-specific 代码。
- `provider` 是**用户输入的渠道名**（展示语义），不是受控的厂商枚举；同一 wireFormat 可以有多条不同 `provider` 名的渠道（如 "DeepSeek"、"我的本地 Qwen"）。
- `apiKey` 持久化在渠道行；但**只写不读**：list / get 等返回给前端的 DTO 必须屏蔽 apiKey（只回 `apiKeySet: boolean` 之类），不回传明文。
- **渠道添加有两种方式**：
  1. **快速预设**：内置已知厂商（DeepSeek 官方 / OpenAI 官方 / Anthropic 官方），`provider` 名 + `wireFormat` + `baseUrl` 预置，用户只填 `apiKey`。
  2. **自定义**：用户填 `provider`(渠道名) + 选 `wireFormat` + `baseUrl` + `apiKey`。
- **模型发现 + 确认**：填完连接信息后，调对应 wireFormat 的 `/models` 接口发现可用模型（`chat_completions` / `responses` → `GET {baseUrl}/v1/models` + `Authorization: Bearer`；`messages` → `GET {baseUrl}/v1/models` + `x-api-key` + `anthropic-version`）。发现成功 → 列出让用户**勾选确认**保留；发现失败（接口不存在 / 网络错）→ 提示用户**手动输入模型名**（允许输入多个）确认。每个确认保留的模型各成一条 `ProviderChannel`（共享同一连接的 provider/wireFormat/baseUrl/apiKey，仅 model 不同）。
- **当前模型**：维护一个「当前渠道」（active channelId）；Agent run 默认走它。切换当前模型即切换底层渠道。
- Agent 内部使用 canonical request / event。
- Provider adapter 只负责 canonical chat request 和厂商 wire format 的互转；**不传 tools / functions 字段、不解析 tool_use / function_call block**。
- 主 Agent 渠道必须支持 streaming；不支持 streaming 的 provider 不能作为主渠道。
- `supportsVision = false` 时，含 `image` block 的 AgentMessage 必须被 Infra 在 build wire 前拒绝（返回 `InvalidInput`）。新加渠道默认 `supportsVision = false` / `supportsThinking = false`，可后续按模型细化。
- `supportsThinking = false` 时，含 `thinking` block 的 AgentMessage 在 build wire 时由 adapter 丢弃，不报错（thinking 只对支持模型有意义）。
- `agent_provider_channels` 表持久化 channel 配置（含 `api_key` / `enabled` / active 标记）；Infra 暴露 `ProviderChannelsRepo` 的 CRUD（add / update / remove / list / get_by_id）+ 模型发现 + active 渠道读写，Runtime / 设置页调用。
- 不再有 `supportsTools` / `supportsServerSideTools` 字段：所有 chat-completable provider 都通过 Skill 文本协议提供工具能力，没有 provider 差异。
- 具体 stream event、thinking、错误码映射写在 channel reference。

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
  kind: "system" | "realtime" | "chat" | "memory" | "skill_result_stub";
  content: string | JsonSummary;
  freshness?: Freshness;
  tokenEstimate?: number;
  droppable: boolean;
};
```

规则：

- Runtime 负责提供业务上下文内容；Infra 负责排序、压缩和 provider format 转换。
- Infra 把 `SystemPromptBuilder` 编译出的 skill 清单作为 `kind = "system"` 的 ContextPart 自动 prepend 到 `systemParts`，Runtime 不需要手动塞。
- Infra 不维护聊天历史；Runtime 每次 run 必须把需要续接的 `AgentMessage[]` 转换成 `chatParts` 注入。
- 当前交易事实必须来自 Runtime 本次提供的 realtime context 或本次 skill 调用。
- 历史聊天和 summary 只能作为交互上下文，不能替代实时行情 / 账户读取。
- `droppable = false` 的内容只允许在 hard failure 前保留；如果超限仍无法发送，必须 fail closed（`stop_reason = context_limit`）。
- Context compaction 只影响本次或后续 provider request 的上下文投影，不修改已经持久化的 `AgentMessage`、`SkillCall`、`DecisionEpisode` 或 evidence snapshot 或 PayloadStore。

### `PayloadStore`

```ts
type PayloadStoreEntry = {
  payloadId: string;          // pl_<uuid>
  kind: "skill_input" | "skill_output" | "image";
  contentJson?: JsonValue;    // skill input/output 走这条
  contentBytes?: Uint8Array;  // image 走 bytes
  contentType?: string;       // image 的 mime（image/png / image/jpeg / ...）
  byteSize: number;
  createdAt: OccurredAt;
};
```

规则：

- 持久化在 `agent_payloads` 表（含 BLOB 列给 image，TEXT 列给 JSON）。
- 写入触发：
  - skill input/output JSON 序列化后超过 **8KB**
  - 图片 attachment（任何尺寸都进 PayloadStore，AgentMessage 只存 `payload://pl_xxx` 引用）
- 第一阶段**不实现 GC / retention policy**；payload 永久保留，用于 decision episode replay。后续需要清理时由独立产品策略处理，不在 Infra 隐式删除。
- `payloadId` 在 `SkillCall.inputPayloadRef` / `SkillCall.outputPayloadRef` / `AgentMessageBlock::Image.dataRef`（形如 `payload://pl_xxx`）之间共享；不允许跨 BC 的对象引用 PayloadStore（agent 自闭环）。
- Provider adapter 在 build wire 时遇到 `dataRef` 必须先从 PayloadStore 拉 bytes，再 base64 编码塞 wire；拉不到返回 `ParseError` 终止本次 dispatch。

---

## 3. Agent Loop

```text
Runtime builds AgentRunRequest
  -> Infra builds canonical chat request
       - SystemPromptBuilder 注入 skill 清单到 systemParts
       - AgentMessage[] (含 inline <skill_result> XML) 注入 chatParts
       - Image dataRef 由 provider adapter 从 PayloadStore 解引用
  -> provider.stream()           // 纯 chat stream，不传 tools
  -> SkillCallParser 增量扫描
       - 文本输出 → emit text_delta
       - 遇 <use_skill ...> 闭合 → emit skill_start → SkillRegistry dispatch
       - dispatch 完成 → emit skill_end → 缓存 <skill_result> 文本
  -> turn 结束（provider stop OR 闭合 </use_skill> 后回写）
       - 若本 turn 触发了至少一次 dispatch：
           构造新一轮 user message，body 是按出现顺序串联的
           <skill_result name="..." call_id="...">{...}</skill_result>
           （失败的是 <skill_error>）
           然后继续 loop
       - 否则 finalize：emit usage / done(stop_reason=completed/provider_stop)
  -> Reactive retry on context-too-long（见 §4）
  -> Hard limit fail closed → done(stop_reason=context_limit)
```

约束：

- 每次 run 必须有最大 turn 数（默认由 Runtime 注入），防止无限 skill 循环。
- Skill 有超时（`SkillSpec.timeoutMs`）；超时作为 `<skill_error code="tool_timeout">` 回传给模型，**不**直接终止 loop。
- 所有 skill 调用都进入统一事件流（`skill_start` / `skill_end`）和 `SkillCall` 审计。
- Provider 返回 context-too-long 时，按 §4 的 reactive retry 策略：压缩一次 → 重试一次 → 如果仍失败 → `stop_reason = context_limit`，emit `error` event (`code = "provider_context_too_long"`)。
- Infra 不在 loop 内创建 `DecisionEpisode` 或 `TradeIntent`；这些由 Runtime 根据模型输出和 skill 结果记录。
- Stream 解析必须**实时**（不等整个 turn 结束）：用户能从 UI 看到 LLM 思考 + skill 调用进度。
- 同一 turn 内多个 `<use_skill>` 按出现顺序**串行** dispatch；不并行（保证 LLM 看到的 skill_result 顺序与发出顺序一致）。

**ProviderStream 实现归属**：Agent Infra 定义 `ProviderStream` trait（接 canonical request、产 stream of chunks）。HTTP + SSE 实现（reqwest 调 Anthropic / OpenAI、解 SSE event、转 stop_reason、聚合 usage）归 Agent Runtime 在 Phase 3 实现，因为它涉及 Runtime 的 `ProviderChannel` 选择 / 鉴权 / retry 策略。Infra 自带一个用于测试的 `ScriptedProvider`，能让 loop_executor 测试无网络运行。

---

## 4. 上下文管理

上下文由四类内容构成：

| 类型 | 内容 | 生命周期 |
|---|---|---|
| Identity / System | Agent 身份、运行纪律、skill 清单（由 SystemPromptBuilder 注入） | 长期，适合 cache |
| Realtime Packet | trigger、账户、行情、新闻、策略、近期 episode 摘要 | 每次 run 重建 |
| Chat Context | 用户最近对话、当前问题、历史 skill_result | 只服务交互 |
| Review / Memory | 用户偏好、复盘建议、策略说明 | 独立存储，按需注入 |

规则：

- Infra 只负责装配和压缩，不判断业务事实是否足够交易。
- `Chat Context` 只用于需要对话续接的 run；非交互后台 run 默认由 Runtime 提供 `Realtime Packet` 和 `Review / Memory`，不要求恢复完整聊天历史。
- 易腐 skill 结果不能长期保留为事实（如行情、K 线、新闻全文等可重新拉取的数据）。
- 交易写 skill 结果应保留操作确认摘要（订单 ID、成交价、Account event ID 等）。
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
| time-based micro clear | 距上一条 assistant 消息超过约 60 分钟 | 清理旧易腐 skill 结果（替换为 `<skill_result_stub />`），保留最近若干条 |
| soft limit | 估算 token 超过 `context_soft_limit_tokens` | 先 MicroClear；仍过大时进入 Summarize / Drop |
| summarize threshold | MicroClear 后仍超过 `context_summarize_threshold` | 调 compact 模型生成摘要边界 |
| manual compact | Runtime 请求 compact | 下一轮强制 Summarize |
| provider rejection | provider 返回 context-too-long | **Reactive retry**：调 `compact_context` 做一次激进压缩 → 重发同一 turn 请求 → 仍失败时 fail closed |
| hard limit | 尽力压缩后仍超过 `context_hard_limit_tokens` | 中止 run，`stop_reason = context_limit` |

**Reactive retry 语义**（解决 spec §3 / §5 之间的接口分工歧义）：

- `compact_context(context, policy) -> ContextBundle` 是**纯计算** API：根据策略对 ContextBundle 排序 / 丢弃 / 替换 stub，返回新 bundle。本身**不**包含重试逻辑。
- "压缩后重试"是 `loop_executor` 的职责，不在 `compact_context` 里：
  1. provider 返回 context-too-long（HTTP 400 或等价 error code，由 adapter 翻译成 canonical `ProviderContextTooLong` 错误）
  2. `loop_executor` catch 该错误，调 `compact_context(..., policy = ReactiveRetry)`
  3. 用压缩后的 bundle **重发同一 turn 请求**（保持 turn id、保持 skill_call 上下文）
  4. **最多重试 1 次**；第二次仍失败 → finalize loop，`stop_reason = context_limit`，emit `error event (code = provider_context_too_long)` + `done event`
- `ReactiveRetry` 策略比 `Summarize` 更激进：直接 Drop 最老一轮 API round（包括其 chat history + skill_results），不等 summarize 模型返回，**保证下一次发送一定更短**。

丢弃 / 压缩顺序：

```text
1. MicroClear 易腐 skill 结果（替换为 <skill_result_stub />）
2. Summarize 尾窗外历史对话
3. Drop 最旧 API round
4. Reactive retry（一次压缩 + 一次重发）
5. HardLimit fail closed → stop_reason = context_limit
```

易腐 skill 结果（可被 MicroClear / Drop / 替换为 stub）：

- 行情、K 线、分时、扫描结果
- 新闻、正文、搜索结果
- 账户读模型快照

不可清理（永远保留 inline，禁止替换 stub）：

- 交易写 skill 结果（operate_account 的 order_id / fill 等审计摘要）
- 策略卡写入结果（Runtime 的 strategy 操作）
- 账户确认结果（Account event 主键 + 状态）

原则：

- 所有可重新读取的数据 skill 结果都属于可清理对象（替换 stub 后 LLM 仍可通过 `<use_skill>` 重新拉取）。
- 交易写 skill、策略写入、账户确认结果不属于可清理对象。
- 易腐 skill 结果替换成 stub 时，**必须保留 `name` + `call_id` + `ref`**，让 replay 能通过 PayloadStore 拉回完整 payload。
- Compact 模型可以独立配置（`compact_channel: ProviderChannel`）；未配置时使用当前 run 的 provider channel / model。
- Summarize 输出必须是中文结构化摘要，至少覆盖关注标的、已建立判断、未决问题、风险纪律、用户偏好、上一轮上下文。
- Summary 是续接上下文，不是事实真源；当前行情和账户状态仍必须重新读取。
- 每次 compact 必须 emit `AgentEvent.compacted`，含 `tier` + `droppedMessages` + `estimatedTokensSaved`。

---

## 5. 对外接口

### Infra Loop API

```rust
run_agent_loop(request, registry, context, event_tx) -> RunSummary;
estimate_context_tokens(context, channel) -> TokenEstimate;
compact_context(context, policy) -> ContextBundle;
```

规则：

- `request` 必须包含 `runId`、provider channel、model、max turns。**不**含 server-side tool 字段。
- `registry` 是本次 run 的 `SkillRegistry` 实例，含 Runtime 本次允许的 skill 集合。
- `context` 由 Runtime 构造；Infra 不主动读取 Quotes / News / Account。Skill 清单在 build 时由 `SystemPromptBuilder` prepend 到 `systemParts`，Runtime 不需要手动塞。
- `event_tx` 接收统一 `AgentEvent`，供 Runtime 和 UI 订阅。
- `compact_context` 是纯计算 API，**不**做 retry；retry 由 `run_agent_loop` 内部 orchestration（见 §4）。

### Skill Registry API

```rust
register_skill(spec: SkillSpec, handler: Arc<dyn SkillHandler>) -> Result<()>;
validate_skill_input(skill_name: &str, input: &JsonValue) -> Result<()>;
dispatch_skill_call(run_id: &str, skill_call_id: SkillCallId, skill_name: &str, input: JsonValue)
    -> SkillCallResult;
list_skills() -> Vec<SkillSpec>;        // 用于 SystemPromptBuilder 拉清单
has_skill(name: &str) -> bool;

trait SkillHandler: Send + Sync + 'static {
    fn invoke(&self, inv: SkillInvocation) -> SkillHandlerFuture;
    // SkillInvocation 含 run_id / skill_call_id / input；handler 返回 SkillHandlerOutput
}
```

规则：

- handler 位于 adapter 或 Runtime wiring，不放在 provider adapter 中。
- `dispatch_skill_call` 必须记录 `SkillCall` 开始和结束；大 payload 自动走 PayloadStore（§2 规则）。
- `skill_call_id` 由 Infra 在 `SkillCallParser` 检测到 `<use_skill>` 闭合时生成；caller 不传 id。
- 任何 skill 执行失败都必须返回结构化错误（`<skill_error code="..." />`），不 panic 终止 loop。
- `validate_skill_input` 校验失败包装为 `<skill_error code="invalid_input" />` 回传给模型。

### SystemPromptBuilder API

```rust
build_system_prompt(skills: &[SkillSpec], base_prompt: &str) -> String;
```

把 enabled skills 按字典序编译成 markdown 清单，prepend protocol 说明 + `base_prompt`。Builder 是纯计算，无 I/O。

### SkillCallParser API

```rust
struct SkillCallParser { /* state machine */ }
impl SkillCallParser {
    fn new() -> Self;
    fn feed(&mut self, chunk: &str) -> Vec<ParserEvent>;
    fn finalize(&mut self) -> Vec<ParserEvent>;  // turn 结束时调用
}

enum ParserEvent {
    TextDelta(String),                                  // emit 给 stream
    UseSkill { name: String, input: JsonValue },        // dispatch
    ParseError { reason: String, partial: String },     // 标签格式坏 → skill_error
}
```

规则：

- Parser 实时增量扫描 provider stream；不缓冲整 turn。
- 检测到 `<use_skill name="X">` 后**只**缓冲到 `</use_skill>` 闭合，期间不 emit text_delta（避免泄漏 raw XML 给 UI）。
- 闭合后解析 JSON：成功 → emit `UseSkill`；失败 → emit `ParseError`。
- 流到 turn 结束仍未闭合的 `<use_skill>` 当 text 处理（不 dispatch）。

### ProviderChannels Repo API

```rust
ProviderChannelsRepo::add(channel: ProviderChannel) -> Result<()>;
ProviderChannelsRepo::update(channel: ProviderChannel) -> Result<()>;
ProviderChannelsRepo::remove(channel_id: &str) -> Result<()>;
ProviderChannelsRepo::get(channel_id: &str) -> Option<ProviderChannel>;
ProviderChannelsRepo::list() -> Vec<ProviderChannel>;
ProviderChannelsRepo::set_active(channel_id: &str) -> Result<()>;
ProviderChannelsRepo::active() -> Option<ProviderChannel>;
```

Runtime / 设置页通过这个 repo 管理 `agent_provider_channels` 表 + 当前渠道。

### 模型发现 API

```rust
// 用给定连接信息调对应 wireFormat 的 /models 接口，返回可用 model id 列表。
// 发现失败（接口缺失 / 网络错）→ Err，调用方据此引导用户手填模型名。
discover_models(wire_format: WireFormat, base_url: &str, api_key: &str)
    -> Result<Vec<DiscoveredModel>>;   // DiscoveredModel { id, displayName? }
```

- `chat_completions` / `responses`：`GET {base_url}/v1/models`，`Authorization: Bearer <api_key>`，解 `{data:[{id}]}`。
- `messages`：`GET {base_url}/v1/models`，header `x-api-key` + `anthropic-version`，解 `{data:[{id, display_name}]}`。
- 这是普通 GET（非 streaming），属于渠道连通性能力，可在 Infra/adapter 实现；与「streaming `ProviderStream` 实现归 Runtime/Phase 3」不冲突。
- 调用方（设置页）拿到列表后让用户勾选确认；发现失败时允许手动输入一个或多个模型名确认。每个确认的模型物化成一条渠道。

### 前端命令（设置页）

设置页通过 specta 强类型 command 操作（不裸调 invoke、apiKey 只提交不回读）：`agent_list_channels`（屏蔽 apiKey）/ `agent_add_channel` / `agent_remove_channel` / `agent_set_active_channel` / `agent_discover_models` / `agent_channel_presets`（返回内置快速预设）。

### 前端消息入口

用户消息入口由 Runtime 拥有；Infra 只要求用户消息最终转换为 `AgentMessage` 和 `ContextBundle` 后进入 loop。

---

## 6. 验收标准 / 例子

- 同一套 Infra loop 支持 `/messages`、`/responses`、`/chat/completions` 三类 wire format，**全部走纯 chat**——provider request 中不传 `tools` 字段、不解析 `tool_use` / `function_call` block。
- 渠道按 wireFormat 添加：快速预设（DeepSeek/OpenAI/Anthropic 官方，仅填 key）或自定义（渠道名 + 格式 + host + key）；保存后调对应格式 `/models` 发现模型让用户确认，发现失败可手填多个模型名；每个确认模型成一条渠道，前端按 `{model} ({渠道名})` 展示，apiKey 不回显。
- 主渠道支持 streaming；前端能看到 `run_start`、`text_delta`、`thinking_delta`（如有）、`skill_start` / `skill_end`、`usage`、`done` / `error`。
- 未注册 skill 被拒绝（parser 检测后返回 `<skill_error code="invalid_input">`），不会因 LLM 任意输出字符串触发 dispatch。
- Runtime 限制本次 run 不允许 `operate_account` skill 时，Infra 不会把它编译进 SystemPromptBuilder 的 skill 清单，模型在 system prompt 中看不到该 skill 存在。
- Provider context-too-long 后，Infra 能调一次 `compact_context(policy=ReactiveRetry)` 并重发同一 turn 请求；第二次仍失败则 `stop_reason = context_limit`，emit `error event (code=provider_context_too_long)`。
- 易腐 skill 结果被压缩成 `<skill_result_stub name="X" call_id="sc_..." ref="pl_..." />` 后，下一轮 prompt 仍是合法 chat 文本（无悬空 XML），且 ref 能在 `agent_payloads` 中查到原 payload。
- Skill 调用 input / output ≤ 8KB 时，`SkillCall.input/outputSummary` = 完整 payload；> 8KB 时 summary 是截断摘要，full payload 在 PayloadStore 中通过 ref 可拉回。
- 图片 attachment 走 PayloadStore：AgentMessage 只存 `payload://pl_xxx` 引用，provider adapter 在 build wire 时 dereference + base64 编码。
- Anthropic extended thinking 模型的 `thinking` block 跨 turn 时保留 `signature` 等 provider metadata，避免 422。
- Quotes / News / Account 不 import Agent Infra 代码；Agent BC 不反向 import 三个执行 BC。

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
