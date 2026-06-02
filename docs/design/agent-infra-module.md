# Agent Infra 模块 Spec

> 本文档定义 Agent 的执行基础设施：模型渠道、消息格式、上下文管理、Tool 注册 / 调用协议、流式事件和基础 loop。
>
> Agent 在本产品里的业务运行方式、允许使用哪些工具、如何记录投资判断和复盘，见 [agent-runtime-module.md](agent-runtime-module.md)。
>
> 本文档只描述最新设计目标，作为实现依据。

## 一句话定位

**Agent Infra 是 LLM Agent 的执行底座**：它把不同 provider 的 wire format 统一成一套 canonical loop，负责消息、上下文、**Tool 注册与调用协议**、stream event 和 context compaction。

它不决定"该不该交易"，也不拥有投资判断记录。Runtime 启动一次 Agent run 后，Infra 只负责可靠执行：

```text
canonical request
  -> system prompt（含 tool 清单 + 协议）
  -> provider stream（chat text）
  -> ToolCallParser 检测 <use_tool> 闭合
  -> ToolRegistry dispatch
  -> 把 <tool_result> 包成下一轮 user message
  -> continue / finalize
  -> usage / error / compact event
```

**为什么走 Tool 文本协议而不是 provider tool_use**（XML 标签嵌在纯 chat text 里，不调用各 provider 原生的 tool_use / function_calling）：

- 同一套 Tool 清单 + 系统提示词协议适配所有 chat-completable provider（Anthropic、OpenAI、本地 Llama / Qwen / 豆包等），不用为每家 wire format 维护 tool_use / function_call adapter
- Tool 描述以 markdown 注入 system prompt，产品负责人能直接读 / 写 tool 说明，agent 可调用能力透明可审
- AgentMessage 历史是纯文本，审计 / replay 时 grep `<use_tool>` / `<tool_result>` 即可，不用解码 provider 的 tool_use block
- 不需要管 provider server-side tool（web_search 等）—— 业务能力全部由我们注册的 Tool 提供

> 「被调用物」从历史命名的 `Skill` 正名为 `Tool`，**文本协议设计本身不变**（仍是 XML 标签嵌纯 chat）。Tool 之上由模型驱动的编排说明书叫 **Skill（playbook，`SKILL.md`）**，是另一层、不是注册 handler，归 Agent Runtime（见下方术语 note）。

契约强度：

- `AgentMessage`、`ToolSpec`、`ToolCall`、`AgentEvent`、`ProviderChannel`、context compaction 顺序、Tool 调用文本协议（`<use_tool>` / `<tool_result>`）是 `Spec-as-source`。
- provider wire-format mapping、token 估算策略、Tool 加载方式是 `Spec-anchored`。
- `AgentRun`、`DecisionEpisode`、`EvidenceRef`、`TradeIntent`、`StrategyCard`、`DecisionReview` 属于 Agent Runtime。

共享类型见 [shared-types.md](shared-types.md)。

---

## 术语：Tool vs Skill（已统一）

> **本模块的"可注册原语"在产品里的 canonical 名是 Tool。** 命名已统一——本文档全文用 `Tool*`（`ToolSpec`/`ToolCall`/`ToolRegistry`/`ToolCallParser`/`<use_tool>`/`<tool_result>`/`<tool_error>`），与 [agent-runtime-module.md](agent-runtime-module.md) 的 `Tool`/`ToolRegistry`/`AgentToolName` 一致，指**同一个机制**。两层模型：
>
> - **Tool（原语）** = 注册进 registry 的可调用能力（name + input schema + handler），in-process、结构化、经文本协议调用。本文档（Infra 层）只定义此层的注册 / 调用 / 审计机制。
> - **Skill（playbook）** = 模型驱动的 `SKILL.md` 说明书，编排若干 tool 完成任务，**不是注册 handler**，归 Agent Runtime / 产品层，**初始为空**。详见 [agent-runtime-module.md](agent-runtime-module.md) §Skills。
>
> 两者正交：Tool 是「手」，Skill 是「剧本」。文本协议（XML 标签嵌在纯 chat 里、不使用 provider 原生 tool_use）的设计理由见「一句话定位」下方——此次只是把被调用物从历史命名 `Skill*` 正名为 `Tool*`，**协议设计本身不变**。`SKILL.md`（playbook 文件名）保留 Skill 字样，因为它属 playbook 层、不是原语层。

---

## 1. 责任边界

Agent Infra 负责：

- 统一 Anthropic Messages、OpenAI Responses、OpenAI-compatible Chat Completions 等 provider wire format（**纯 chat**，不使用各 provider 的 tool_use / function_calling 原生机制）。
- 表达和持久化对话消息、可选 thinking、tool 调用 / 结果文本（以 XML 标签嵌在 chat text 中）。
- 构造 provider 可接受的 canonical request，包括把 enabled `ToolSpec` 集合编译成 system prompt tool 清单。
- 管理 context window、压缩、易腐 tool 结果清理和 context-too-long reactive retry。
- 提供 `ToolRegistry`，注册、校验、分发和超时控制 tool。
- 记录所有 tool 调用的 `ToolCall` 审计（input / output 摘要 + payload ref）。
- 把 provider stream 和 tool lifecycle 转成统一 `AgentEvent` 给前端 / Runtime 消费。
- 实现 `ToolCallParser`：在 stream 中扫描 `<use_tool>` 闭合 → 缓冲 → 解析 → dispatch。
- 实现 `PayloadStore`：持久化 ToolCall 的完整 input / output payload，用于 decision episode replay。
- 执行基础 Agent loop，限制最大 turn 数，避免无限 tool 循环。

Agent Infra 不负责：

- 监听 News / Account / Quotes 事件并决定何时启动 Agent run。
- 决定某类 run 允许使用哪些 tool。
- 构造投资决策 packet。
- 判断新闻重要性、是否交易、是否调仓。
- 记录 `DecisionEpisode`、`TradeIntent`、`DecisionReview`。
- 管理策略卡生命周期或策略注入规则。
- 直接调用 Quotes / News / Account 内部实现。
- 直接写账户、持仓、订单、新闻或行情数据。
- 使用 provider 自带的 server-side tool（web_search / code_interpreter / file_search 等）。

边界规则：

- Infra 只认识通用 `ToolSpec` / `ToolCall`，不内嵌具体业务 tool 策略。
- 具体业务 tool（fetch_quote / news_search / operate_account 等）由 Runtime 或 adapter 注册进 `ToolRegistry`。
- Infra 可以拒绝未注册 tool，但不能自己决定"本次 run 可否交易"。
- Provider 层只负责 chat 请求与流式响应，不参与 tool dispatch、不感知业务工具集。
- Quotes / News / Account 不 import Agent Infra。

---

## 2. 领域模型

### 核心概念

| 概念 | 含义 | 身份 |
|---|---|---|
| `AgentMessage` | provider 上下文和聊天历史的持久化消息 | `message_id` |
| `ToolSpec` | 一个可注册 tool 的协议描述（含 markdown 描述 + input schema + 示例） | `tool_name` |
| `ToolCall` | 一次 tool 调用审计 | `tool_call_id` |
| `AgentEvent` | Agent loop 的统一流式事件 | 流内序号或 `event_id` |
| `ProviderChannel` | 模型渠道配置，适配不同 wire format | `channel_id` |
| `ContextBundle` | Runtime 交给 Infra 的上下文包 | `context_id` 或 run 内临时 ID |
| `PayloadStore` | ToolCall input / output 完整副本存储，用于 replay | `payload_id` |

### 不变量

- 每次 provider 调用必须使用 canonical request，不把业务 DTO 直接塞给 provider adapter。
- 所有 tool 调用必须先通过 `ToolRegistry` 校验。
- 所有 tool 调用都必须记录 `ToolCall`。
- 未注册 tool 必须拒绝（parser 检测到未知 tool name 时返回 `<tool_error>` 给模型，不进入 dispatch）。
- Tool 超时、provider 错误、context-too-long 必须转换为统一 `AgentEvent.error` 或 tool error。
- Infra 不能绕过 Runtime 的 run policy 调用 tool。
- 交易写 tool 结果、账户确认结果不属于易腐内容，context 压缩时不能丢失审计摘要。
- 易腐 tool 结果在上下文中可压缩成 stub，但 `agent_messages` / `agent_tool_calls` 持久化记录不动；stub 必须含 `ref=<payload_id>` 让 replay 能从 `PayloadStore` 拉回原文。
- Provider 请求中不传 `tools` 数组、不解析 `tool_use` / `function_call` block；tool 调用走 §2 定义的 XML 文本协议。

### Tool 调用文本协议

LLM 在 chat 文本中用 XML 标签发起 tool 调用 / 接收结果。**协议固定，不允许扩展或换格式**：

```text
LLM 输出：
  ……一些自然语言推理……
  <use_tool name="fetch_quote">{"tsCode": "600519.SH"}</use_tool>

我们解析后回传（作为下一轮 user message 的 text block）：
  <tool_result name="fetch_quote" call_id="tc_abc123">
  {"price": "1820.5", "freshness": {"status": "fresh", "ageMs": 1200}, ...}
  </tool_result>

失败时回传：
  <tool_error name="fetch_quote" call_id="tc_abc123" code="quote_missing">
  {"message": "no snapshot for 600519.SH"}
  </tool_error>
```

规则：

- 标签名一律小写：`use_tool` / `tool_result` / `tool_error` / `tool_result_stub`。
- `name` 属性必填，值是 `ToolRegistry` 中注册的 tool name。
- `<use_tool>` 内容必须是合法 JSON（即 `ToolSpec.inputSchema` 校验的 input）。
- `<tool_result>` 内容是 JSON；`<tool_error>` 内容是 JSON 且必须带 `code` 属性（取 `ErrorCode` 封闭集合）。
- `call_id` 由 Infra 在 parser 检测到 `<use_tool>` 闭合时生成（`tc_<uuid>`），写入回传标签，供模型在后续推理中显式引用某次结果。
- LLM 输出单 turn 内可以有多个 `<use_tool>`；Infra 按出现顺序串行 dispatch（不并行），逐个回传 `<tool_result>`（支持批量工具调用）。
- **工具调用是本轮文本的逻辑终点（best practice，对齐 native tool-calling 语义）**：
  - 首个 `<use_tool>` **之前**的文本（preamble / 推理）实时 emit 为 `text_delta`。
  - 首个 `<use_tool>` **之后**的文本是模型在「无工具结果」下的推测续写（hallucination）——**不 emit、不作权威输出**（native 协议里模型在工具调用处即 `stop_reason=tool_use` 结束本轮；文本协议下模型可能继续吐字，按此规则丢弃）。
  - 同一 turn 内多个 `<use_tool>` 仍**全部按序收集 + dispatch**；tool 结果在**下一轮** user message 回灌。
  - 因此事件顺序天然是 `text_delta…（preamble）→ tool_start/tool_end…`，无需文本与 tool 交错。
- 模型在自然语言中提到"我想调用 fetch_quote"但**没**输出闭合标签时，**不**触发 dispatch；这是 chat 文本，不是调用。
- 标签嵌套不合法（例如 `<use_tool>` 内出现 `<use_tool>`）→ `<tool_error>` `parse_error`。
- 未闭合标签（流到 turn 结束仍未见 `</use_tool>`）→ 当 turn 文本处理；不 dispatch。

调用失败的 `code` 归类（全部包成 `<tool_error name="..." call_id="..." code="...">`，**不终止 loop**，回传给模型让它自行纠偏 / 重试）：

| 失败点 | `code` | 说明 |
|---|---|---|
| `name` 未在本次 `ToolRegistry` 注册 | `invalid_input` | parser 检测到未知 tool name，不进入 dispatch（§1 边界）；模型在 system prompt 中本就看不到未授权 tool |
| `<use_tool>` 内不是合法 JSON | `parse_error` | parser 解析失败（含 JSON 语法错、标签嵌套错） |
| JSON 合法但不过 `ToolSpec.inputSchema` | `invalid_input` | `validate_tool_input` 拒绝，不调用 handler |
| handler 执行超过 `ToolSpec.timeoutMs` | `tool_timeout` | Infra 中断等待、回传超时 error；handler 副作用是否已发生由 handler 自身幂等保证（Infra 不感知） |
| handler 内部返回业务错误 | handler 给出的 `ErrorCode` | 例如领域 tool 的 `quote_stale` / `insufficient_cash`；属业务结果，不是协议错 |

- `call_id` 回引语义：模型可在后续 turn 的自然语言里引用某次 `call_id`（例如「按 `tc_abc123` 的行情…」）作为推理锚；Infra 不强制模型引用，也不解析这种引用——它只保证每次 `<tool_result>` / `<tool_error>` / `<tool_result_stub>` 都带稳定 `call_id`，与 `ToolCall.toolCallId` / `agent_tool_calls` 行一一对应。
- `<tool_error>` 的 `code` 必须取 `ErrorCode` 封闭集合（[shared-types.md](shared-types.md) §5）；协议层失败用 `invalid_input` / `parse_error` / `tool_timeout`，业务层失败由 handler 返回对应领域 code。Infra 不发明新 code。

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
  conversationId?: string;   // 多轮会话标识（Runtime 提供）；跨 run 续接靠它分组
  seq?: number;              // 会话内单调序号；持久化排序用
  kind?: "chat" | "summary"; // 默认 chat；summary = 滚动压缩检查点（durable：不被 MicroClear/Drop；但下一次 Summarize 会把它折叠进新摘要，见 §4）
  role: AgentMessageRole;
  blocks: AgentMessageBlock[];
  createdAt: OccurredAt;
};
```

规则：

- `AgentMessage` 是 provider 上下文和聊天历史，不是投资判断。
- **会话归属由 `conversationId` 决定**（Runtime 提供，Infra 不定义"会话"业务语义）；同一 conversationId 的消息按 `seq` 排序构成多轮历史。`runId` 仍标记是哪一次 run 产生的。
- **持久化是全量审计**：loop 产出的每条 `AgentMessage` 落 `agent_messages`，**上下文压缩不修改已持久化的消息**（§4）；续接时按需 load 压缩视图（summary 检查点 + 最近若干轮），不是全量。
- `kind = "summary"` 的消息是 §4 Summarize 产出的压缩检查点：在上下文投影里替代被压缩的旧消息，标记 durable（不被 MicroClear / Drop 触碰）。**例外**：下一次 Summarize **必须**把现存 summary 折叠进新的滚动摘要（§4 "滚动累积摘要"）—— durable 在这里指"不被丢弃/替 stub"，不指"不被合并"。
- Tool 调用 / 结果以 XML 标签嵌在 `text` block 中，**不**作为独立 block type；这是审计可读 + provider 通用的关键。
- Tool 调用 audit 真源是 `ToolCall`；chat 历史中的 `<use_tool>` / `<tool_result>` 是给 LLM / 用户看的副本。
- `dataRef` 是图片在 PayloadStore / 本地文件系统的 URI（例如 `payload://pl_abc123` 或 `file:///path/to.png`）；**不是 base64 数据**。Provider adapter 在 build wire 时负责 dereference → 读字节 → base64 编码 → 塞 wire format。Dereference 失败必须返回 `ParseError` 而非静默丢弃。
- thinking 是否持久化取决于 provider 支持和配置；跨 provider 不保证恢复。Anthropic extended thinking 模型要求保留 `signature`；adapter 在写 `thinking` block 时必须保存 provider-specific metadata（如 Anthropic 的 signature / redacted）到 `metadata` 字段。
- role 和 block 的允许组合：

| role | 允许 block.type |
|---|---|
| `system` | `text` |
| `user` | `text`、`image` |
| `assistant` | `text`、`thinking` |

`tool` role 不存在；tool_result 以 `user` role + text block（含 `<tool_result>` XML）形式回写。

### `ToolSpec` / `ToolRegistry`

```ts
type SideEffect = "none" | "non_trading_write" | "trading_write";

type ToolSpec = {
  name: string;             // 例如 "fetch_quote"、"news_search"、"operate_account"
  description: string;      // markdown，一段话说明 tool 用途和典型场景（注入 system prompt）
  inputSchema: JsonSchema;  // tool 调用 input 的 JSON schema（dispatch 前校验）
  examples: string[];       // 至少 1 个完整 `<use_tool ...>{...}</use_tool>` 示例字符串
  sideEffect: SideEffect;
  timeoutMs: number;        // dispatch 超时
};

type ToolRegistrySnapshot = {
  tools: ToolSpec[];
  registeredAt: OccurredAt;
};
```

规则：

- Infra 只定义注册协议，不规定产品里必须有哪些 tool。
- 具体产品在 Runtime spec 中规定 canonical tool name union；本项目使用 Agent Runtime 的 `AgentToolName`。
- Runtime 决定每类 run 的 enabled tools，把对应 `ToolSpec` 注册进本次 loop。
- `sideEffect = "trading_write"` 的 tool 必须由 Runtime 显式允许，Infra 默认不得注册到非交易 run。
- 同名 tool 只能注册一次；重复注册必须 fail closed。
- Tool input 必须按 `inputSchema` 校验；校验失败包装为 `<tool_error>` 回传，不调用 handler。
- Tool output 必须转换成可摘要的 `JsonSummary`，供 stream / 审计 / chat 历史复用。
- Tool 描述和示例应当能被产品负责人手写为 markdown（例如 `skills/<name>/SKILL.md` 或编译期 `include_str!`），不要塞业务逻辑代码到描述里。

### System Prompt Tool 清单

每次 Agent loop 启动时，Infra 用 `SystemPromptBuilder` 把 enabled `ToolSpec` 集合编译成一段 system prompt 前缀，自动 prepend 到 `ContextBundle.systemParts`：

```text
你可以使用以下 tool。要调用某个 tool，输出 XML 标签
`<use_tool name="...">{...}</use_tool>`，内容是符合该 tool input schema 的 JSON。
每次调用后会以 `<tool_result name="..." call_id="...">` 形式回复给你。

## fetch_quote
获取单只标的实时行情快照。
Input: {"tsCode": "string，6位+.SH/.SZ/.BJ"}
Example: <use_tool name="fetch_quote">{"tsCode": "600519.SH"}</use_tool>

## news_search
...
```

规则：

- 注入顺序：固定 protocol 说明 → tool 列表（按 name 字典序，保证 prompt cache hit 一致）→ **skill 索引段**（可选）。
- 每个 tool 段：`## <name>` + description + `Input:` schema 摘要 + 至少 1 个 example。
- **skill 索引（渐进披露）**：tool 清单之后可追加「## 可用 Skill（playbook）」索引段——每条 `- <name>: <description>`（按 name 字典序），只放索引、不放 skill 正文；正文由模型按需经 `load_skill` 拉取。索引为空时省略整段。索引来源由 Runtime 从 skill 存盘目录扫描后传入（Builder 仍是纯计算，无 I/O）。详见 [agent-runtime-module.md](agent-runtime-module.md) §Skills。
- system prompt 中的 tool 清单 + skill 索引部分**不允许由 LLM 修改 / 看不见**；Runtime 注入后只读。

### `ToolCall`

```ts
type ToolCall = {
  toolCallId: string;          // tc_<uuid>，由 Infra 在 parser 检测到 <use_tool> 闭合时生成
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

- `name` 必须是本次 `ToolRegistry` 中已注册 tool name。
- Tool 被业务决策引用时，Runtime 可把 `ToolCall` 转成 `EvidenceRef`；Infra 不决定证据归属。
- 拒绝型业务结果不一定是 `isError = true`，例如 Account 拒单应由 tool output 表达业务原因（包含 `rejectionReason` 字段）。
- **PayloadStore 双层存储**（解决 LLM 视野 vs 长期审计的张力）：
  - 任何 tool 调用都会**同时**写入：
    1. chat 历史中的 `<tool_result>` text block（LLM 视野，可被 context compaction 替换为 stub）
    2. `agent_payloads` 表中的完整 input / output 副本（持久化，不受 compaction 影响）
  - 当 input / output JSON 序列化后**超过 8KB** 时，`ToolCall` 行的 `inputSummary` / `outputSummary` 只存截断摘要（前 1KB + `"[truncated, see ref]"`），完整数据走 `inputPayloadRef` / `outputPayloadRef`。
  - 当 input / output 小于阈值时，summary 字段 = 完整 payload 内容，ref 字段为空。
- 当 context compaction 把某条 `<tool_result>` 在 chat 历史中替换为 stub 时，stub 文本格式必须为 `<tool_result_stub name="..." call_id="..." ref="..." />`，模型可以读 stub 知道历史发生过这次调用，但 inline 数据已折叠；replay 时通过 ref 从 `agent_payloads` 拉回。
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
  | { type: "error"; runId: string; code: ErrorCode; message: string };
```

规则：

- `AgentEvent` 是 loop 执行事件，不是业务领域事件。
- Runtime 可以监听 `tool_end`、`done`、`error` 来更新 `AgentRun` 状态和业务审计记录。
- 后台 run 也必须产生事件流；前端可选择折叠展示。
- `text_delta` 只 emit 首个 `<use_tool>` **之前**的 preamble 文本（实时、与原始 LLM 输出顺序一致）；首个 tool 之后的文本按上文「工具调用是本轮文本逻辑终点」规则抑制，不 emit。
- Tool input / output 在 event 内是摘要；完整 payload 通过 `toolCallId` 查 `agent_tool_calls` + `agent_payloads`。
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
- 不再有 `supportsTools` / `supportsServerSideTools` 字段：所有 chat-completable provider 都通过 Tool 文本协议提供工具能力，没有 provider 差异。
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
  kind: "system" | "realtime" | "chat" | "memory" | "tool_result_stub";
  content: string | JsonSummary;
  freshness?: Freshness;
  tokenEstimate?: number;
  droppable: boolean;
};
```

规则：

- Runtime 负责提供业务上下文内容；Infra 负责排序、压缩和 provider format 转换。
- Infra 把 `SystemPromptBuilder` 编译出的 tool 清单作为 `kind = "system"` 的 ContextPart 自动 prepend 到 `systemParts`，Runtime 不需要手动塞。
- Infra 不维护聊天历史；Runtime 每次 run 必须把需要续接的 `AgentMessage[]` 转换成 `chatParts` 注入。
- 当前交易事实必须来自 Runtime 本次提供的 realtime context 或本次 tool 调用。
- 历史聊天和 summary 只能作为交互上下文，不能替代实时行情 / 账户读取。
- `droppable = false` 的内容只允许在 hard failure 前保留；如果超限仍无法发送，必须 fail closed（`stop_reason = context_limit`）。
- Context compaction 只影响本次或后续 provider request 的上下文投影，不修改已经持久化的 `AgentMessage`、`ToolCall`、`DecisionEpisode` 或 evidence snapshot 或 PayloadStore。

### `PayloadStore`

```ts
type PayloadStoreEntry = {
  payloadId: string;          // pl_<uuid>
  kind: "tool_input" | "tool_output" | "image";
  contentJson?: JsonValue;    // tool input/output 走这条
  contentBytes?: Uint8Array;  // image 走 bytes
  contentType?: string;       // image 的 mime（image/png / image/jpeg / ...）
  byteSize: number;
  createdAt: OccurredAt;
};
```

规则：

- 持久化在 `agent_payloads` 表（含 BLOB 列给 image，TEXT 列给 JSON）。
- 写入触发：
  - tool input/output JSON 序列化后超过 **8KB**
  - 图片 attachment（任何尺寸都进 PayloadStore，AgentMessage 只存 `payload://pl_xxx` 引用）
- 第一阶段**不实现 GC / retention policy**；payload 永久保留，用于 decision episode replay。后续需要清理时由独立产品策略处理，不在 Infra 隐式删除。
- `payloadId` 在 `ToolCall.inputPayloadRef` / `ToolCall.outputPayloadRef` / `AgentMessageBlock::Image.dataRef`（形如 `payload://pl_xxx`）之间共享；不允许跨 BC 的对象引用 PayloadStore（agent 自闭环）。
- Provider adapter 在 build wire 时遇到 `dataRef` 必须先从 PayloadStore 拉 bytes，再 base64 编码塞 wire；拉不到返回 `ParseError` 终止本次 dispatch。

---

## 3. Agent Loop

```text
Runtime builds AgentRunRequest
  -> Infra builds canonical chat request
       - SystemPromptBuilder 注入 tool 清单到 systemParts
       - AgentMessage[] (含 inline <tool_result> XML) 注入 chatParts
       - Image dataRef 由 provider adapter 从 PayloadStore 解引用
  -> provider.stream()           // 纯 chat stream，不传 tools
  -> ToolCallParser 增量扫描
       - 文本输出 → emit text_delta
       - 遇 <use_tool ...> 闭合 → emit tool_start → ToolRegistry dispatch
       - dispatch 完成 → emit tool_end → 缓存 <tool_result> 文本
  -> turn 结束（provider stop OR 闭合 </use_tool> 后回写）
       - 若本 turn 触发了至少一次 dispatch：
           构造新一轮 user message，body 是按出现顺序串联的
           <tool_result name="..." call_id="...">{...}</tool_result>
           （失败的是 <tool_error>）
           然后继续 loop
       - 否则 finalize：emit usage / done(stop_reason=completed/provider_stop)
  -> Reactive retry on context-too-long（见 §4）
  -> Hard limit fail closed → done(stop_reason=context_limit)
```

约束：

- 每次 run 必须有最大 turn 数（默认由 Runtime 注入），防止无限 tool 循环。
- Tool 有超时（`ToolSpec.timeoutMs`）；超时作为 `<tool_error code="tool_timeout">` 回传给模型，**不**直接终止 loop。
- 所有 tool 调用都进入统一事件流（`tool_start` / `tool_end`）和 `ToolCall` 审计。
- Provider 返回 context-too-long 时，按 §4 的 reactive retry 策略：压缩一次 → 重试一次 → 如果仍失败 → `stop_reason = context_limit`，emit `error` event (`code = "provider_context_too_long"`)。
- Infra 不在 loop 内创建 `DecisionEpisode` 或 `TradeIntent`；这些由 Runtime 根据模型输出和 tool 结果记录。
- Stream 解析必须**实时**（不等整个 turn 结束）：用户能从 UI 看到 LLM 思考 + tool 调用进度。
- 同一 turn 内多个 `<use_tool>` 按出现顺序**串行** dispatch；不并行（保证 LLM 看到的 tool_result 顺序与发出顺序一致）。

**ProviderStream 实现归属**：Agent Infra 定义 `ProviderStream` trait（接 canonical request、产 stream of chunks）**并自带 HTTP + SSE 实现** `HttpProvider`（reqwest 调三种 wire format、解 SSE event、转 stop_reason、聚合 usage、把 provider 错误分类成 `ProviderContextTooLong` / 瞬时 / 致命）。**容错机制**（瞬时退避重试 + 渠道 fallback，§4）也在 Infra：loop 在调用方传入的有序 `providers` 上执行。归 **Runtime 的只是策略**：选哪些 `ProviderChannel` 作主/备、鉴权、`RetryConfig` 取值——通过 `request.channel` / `fallback_channels` / `retry` 注入。Infra 另自带 `ScriptedProvider`，让 loop_executor 测试无网络运行。

---

## 4. 上下文管理

上下文由四类内容构成：

| 类型 | 内容 | 生命周期 |
|---|---|---|
| Identity / System | Agent 身份、运行纪律、tool 清单（由 SystemPromptBuilder 注入） | 长期，适合 cache |
| Realtime Packet | trigger、账户、行情、新闻、策略、近期 episode 摘要 | 每次 run 重建 |
| Chat Context | 用户最近对话、当前问题、历史 tool_result | 只服务交互 |
| Review / Memory | 用户偏好、复盘建议、策略说明 | 独立存储，按需注入 |

规则：

- **Infra 只提供压缩「机制」，不内置任何业务保留策略**。压缩只认两个**通用信号**，二者都由 Runtime 在装配 / 注册时设置：
  - `ContextPart.droppable`（Runtime 注入 realtime / memory 内容时自己标）：`true` = 可清理，`false` = pinned 不动。
  - `ToolSpec.sideEffect`：`trading_write` 的 tool 结果视为不可逆事实，**永不丢 / 不替 stub**；`none` / `non_trading_write` 视为可清理。
  - "投资场景什么该留、什么该丢"是 **Runtime 的策略**，通过设置上面两个旋钮 + 提供 `summarize_prompt` 来表达；Infra 不感知"行情/账户/交易"这些业务含义。（下文用行情/交易举例只是说明，不是 Infra 硬编码。）
- **主动压缩（loop 每轮发请求前）**：loop 在每次 `provider.next_turn` 之前先 `estimate_context_tokens`，按下方触发表主动 MicroClear / Summarize；不只在 provider 报错时被动 ReactiveRetry。
- `Chat Context` 只用于需要对话续接的 run；非交互后台 run 默认由 Runtime 提供 `Realtime Packet` 和 `Review / Memory`，不要求恢复完整聊天历史。
- 可清理内容（`droppable=true` / 非 trading_write 的 tool 结果）：替 stub / 丢弃，LLM 仍可通过 `<use_tool>` 重新拉取。例：行情 / K 线 / 新闻全文 / 旧账户读快照。
- 不可清理内容（`droppable=false` / `trading_write` 结果 / `kind=summary` 检查点）：永远 inline 保留。例：order_id / 成交价 / Account event ID。**Drop 一整轮时只清该轮的可清理部分，不可清理项保留**。

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
| time-based micro clear | 距上一条 assistant 消息超过约 60 分钟 | 清理旧易腐 tool 结果（替换为 `<tool_result_stub />`），保留最近若干条 |
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
  3. 用压缩后的 bundle **重发同一 turn 请求**（保持 turn id、保持 tool_call 上下文）
  4. **最多重试 1 次**；第二次仍失败 → finalize loop，`stop_reason = context_limit`，emit `error event (code = provider_context_too_long)` + `done event`
- `ReactiveRetry` 策略比 `Summarize` 更激进：直接 Drop 最老一轮 API round（包括其 chat history + tool_results），不等 summarize 模型返回，**保证下一次发送一定更短**。

**Provider 调用容错：瞬时退避重试 + 渠道 fallback**（与 context-too-long reactive retry 正交，按错误分类分别处理）：

- Infra 把 provider 错误分三类（HttpProvider / adapter 翻译成 canonical 错误）：
  - `ProviderContextTooLong`：上下文超长 → 走上面的 reactive retry（压缩 + 重发），**不**走退避 / fallback。
  - **瞬时（transient）**：HTTP 5xx / 429 / 连接失败 / 超时 / 流中断 / 上游 `upstream_error` → 可重试。
  - **致命（fatal）**：4xx（非 429）/ 鉴权 / 请求格式 / wire 映射错误 → **立即失败**，不重试不 fallback（重试无意义）。
- **退避重试（同渠道）**：遇瞬时错误对**同一渠道**重发 provider 调用，指数退避（`baseBackoffMs * 2^(n-1)`，封顶 `maxBackoffMs`），每渠道最多 `maxAttemptsPerChannel` 次。重试**只重发 provider 调用**——`input` 已在 turn 开始时由 Infra 落库一次，**不重复持久化**；turn 计数不前进。
- **渠道 fallback**：某渠道退避重试耗尽后，按 Runtime 提供的**有序备用渠道列表**切到下一个渠道、重复退避策略；切换后 **sticky**（后续 turn 从当前可用渠道起，不再每轮重试已死的主渠道）。全部渠道耗尽 → fail closed（emit error event，`stop_reason` 反映 provider 错误）。
- **边界**：选哪些渠道做 fallback、各自鉴权 = **Runtime 策略**（通过 `fallback_channels` + 各自 `ProviderChannel` 注入）；Infra 只做「按序退避重试 + 切换」的**机制**。⚠ 换渠道 = 换模型，对话中途能力 / 风格可能突变，由 Runtime 权衡。
- **可观测**：每次重试 / fallback 切换记 `tracing` warn（v1 不新增 `AgentEvent` 变体）。
- 配置 `RetryConfig { maxAttemptsPerChannel?, baseBackoffMs?, maxBackoffMs? }`（Runtime 提供，缺省内置 **3 次 / 500ms / 8000ms**）。

丢弃 / 压缩顺序：

```text
1. MicroClear 易腐 tool 结果（替换为 <tool_result_stub />）
2. Summarize 尾窗外历史对话
3. Drop 最旧 API round
4. Reactive retry（一次压缩 + 一次重发）
5. HardLimit fail closed → stop_reason = context_limit
```

易腐 tool 结果（可被 MicroClear / Drop / 替换为 stub）：

- 行情、K 线、分时、扫描结果
- 新闻、正文、搜索结果
- 账户读模型快照

不可清理（永远保留 inline，禁止替换 stub）：

- 交易写 tool 结果（operate_account 的 order_id / fill 等审计摘要）
- 策略卡写入结果（Runtime 的 strategy 操作）
- 账户确认结果（Account event 主键 + 状态）

原则：

- 所有可重新读取的数据 tool 结果都属于可清理对象（替换 stub 后 LLM 仍可通过 `<use_tool>` 重新拉取）。
- 交易写 tool、策略写入、账户确认结果不属于可清理对象。
- 易腐 tool 结果替换成 stub 时，**必须保留 `name` + `call_id` + `ref`**，让 replay 能通过 PayloadStore 拉回完整 payload。
- **Summarize 的 prompt 由 Runtime 提供**（`CompactionConfig.summarize_prompt`）。Infra 只负责执行：把"尾窗外待压消息"作为输入、`summarize_prompt` 作为 system，调 compact 模型生成摘要，产出一条 `kind=summary` 的 durable 消息替换被压消息。**Infra 不写 prompt 内容**（"摘要要覆盖关注标的/已建判断/未决问题/风险纪律/用户偏好"等是 Runtime 在 prompt 里规定的，不是 Infra 硬编码）。未提供 `summarize_prompt` → Summarize 档降级为 Drop（仍遵守"不可清理项保留"）。
- **滚动累积摘要（长期对话不丢史）**：第 2 次及以后的 Summarize **必须把已有的 `kind=summary` 检查点一并纳入输入**，产出一份"旧摘要 + 新对话"的新滚动摘要并**替换旧摘要** —— 上下文投影里全程只保留一个累积摘要。这样 `load_conversation_view`（只取最后一个 summary + 其后）就是完整无损的；否则多轮压缩后续接会丢早期历史。
  - **"只保留一个摘要"是视图层不变量，不是存储层**：`agent_messages` 是全量审计真源（§2），每个压缩周期都会**新增**一条 `kind=summary` 行并保留所有历史摘要行；"只剩一个"指的是 `load_conversation_view` 投影 / 喂给 provider 的上下文里只出现最新那一条。审计 / 复盘需要看历次摘要演化时走 `load_conversation`（全量）。
  - **durable 的两层含义要分清**：`kind=summary` 检查点 durable，指它不被 MicroClear / Drop（不丢、不替 stub），但它**要被折叠进**下一份滚动摘要；而 `trading_write` / `droppable=false` 的 **tool 结果** durable，指它既不丢也**不纳入摘要**、始终原样 inline 保留（order_id / 成交价等不可逆事实不能被摘要改写）。实现上：Summarize 选取待压前缀时，对 summary 检查点要**纳入折叠**，仅对"非 summary 的 durable 结果"跳过。
  - Infra 把待压材料喂给 compact 模型时应明确框定为"待压缩历史材料、只输出摘要、不要回复其中的问题"，并把已有摘要单独标注"必须完整保留并合并"，避免弱模型把转录当成对话来续答、或漏掉旧摘要里的事实。
  - 若待压前缀里只有旧摘要、无新内容 → 跳过本次摘要（避免无意义地重摘）。Runtime 的 `summarize_prompt` 应能处理"输入含上一版摘要时产出完整合并摘要"的情形。
- Compact 模型可独立配置（`CompactionConfig.compact_channel`）；未配置时复用当前 run 的 channel。
- Summary 是续接上下文，不是事实真源；当前行情和账户状态仍必须重新读取（fail-closed 原则：因为旧数据可丢、agent 行动前必须重读最新，所以可清理项才安全可丢）。
- 每次 compact 必须 emit `AgentEvent.compacted`，含 `tier` + `droppedMessages` + `estimatedTokensSaved`。

### 多轮会话持久化与续接

- **消息持久化完全归 Infra**：调用方（Runtime）只把这一轮的新消息放进 `request.input`，**绝不自己 upsert / 分配 seq / 打 conversationId**。`run_agent_turn` 在 `conversationId` + `repo` 都有时，自己 ① 先把 `input` 落库（分配 `seq` + 打 `conversationId`）② loop 产出的每条 `AgentMessage`（assistant + `<tool_result>` user + `kind=summary` 检查点）也由 Infra 落库。`agent_messages` 是**全量审计真源，压缩不修改它**。持久化是 Infra 的本职，不是调用方的责任。
- **续接（开新 run 接上历史）= Infra 自动完成**：只要带 `conversationId` 且 `repo` 存在，Infra 在持久化 `input` 后自动按 `conversationId` load **压缩视图**（最近一个 `kind=summary` 检查点 + 其后的最近若干轮，含刚落库的 `input`）作为本轮上下文，而非全量历史（否则上下文随会话无限增长）。无 `conversationId` 或无 `repo` → 只跑 `input`（新会话 / 无状态）。**调用方无需、也不应手动 load 历史**。
- `conversationId` 的"会话"业务语义（一次咨询 = 一个会话？跨天延续？）由 Runtime 定义；Infra 只按它分组 + 排序 + load/save。
- Infra 只暴露**唯一编排入口** `run_agent_turn(request{ conversationId, input, … }, …, repo)`，行为由 `conversationId` + `repo` 决定：`repo=Some` + `conversationId` → persist(input) → load 压缩视图 → 跑 loop → persist(产出)；`conversationId` 新建 / None → 只跑 `input`（新会话）；`repo=None` → 纯无状态（不 persist 不 load）。**不再有 `seed_messages` 这种"内容 + 续接"双控字段，也不需要调用方代劳任何持久化。**

---

## 5. 对外接口

### Infra Loop API

```rust
// 唯一编排入口。调用方只给「这轮新消息 input」+「会话 id」，持久化与续接由 Infra 全包：
//   repo=Some & conversationId 已存在 → Infra persist(input) → load_conversation_view 续接 → 跑 → persist(产出)
//   repo=Some & conversationId 新/None → 只跑 input（新会话），产出仍落库（若有 conversationId）
//   repo=None                          → 纯无状态：不 persist、不 load，只跑 input
// 调用方永远不自己 upsert / 分配 seq / load 历史 —— 那是 Infra 的职责。
// providers = 有序 provider 列表：providers[0] = primary（对应 request.channel），
// providers[1..] 对应 request.fallback_channels（顺序一致）。调用方按 [channel]++fallback_channels
// 构建（生产用 HttpProvider，测试注入 ScriptedProvider）。loop 按 §4 容错策略在其上退避重试 + fallback。
run_agent_turn(request, registry, context, providers, event_tx, repo) -> RunSummary;
estimate_context_tokens(messages, context, channel) -> TokenEstimate;
compact_context(messages, context, policy, ...) -> Compacted; // 纯计算，作用于会话 messages + ContextBundle

// AgentRunRequest 关键字段
type AgentRunRequest = {
  runId: string;
  trigger: string;
  channel: ProviderChannel;           // 主渠道
  maxTurns: number;
  input: AgentMessage[];              // 这一轮的新消息（通常一条 user）；Infra 负责落库，调用方不自己 upsert
  conversationId?: string;            // 多轮会话标识（Runtime 提供）；有它 + repo 即自动续接 + 落库
  compaction?: CompactionConfig;      // 上下文压缩配置（Runtime 提供；缺省用 channel 推导的阈值）
  fallbackChannels?: ProviderChannel[]; // 有序备用渠道（§4 容错）；主渠道瞬时重试耗尽后按序切换。缺省空
  retry?: RetryConfig;                // 瞬时退避重试策略（§4）；缺省内置 3 次 / 500ms / 8000ms
};

type RetryConfig = {
  maxAttemptsPerChannel?: number;     // 每渠道瞬时错误最多尝试次数（含首次），缺省 3
  baseBackoffMs?: number;             // 指数退避基数，缺省 500（500→1000→2000…）
  maxBackoffMs?: number;              // 退避封顶，缺省 8000
};

type CompactionConfig = {
  softLimitTokens?: number;           // 缺省由 channel.contextWindowTokens 推导
  summarizeThresholdTokens?: number;
  hardLimitTokens?: number;
  keepRecentTurns?: number;           // 最近 N 轮永不摘要（默认若干）
  summarizePrompt?: string;           // Runtime 提供的摘要指令；缺省 → Summarize 降级为 Drop
  compactChannel?: ProviderChannel;   // 摘要模型；缺省复用 run 的 channel
};
```

规则：

- `request` 必须包含 `runId`、provider channel、model、max turns。**不**含 server-side tool 字段。
- `registry` 是本次 run 的 `ToolRegistry` 实例，含 Runtime 本次允许的 tool 集合。
- `context` 由 Runtime 构造；Infra 不主动读取 Quotes / News / Account。Tool 清单在 build 时由 `SystemPromptBuilder` prepend 到 `systemParts`，Runtime 不需要手动塞。
- `event_tx` 接收统一 `AgentEvent`，供 Runtime 和 UI 订阅。
- `run_agent_turn` 持久化与续接全包（§4「多轮会话持久化与续接」）：`repo` + `conversationId` 都有时，先落 `input`、自动 load 压缩视图续接、产出按 `conversationId` 落库；调用方不碰持久化。每轮发请求前主动 `estimate_context_tokens` → 按 `compaction` 阈值主动压缩（§4）。`repo = None` → 纯无状态。
- `compact_context` 是纯计算 API（作用于会话 messages + ContextBundle，按 `droppable` / `sideEffect` 通用信号），**不**做 retry、**不**调模型；Summarize 的模型调用 + ReactiveRetry 由 `run_agent_turn` orchestrate（见 §4）。

### Conversation / Messages Repo API

```rust
upsert_message(msg: &AgentMessage) -> Result<()>;
load_conversation(conversation_id: &str) -> Result<Vec<AgentMessage>>;       // 全量（审计）
load_conversation_view(conversation_id: &str) -> Result<Vec<AgentMessage>>;  // 压缩视图：最近 summary 检查点 + 其后最近若干轮
```

- `agent_messages` 全量审计真源；`load_conversation_view` 给续接用，不返回全量。

### Tool Registry API

```rust
register_tool(spec: ToolSpec, handler: Arc<dyn ToolHandler>) -> Result<()>;
validate_tool_input(tool_name: &str, input: &JsonValue) -> Result<()>;
dispatch_tool_call(run_id: &str, tool_call_id: ToolCallId, tool_name: &str, input: JsonValue)
    -> ToolCallResult;
list_tools() -> Vec<ToolSpec>;         // 用于 SystemPromptBuilder 拉清单
has_tool(name: &str) -> bool;

trait ToolHandler: Send + Sync + 'static {
    fn invoke(&self, inv: ToolInvocation) -> ToolHandlerFuture;
    // ToolInvocation 含 run_id / tool_call_id / input；handler 返回 ToolHandlerOutput
}
```

规则：

- handler 位于 adapter 或 Runtime wiring，不放在 provider adapter 中。
- `dispatch_tool_call` 必须记录 `ToolCall` 开始和结束；大 payload 自动走 PayloadStore（§2 规则）。
- `tool_call_id` 由 Infra 在 `ToolCallParser` 检测到 `<use_tool>` 闭合时生成；caller 不传 id。
- 任何 tool 执行失败都必须返回结构化错误（`<tool_error code="..." />`），不 panic 终止 loop。
- `validate_tool_input` 校验失败包装为 `<tool_error code="invalid_input" />` 回传给模型。

### SystemPromptBuilder API

```rust
build_system_prompt(tools: &[ToolSpec], base_prompt: &str) -> String;
```

把 enabled tools 按字典序编译成 markdown 清单，prepend protocol 说明 + `base_prompt`。Builder 是纯计算，无 I/O。

### ToolCallParser API

```rust
struct ToolCallParser { /* state machine */ }
impl ToolCallParser {
    fn new() -> Self;
    fn feed(&mut self, chunk: &str) -> Vec<ParserEvent>;
    fn finalize(&mut self) -> Vec<ParserEvent>;  // turn 结束时调用
}

enum ParserEvent {
    TextDelta(String),                                  // emit 给 stream
    UseTool { name: String, input: JsonValue },         // dispatch
    ParseError { reason: String, partial: String },     // 标签格式坏 → tool_error
}
```

规则：

- Parser 实时增量扫描 provider stream；不缓冲整 turn。
- 检测到 `<use_tool name="X">` 后**只**缓冲到 `</use_tool>` 闭合，期间不 emit text_delta（避免泄漏 raw XML 给 UI）。
- 闭合后解析 JSON：成功 → emit `UseTool`；失败 → emit `ParseError`。
- 流到 turn 结束仍未闭合的 `<use_tool>` 当 text 处理（不 dispatch）。

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
- 主渠道支持 streaming；前端能看到 `run_start`、`text_delta`、`thinking_delta`（如有）、`tool_start` / `tool_end`、`usage`、`done` / `error`。
- 未注册 tool 被拒绝（parser 检测后返回 `<tool_error code="invalid_input">`），不会因 LLM 任意输出字符串触发 dispatch。
- Runtime 限制本次 run 不允许 `operate_account` tool 时，Infra 不会把它编译进 SystemPromptBuilder 的 tool 清单，模型在 system prompt 中看不到该 tool 存在。
- Provider context-too-long 后，Infra 能调一次 `compact_context(policy=ReactiveRetry)` 并重发同一 turn 请求；第二次仍失败则 `stop_reason = context_limit`，emit `error event (code=provider_context_too_long)`。
- 易腐 tool 结果被压缩成 `<tool_result_stub name="X" call_id="tc_..." ref="pl_..." />` 后，下一轮 prompt 仍是合法 chat 文本（无悬空 XML），且 ref 能在 `agent_payloads` 中查到原 payload。
- Tool 调用 input / output ≤ 8KB 时，`ToolCall.input/outputSummary` = 完整 payload；> 8KB 时 summary 是截断摘要，full payload 在 PayloadStore 中通过 ref 可拉回。
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

---

## 8. 验证记录（2026-06-01，实网 + hermetic）

> 点-时验证快照，非契约。后续行为变更若推翻这里的结论，更新本节。验证方式：**LLM-as-Judge**
> （被测 agent 真实跑 `run_agent_turn` → 裁判模型按 rubric 打 `{pass,score,reason}`），裁判恒用
> 一个独立可靠渠道（opus-4.5），与被测渠道隔离。凭证只走环境变量，不入库。

**hermetic**：`cargo test` 631 passed / 0 failed（含 5 个 retry/fallback 单测、边界-seq 有界性测、http 错误分类测、续接持久化单测）。

**三种 wire format 实网全过**（持久化 / 压缩 Summarize 跑在该 wire / 上下文管理，judge 均 1.00）：
- `messages` → Anthropic relay / claude-opus-4-5
- `responses` → OpenAI relay / gpt-5
- `chat_completions` → DeepSeek / deepseek-v4-flash

覆盖场景：答题相关性、tool 忠实、多轮记忆、跨轮约束累积、durable 逐字保真 vs droppable 可压、MicroClear 后答案正确、摘要忠实 + 无幻觉、keep_recent 逐字、Drop 降级保 durable、滚动累积摘要折叠。

**100+ 轮长跑压测**（`judge_hundred_turn_longterm_memory`，105 轮，每轮触发 Summarize）——DeepSeek 与 gpt-5/responses **两个渠道各跑满**：
- `ok=105/105 fail=0`，`cycles≈100`，`full_audit≈310`，**`view_len` 全程恒为 6**（续接视图有界，不随轮数膨胀）。
- 第 1 轮埋的账户代号 + 第 50 轮埋的口令穿越 ~100 次滚动折叠**逐字不丢**，终判 1.00。

**容错**（瞬时退避重试 + 渠道 fallback，§4）：5 个 hermetic 单测覆盖「重试恢复 / 耗尽切备用 / 致命不重试 / context-too-long 不当瞬时 / 全渠道耗尽」；20 分钟 gpt-5 长跑期间 relay 有瞬时抖动（502 / 579 / 582 / 593 / 空响应 = 上游 `upstream_error`），被退避重试在底层吸收，长跑 `fail=0`。

**测试中发现并修复的真实 bug**：
1. `responses` 适配器多轮断链：assistant 轮误用 `input_text`（应 `output_text`），单轮侥幸过、任何多轮被上游 502 拒。
2. 续接视图无界：`apply_summary` 让滚动摘要继承「最旧」seq，致 `load_conversation_view` 随轮数线性膨胀；改取「边界 seq」后有界。

**复现环境变量**：agent 渠道 `TEST_ANT_*` / `TEST_OAI_*` / `TEST_DS_*`（设哪个 `pick_fast_agent_channel` 选哪个）；裁判 `JUDGE_BASE/KEY/MODEL/WIRE`。所有 judge 测试 `#[ignore]`，缺渠道自动跳过。
