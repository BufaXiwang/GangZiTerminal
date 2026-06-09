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
- `AgentRun`、`InvestmentStrategy`、`AnalysisResult`、`AgentTrade` 属于 Agent Runtime。

共享类型见 [shared-types.md](shared-types.md)。

---

## 术语：Tool vs Skill（已统一）

> **本模块的"可注册原语"在产品里的 canonical 名是 Tool。** 命名已统一——本文档全文用 `Tool*`（`ToolSpec`/`ToolCall`/`ToolRegistry`/`ToolCallParser`/`<use_tool>`/`<tool_result>`/`<tool_error>`），与 [agent-runtime-module.md](agent-runtime-module.md) 的 `Tool` / `ToolRegistry` 一致，指**同一个机制**。两层模型：
>
> - **Tool（原语）** = 注册进 registry 的可调用能力（name + input schema + handler），in-process、结构化、经文本协议调用。本文档（Infra 层）只定义此层的注册 / 调用 / 审计机制。
> - **Skill（playbook）** = 模型驱动的 `SKILL.md` 说明书，编排若干 tool 完成任务，**不是注册 handler**；子系统（SkillStore + 存盘 + fork 执行）归 Infra，playbook 内容由产品 / 人编写，**初始为空**。详见 §3.6（Skill 子系统）。
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
- 实现 `PayloadStore`：持久化 ToolCall 的完整 input / output payload，用于决策回放 / 复盘（Runtime 按 `run_id` 回查）。
- 执行基础 Agent loop，限制最大 turn 数，避免无限 tool 循环。

Agent Infra 不负责：

- 监听 News / Account / Quotes 事件并决定何时启动 Agent run。
- 决定某类 run 允许使用哪些 tool。
- 构造投资决策 packet。
- 判断新闻重要性、是否交易、是否调仓。
- 记录 `AnalysisResult`、`AgentTrade` 或复盘报告。
- 管理 `InvestmentStrategy` 生命周期或策略注入规则。
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
  isSpawn?: boolean;        // fork 类工具标记（run_subagent/run_skill 等）；子 agent registry 构造时按此剔除（§3.5 无嵌套），缺省 false
};

type ToolRegistrySnapshot = {
  tools: ToolSpec[];
  registeredAt: OccurredAt;
};
```

规则：

- Infra 只定义注册协议，不规定产品里必须有哪些 tool。
- Infra 不维护本产品的全局 tool name 目录；tool name 在 `ToolRegistry` 中以 opaque `ToolName` 注册。Infra 默认 tool 名见 §3.6 `InfraToolName`，Runtime 领域 tool 名见 [agent-runtime-module.md §4](agent-runtime-module.md#4-工具)。
- Runtime 决定每类 run 的 enabled tools，把对应 `ToolSpec` 注册进本次 loop。
- `sideEffect = "trading_write"` 的 tool 必须由 Runtime 显式允许，Infra 默认不得注册到非交易 run。
- 同名 tool 只能注册一次；重复注册必须 fail closed。
- **inputSchema 校验是强制且自动的**：`ToolRegistry` 在注册时就用 `inputSchema` 编译出 JSON Schema 校验器（不依赖调用方手动传校验器），dispatch 前对 input 校验。校验失败：① 包装为 `<tool_error code="invalid_input">` 回传给模型让它自行纠偏（**不调用 handler、不终止 loop**）；② 同时 emit `tool_end{ isError: true }` 事件，让前端能展示「这次工具调用 input 不合 schema」。`inputSchema` 是**强制契约**，不是文档性提示。
- Tool output 必须转换成可摘要的 `JsonSummary`，供 stream / 审计 / chat 历史复用。
- Tool 描述和示例应当能被产品负责人手写为 markdown（例如 `skills/<name>/SKILL.md` 或编译期 `include_str!`），不要塞业务逻辑代码到描述里。

### System Prompt Tool 清单

每次 Agent loop 启动时，Infra 用 `SystemPromptBuilder` 把 enabled `ToolSpec` 集合编译成一段 system prompt 前缀，自动 prepend 到 `ContextBundle.systemParts`：

```text
你可以使用以下 tool。要调用某个 tool，输出 XML 标签
`<use_tool name="...">{...}</use_tool>`，内容是符合该 tool input schema 的 JSON。
每次调用后会以 `<tool_result name="..." call_id="...">` 形式回复给你。

## read_file
读取文件内容（任意路径，只读）。
Input: {"path": "string", "offset?": number, "limit?": number}
Example: <use_tool name="read_file">{"path": "/tmp/notes.md"}</use_tool>

## fetch_quotes
（领域 tool 示例——由 Runtime 注入，非 Infra 自带）获取标的实时行情。
...
```

规则：

- 注入顺序：固定 protocol 说明 → tool 列表（按 name 字典序，保证 prompt cache hit 一致）→ **skill 索引段**（可选）。
- 每个 tool 段：`## <name>` + description + `Input:` schema 摘要 + 至少 1 个 example。
- **skill 索引（渐进披露）**：tool 清单之后可追加「## 可用 Skill」索引段——每条 `- <name>: <description>`（按 name 字典序），**只放索引、不放 skill 正文**；模型经 `run_skill` 触发（fork 子 agent 执行该 skill，正文只进子 agent、不进父 prompt）。索引为空时省略整段。索引来源由 Runtime 从 skill 存盘目录扫描后传入（Builder 仍是纯计算，无 I/O）。详见 §3.5（子 Agent / Fork）、§3.6（Skill 子系统）。
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
- Tool 被业务决策引用时，证据即该 run 的 `ToolCall` 审计（Runtime 按 `run_id` 回查重建"当时看到了什么"）；Infra 只负责记录 `ToolCall`，不决定证据归属。
- 拒绝型业务结果不一定是 `isError = true`，例如 Account 拒单应由 tool output 表达业务原因（包含 `rejectionReason` 字段）。
- **PayloadStore 双层存储**（解决 LLM 视野 vs 长期审计的张力）：
  - 任何 tool 调用都会**同时**写入：
    1. chat 历史中的 `<tool_result>` text block（LLM 视野，可被 context compaction 替换为 stub）
    2. `agent_payloads` 表中的完整 input / output 副本（持久化，不受 compaction 影响）
  - 当 input / output JSON 序列化后**超过 8KB** 时，`ToolCall` 行的 `inputSummary` / `outputSummary` 只存截断摘要（前 1KB + `"[truncated, see ref]"`），完整数据走 `inputPayloadRef` / `outputPayloadRef`。
  - 当 input / output 小于阈值时，summary 字段 = 完整 payload 内容，ref 字段为空。
- 当 context compaction 把某条 `<tool_result>` 在 chat 历史中替换为 stub 时，stub 文本格式必须为 `<tool_result_stub name="..." call_id="..." ref="..." />`，模型可以读 stub 知道历史发生过这次调用，但 inline 数据已折叠；replay 时通过 ref 从 `agent_payloads` 拉回。
- LLM 视野优先 inline 全文；PayloadStore 是审计 / replay 用的并行存储，**不**给 LLM 当下读，而是给决策回放 / 用户复盘用。

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
  | "token_budget_exceeded"      // 累计 token（含 fork 子 run 回灌）超过 request.tokenBudget
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
- **`usage` 是「每轮增量」语义**：每个 turn 的 provider 响应聚合出一条 `usage`（只含该 turn 的 token），loop **不**再额外 emit「run 累计」那一条。订阅方（前端 / Runtime）按需自行累加；run 总量由 Runtime 在收尾时落到 `AgentRun` 记录。即一次 run 收到 N 个 turn 就有 N 条 `usage`，每条互不重叠。
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
  systemParts: ContextPart[];   // 唯一一条 lane：身份 + 工具清单 + Runtime 注入的 realtime packet / memory
};

type ContextPart = {
  kind: "system" | "realtime" | "memory" | "tool_result_stub";  // 语义标签；全部承载在 systemParts 内
  content: string | JsonSummary;
  freshness?: Freshness;
  tokenEstimate?: number;
  droppable: boolean;
};
```

> **设计：单 lane（`systemParts`）+ 独立 `messages` 历史**。早期设计曾有 `realtimeParts` / `chatParts` / `memoryParts` 四条 lane，但多轮历史已改由独立的 `AgentMessage[]`（`request.input` + repo 自动续接，见 §4「多轮会话持久化与续接」）承载，`chatParts` 成为僵尸；`realtimeParts` / `memoryParts` 因 provider 只渲染 `systemParts` 而从不进 wire。现统一为：**Runtime 把 realtime packet（每次 run 重建的行情 / 账户快照）和 memory 都作为 `ContextPart` 放进 `systemParts`（用 `kind` 标语义、用 `droppable` 标可清理性）；run 过程中拉的实时数据走 tool（`fetch_quotes` 等）→ 落进 `messages` 的 `<tool_result>` → 由 message-lane 压缩处理**。

规则：

- Runtime 负责提供业务上下文内容（放进 `systemParts`）；Infra 负责排序、压缩和 provider format 转换。
- Infra 把 `SystemPromptBuilder` 编译出的 tool 清单作为 `kind = "system"` 的 ContextPart 自动 prepend 到 `systemParts`，Runtime 不需要手动塞。
- Infra 不维护聊天历史；多轮续接由 Infra 经 `request.input` + `conversationId` + repo 自动完成（§4），Runtime **不**把历史塞进 ContextBundle。
- 当前交易事实必须来自 Runtime 本次注入的 realtime packet（`systemParts`）或本次 tool 调用。
- 历史聊天和 summary 只能作为交互上下文，不能替代实时行情 / 账户读取。
- `droppable = false` 的内容只允许在 hard failure 前保留；如果超限仍无法发送，必须 fail closed（`stop_reason = context_limit`）。
- Context compaction 只影响本次或后续 provider request 的上下文投影，不修改已经持久化的 `AgentMessage`、`ToolCall`、`AnalysisResult` / `AgentTrade` 或 PayloadStore。

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
- 第一阶段**不实现 GC / retention policy**；payload 永久保留，用于决策回放 / 复盘。后续需要清理时由独立产品策略处理，不在 Infra 隐式删除。
- `payloadId` 在 `ToolCall.inputPayloadRef` / `ToolCall.outputPayloadRef` / `AgentMessageBlock::Image.dataRef`（形如 `payload://pl_xxx`）之间共享；不允许跨 BC 的对象引用 PayloadStore（agent 自闭环）。
- Provider adapter 在 build wire 时遇到 `dataRef` 必须先从 PayloadStore 拉 bytes，再 base64 编码塞 wire；拉不到返回 `ParseError` 终止本次 dispatch。

---

## 3. Agent Loop

```text
Runtime builds AgentRunRequest
  -> Infra builds canonical chat request
       - SystemPromptBuilder 注入 tool 清单到 systemParts
       - AgentMessage[] (含 inline <tool_result> XML) 作为独立 messages 历史（repo 续接）
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
- **可取消**：`run_agent_turn` 接受一个取消信号（`CancellationToken`）。loop 在每个 turn 边界 + provider stream 进行中检查；一旦被取消 → 停止后续 turn、emit `done{ stop_reason: "cancelled" }`（已落库的消息 / ToolCall 不回滚，正在执行的 handler 由其自身幂等保证）。取消由触发入口（Tauri command / scheduler / Runtime）持有 token、经另一条命令触发（见前端架构「取消长任务」）。
- Tool 有超时（`ToolSpec.timeoutMs`）；超时作为 `<tool_error code="tool_timeout">` 回传给模型，**不**直接终止 loop。
- 所有 tool 调用都进入统一事件流（`tool_start` / `tool_end`）和 `ToolCall` 审计。
- Provider 返回 context-too-long 时，按 §4 的 reactive retry 策略：压缩一次 → 重试一次 → 如果仍失败 → `stop_reason = context_limit`，emit `error` event (`code = "provider_context_too_long"`)。
- Infra 不在 loop 内创建 `AnalysisResult` 或 `AgentTrade`；这些由 Runtime 根据模型输出和 tool 结果记录。
- Stream 解析必须**实时**（不等整个 turn 结束）：用户能从 UI 看到 LLM 思考 + tool 调用进度。
- 同一 turn 内多个 `<use_tool>` 按出现顺序**串行** dispatch；不并行（保证 LLM 看到的 tool_result 顺序与发出顺序一致）。

**ProviderStream 实现归属**：Agent Infra 定义 `ProviderStream` trait（接 canonical request、产 stream of chunks）**并自带 HTTP + SSE 实现** `HttpProvider`（reqwest 调三种 wire format、解 SSE event、转 stop_reason、聚合 usage、把 provider 错误分类成 `ProviderContextTooLong` / 瞬时 / 致命）。**容错机制**（瞬时退避重试 + 渠道 fallback，§4）也在 Infra：loop 在调用方传入的有序 `providers` 上执行。归 **Runtime 的只是策略**：选哪些 `ProviderChannel` 作主/备、鉴权、`RetryConfig` 取值——通过 `request.channel` / `fallback_channels` / `retry` 注入。Infra 另自带 `ScriptedProvider`，让 loop_executor 测试无网络运行。

---

## 3.5 子 Agent / Fork（execution 底座，**Infra 层**）

> **fork 子 agent 是 Infra 的执行能力，不是 Runtime。** 它就是「在隔离上下文里再跑一遍 loop、跑完只把结果带回父」——纯执行机制，与业务无关（参考 Claude Code 的 `runAgent`）。Runtime 只决定「用哪个 agent 定义 / 哪些 skill / 什么权限」，**怎么 fork、怎么隔离、怎么回灌结果都在 Infra**。

### 机制

- Infra 提供 **`run_forked_agent`**（内部 API）：给定 `{ prompt, system_prompt, tools, channel, parent_run_id }` → **起一个子 run**（复用同一套 `run_agent_turn` loop），跑完只返回**子 run 末轮文本**（= 子 run **最后一轮**、即模型不再发起工具调用、给出最终答案那一轮的 assistant 文本；对齐 Claude Code「只取子 agent 的最后一条消息」）。中间轮的自然语言铺垫（"我先读一下文件…"之类）与工具机制**都不回父**。
  - **末轮识别（实现）**：聚合子 run 事件流时，每遇到一个 `ToolStart`（= 当前轮发起了工具调用、不是末轮）就**清空文本累加器**；`TextDelta` 直接 append。子 loop 内每个 turn 的事件顺序固定为「该 turn 的全部 `TextDelta` → 该 turn 的 `ToolStart`/`ToolEnd`」，且只有**没有任何工具调用**的 turn 才是末轮（loop 据此 `break`）——故 loop 结束时累加器里恰好只剩末轮文本。
  - **提示子把结论放最后一条**：`run_forked_agent` 给子 run 的引导（seed user message）末尾**统一拼一句固定提示**，告诉子 agent「只有你的最后一条消息会被返回，请把完整结论 / 产出放进末轮，不要分散在中间轮」。`run_subagent`（自由 prompt）与 `run_skill`（SKILL.md 作 prompt）两条 fork 路径都带上。
  - **隔离上下文**：子 run 用**全新 `conversation_id`**（带 `parent_run_id` 关联），不与父共享消息历史；子的中间 tool 调用 / 试错**不进父上下文**。
  - **token 上下文**：子 run **继承父的上下文窗口 / compaction 配置**（`run_agent_turn` 按 channel 推导阈值）。**token 预算**：`run_agent_turn` 接受 `tokenBudget` 入参；子 run 的 token usage 经 `SubAgentTask.progress.tokens` 累计并**回灌父 run 预算**（父把子消耗计入自己 budget）；父或子任一累计超 budget → 停后续 turn、emit `done(stop_reason="token_budget_exceeded")`。
  - **继承**：默认 `channel` / `model` / `effort` / 工具集都**继承父**（本项目不做 per-子 model 覆盖）；可传 `tools` 子集**收紧**（如只读工具）。**但子工具集一律剔除全部 spawn-class 工具**——除内置 `run_subagent` / `run_skill` 外，未来 Runtime 若注入 fork 类领域工具同理；剔除**由 `ToolSpec.isSpawn` 标记驱动、不靠 name 白名单**，确保任何新增 fork 类工具都被覆盖。只有顶层 agent 能 spawn（见「不变量」）。`create_skill`（写文件、不递归）`isSpawn=false`，保留。
  - **审计**：子 run 全量消息照常落 `agent_messages`（自己的 `conversation_id` + `parent_run_id`），可单独 replay。
- 父对话侧：fork 表现为一次 **tool 调用**（`run_subagent` / `run_skill`，§5 工具），其 `<tool_result>` = 子 run 的结果文本。
- **前端可见性（不污染上下文）**：子 run 的中间过程**不进父 LLM 上下文**（父只在 fork 完成拿末轮结论），但**会作为 `AgentEvent::SubAgentActivity` 转发给前端**——让用户在子 agent（阻塞）跑时实时看到它在调什么工具 / 输出什么（对齐 Claude Code sidechain 视图）。机制：发起 run 的入口把本 run 的 `event_tx`（= 前端通道）注入 `ForkRuntime.parent_event_tx`；`run_forked_agent` 的事件聚合处把子事件打 `{parentRunId, agentId, kind}` 标签转发。`kind ∈ started/tool_start/tool_end/text/done`。这是**纯前端展示通道**，与「只回末轮文本」的上下文卫生互不影响。

### 两种模式（对齐 CC）

| 模式 | 上下文 | system prompt / 工具 / model | 用途 |
|---|---|---|---|
| **命名子 agent / skill**（默认）| **全新隔离**上下文 | 给定的（skill = SKILL.md 作 prompt；工具默认继承、可收紧，**但一律剔除全部 spawn-class 工具（`run_subagent`/`run_skill` 等，按 `ToolSpec.isSpawn` 驱动）→ 子 agent 不能再 fork**）| 专门子任务 / 跑 skill，父只收结果 |
| **隐式 fork**（可选，后续）| **继承父完整上下文 + system prompt + 精确工具池** | 全继承（`inherit`）| "在当前上下文分叉继续干" |

### 子 Agent 任务管理（注册表 + 生命周期，对齐 CC `LocalAgentTask`）

主 Agent 通过 Infra 的**子 agent 任务注册表**管理所有 spawn 出来的子 run。每条任务：

```ts
type SubAgentTask = {
  agentId: string;                 // 子 run 标识
  parentRunId: string;             // 关联父
  description: string;             // 一句话任务描述
  status: 'queued' | 'running' | 'completed' | 'failed' | 'killed';
  progress: { tokens: number; toolUses: number; durationMs: number };  // 从子 run 消息累计
  abort: AbortHandle;              // 用于 stop
  notified: boolean;               // 防重复通知
};
```

**生命周期**：`spawn`（注册→running）→ `update_progress`（按子 run 的 turn 累计 token/tool_uses）→ 终态 `complete` / `fail` / `kill`（abort）。

**三种执行模式**：
| 模式 | 行为 | 父侧拿到 |
|---|---|---|
| **前台**（默认）| 阻塞——主 agent 等子 run 跑完 | 子结果作为 `run_subagent`/`run_skill` 的 `<tool_result>` |
| **后台**（`run_in_background`）| 异步并发跑 | 立即拿 `agentId`；完成时收一条 **`<task-notification>`**（含 `<status>` + `<usage>`）；中途可读进度 / 停 |
| **并行** | 一轮内发多个 spawn → 并发执行 | 各自的结果 / 通知 |

**管理 API（Infra 提供给父 agent / Runtime）**：
- `run_subagent` / `run_skill`（前台或后台，§3.6 工具）—— spawn。
- `subagent_output(agentId)` —— 读后台任务的中间进度 / 已产出。
- `stop_subagent(agentId)` —— abort 一个运行中的子 run。
- 完成通知：Infra 在子 run 终态时 emit `AgentEvent`（`<task-notification>`），父 loop 收到后注入下一轮。

### 不变量

- **不允许嵌套（对齐 CC `isInForkChild`，只有顶层能 spawn）**：fork 出的子 agent ——① **工具集不含任何 spawn-class 工具（`run_subagent` / `run_skill`（按 `ToolSpec.isSpawn` 标记剔除、不靠 name 白名单，覆盖未来任何新增 fork 类工具））**，从根上没法再 fork（首选机制，对齐 Claude Code——CC 的 fork child 里再 fork 会被拒）；② 运行时带 `is_subagent` 布尔标记（顶层 run = `false`，`ForkRuntime::child()` 产出的子运行时 = `true`），再 fork 一律拒（兜底守卫：`run_forked_agent` / `spawn_or_run` 见 `is_subagent == true` 即返回 invalid_input，防 spawn 工具未被正确剔除）。**没有深度计数**，只有顶层 agent 能 spawn。`create_skill`（写文件、不递归）不算 spawn，子 agent 保留。
- **只回末轮文本**：父对话只拿子 run **末轮**（不再发起工具调用、给出最终答案那一轮）的 assistant 文本（+ 后台通知 + 可选进度）；子的中间轮铺垫与工具机制**都不回父**（上下文卫生 = fork 的核心价值，对齐 Claude Code「只取最后一条消息」）。实现上靠「每遇 `ToolStart` 清空文本累加器」保留末轮，并在子 system prompt 提示子把完整结论放最后一条消息（见「机制」）。
- **审计独立**：每个子 run 全量消息按自己的 `conversation_id` + `parentRunId` 落 `agent_messages`，可单独 replay。
- skill 执行复用本机制：`run_skill` = 以 `SKILL.md` 全文为 `prompt` 调 `run_forked_agent`（见 §3.6 Skill 子系统）。

### 暂不做（CC 有，本项目用不上 / 后续）

- **`worktree` / `remote` 隔离**：CC 给编码场景（独立 git 工作树 / 远端 CCR）；本项目（A 股研究，不在工作树改代码）不需要。
- **`SendMessage` / teams / swarms**：多 agent 互发消息协作；本项目 fan-out + 汇总即可，过重，不做。

### 实现注记（2026-06-02 已落地：`infrastructure/agent/subagent.rs`）

机制 + 任务注册表 + 前台/后台/并行 + `run_subagent`/`run_skill`（替换 inline `load_skill`）+ `stop_subagent`/`subagent_output` 均已实现 + hermetic 测试（ScriptedProvider，无网络）。依赖注入分两层：`ForkHandle` 只持**静态依赖**（ProviderFactory + registry + repo + SkillStore + 任务注册表 + 占位默认配置）；**运行时上下文**（channel / is_subagent / parentRunId / 父 event_tx）由 `ForkRuntime` 在**发起 run 时注入**——经 `DispatchExt`（对 registry 不透明的 `Arc<dyn Any>`）透传给 `run_agent_turn_forked`，fork handler 在 **dispatch 时** downcast 回 `ForkRuntime` 读取。**不允许嵌套（对齐 CC `isInForkChild` 布尔，无深度计数）**：构造子 run registry（`child_registry`）时**无论 `allowedTools` 是否给定，都剔除全部 spawn-class 工具（按 `ToolSpec.isSpawn` 标记，覆盖 `run_subagent`/`run_skill`（及未来任何 isSpawn 工具））**——子 agent 的 system prompt 里根本没有这些工具，从根上没法再 fork（首选机制，对齐 CC）。另设兜底布尔守卫：`ForkRuntime` 顶层 run = `is_subagent=false`，`ForkRuntime::child()` 把派生的子运行时置 `is_subagent=true`；`spawn_or_run` 在发起 fork 前预检发起方 `is_subagent`，`run_forked_agent` 再对发起方 `is_subagent` 兜底一次——`is_subagent == true` 即拒（防 spawn 工具未被正确剔除）。hermetic 测试 `subagent_has_no_fork_tools_no_nesting` 覆盖「子 registry 无 spawn 工具 + 子尝试 `<use_tool name="run_subagent">` 被当未注册 tool 拒、不产生第二层子 run」，`subagent_flag_refuses_nested_fork` 覆盖布尔守卫。以下几点**当前为务实折中 / 待补**：

- **`parentRunId` 关联**：暂编码在子 `conversation_id`（`fork:<parentRunId>:<uuid>`）+ 内存 `SubAgentTask`，**未加 DB 列**（加列 = migration + domain 改动）。要按父 replay 审计再加列。
- **`<task-notification>`**：暂以 `AgentEvent::TextDelta` 文本信封发（不新增 event 变体，保协议不变）；后续可加专用变体。
- **token 预算**：spec 已定义 `run_agent_turn` 的 `tokenBudget` 入参 + `token_budget_exceeded` stop_reason + 子 usage 回灌父累加（见 §6 `run_agent_turn` / `AgentStopReason`）。**执行实现是 Infra 自己的增量（不绑 Phase 3）**——loop 已按 turn 累计 usage（`SubAgentTask.progress.tokens`），只需在 turn 边界加一道预算检查 + 超限 emit stop_reason 即可，可随时独立落地；当前代码尚未接预算检查，子继承父 compaction/window。
- **生产接线**：fork 上下文已改为 **run 时注入**（`ForkRuntime` → `DispatchExt`），**不再静态捕获、也不需要 registry replace**。`bootstrap` 的 `ForkHandle` 仅装静态依赖 + 占位默认配置；Phase 3 接线只需触发入口（Tauri command / scheduler / Runtime）在发起 run 时构造 `ForkRuntime`（真实 channel / parentRunId / 父 event_tx）传给 `run_agent_turn_forked`。即：**fork 机制已就绪 + 单测通过（含 `is_subagent` 布尔守卫拒绝嵌套的回归）；生产联动随 Phase 3 的 run 触发接入。**

---

## 3.6 Infra 默认 Tools + Skill 子系统（业务无关，Infra 注册）

> **归属澄清**：以下通用 tool 与 skill 子系统**全部由 Infra 默认注册、属 Infra 层**（业务无关，代码在 `infrastructure/agent/`，bootstrap 时注册）。**Runtime 不拥有、不重复定义**——Runtime 只负责：触发 Agent、**注入领域 tool**（fetch_quotes / operate_account 等）、联合不同 domain。Infra 只定义 `ToolRegistry` / `ToolSpec` / 调用审计 / spawn-class 等通用协议，以及 Infra 自己默认注册的 tool 名；Runtime 的领域 tool 名和 schema 只定义在 [agent-runtime-module.md §4](agent-runtime-module.md#4-工具)。Infra 不维护“全产品工具枚举”，对 Runtime 注入的 tool name 只按已注册的 opaque `ToolName` 处理。

Infra 默认注册的通用 tool：

| tool | 作用 | 契约要点 |
|---|---|---|
| `read_file` | 任意路径只读 | `{path, offset?, limit?}` → `{content, truncated}` |
| `write_file` / `edit_file` | **仅工作区**写 / 定向改 | 越界 → `path_outside_workspace`；edit 未命中/不唯一 → `invalid_input` |
| `run_bash` | 任意路径跑命令，cwd 默认工作区 | 危险命令 denylist → `command_rejected`；约定级沙箱 |
| `todo_write` | 多步任务的步骤清单（agent 进度便签） | `{items:[{content,status}]}` 整表替换 → 回 `{items}`；纯便签 `SideEffect::None` |
| `web_search` | 联网搜索（多源并行聚合） | `{query, maxResults?}` → `{results:[{title,url,snippet,source}], providers}`；未配置任何源 → `invalid_input` |
| `web_extract` | 读网页正文（无 key） | `{urls(≤5)}` → `{results:[{url,title,content}\|{url,error}]}`；多 URL 并行 + 单条容错；当前仅 HTML |
| `run_subagent` | fork 隔离子 agent 跑子任务 | 见 §3.5；只回结果 |
| `create_skill` | 写 `<skills_dir>/<name>/SKILL.md` | `{name(slug), description, body}` → `{path, created}` |
| `run_skill` | fork 子 agent 跑某 skill | `{name, args?}` → `{name, result}`；以 SKILL.md 为 prompt（§3.5）|

**约定级沙箱**：write/edit 锁工作区（`<appData>/gangzi/workspace/`，路径规范化防 `..` 逃逸）；read/bash 不限路径；危险命令门禁。`run_bash` 不限路径可绕过写限制——约定级接受（强隔离需 OS sandbox，后续可选）。

**`web_search`（联网搜索，多源并行聚合）**：
```ts
type WebSearchInput  = { query: string; maxResults?: number };           // 默认 8
type WebSearchResult = { title: string; url: string; snippet: string; source: string }; // source=哪个源
type WebSearchOutput = { results: WebSearchResult[]; providers: string[] }; // providers=本次参与的源
```
- **可插拔多源 + 并行聚合**（参考 hermes-agent `WebSearchProvider` 抽象，但 hermes 是单源 dispatch，本项目要并行聚合）：
  - `WebSearchProvider` trait（`name` + `search(query,max)`）；每个搜索源一个实现。
  - `MultiWebSearch` 聚合器：对所有 **已启用** 源 **并行 fan-out** → 单源失败容错（忽略，不拖垮整体）→ **按 canonical URL 去重** → 跨源交错排序 → 回带 `source` 标签的聚合列表。
- **初始源**：DuckDuckGo（免费无 key，HTML 抓取）、Jina（免费，可选 key 提额）、博查 Bocha（需 key，中文最佳）、Tavily（需 key，免费额度）。trait 抽象使后续加 SearXNG / Brave 等只需各加一个实现。
- **配置**：provider key / 开关由 adapter 从设置注入（key 只写不回显，同 LLM key）；一个源都没启用 → 工具返回 `invalid_input`（明确提示去配置）。`SideEffect::None`（只读外部、不写本地）。前端**不**直接发外部 HTTP——全部走 Rust（架构红线）。
- provider 各自的 wire format（端点 / 鉴权 / 响应字段）是 infra 实现细节，不在 spec 固化（类比 TDX / news provider adapter）。

**`web_extract`（读网页正文，无 key）**：
```ts
type WebExtractInput  = { urls: string[] };  // ≤5
type WebExtractResult = { url: string; title?: string; content?: string; error?: string };
type WebExtractOutput = { results: WebExtractResult[] };
```
- `web_search` 只回摘要 + URL；`web_extract` 才读得到**原文**。两者 + fork 子 agent = 完整研究链（搜 → 读正文 → 综合回简报）；对基本面 / 产业链定性分析尤其关键（研报 / 年报正文）。
- reqwest 抓 HTML → `scraper` readability-lite 抽 main content（article/main/body 内的 p/h/li 文本，去 nav/script 噪声）→ 截断（每页 ~8000 字）。多 URL 并行、单条失败回 `{url,error}` 不拖垮整体。`SideEffect::None`，前端不发外部 HTTP（走 Rust）。
- MVP 仅 HTML；PDF（年报 / arxiv）非 HTML content-type → 回 error，后续补。

**`todo_write`（agent 步骤便签，对齐 Claude Code TodoWrite）**：
```ts
type TodoStatus = "pending" | "in_progress" | "completed";
type TodoItem = { content: string; status: TodoStatus };
type TodoWriteInput  = { items: TodoItem[] };   // 每次传完整清单，整表替换旧的
type TodoWriteOutput = { items: TodoItem[] };   // 回显当前清单
```
- **整表替换、无服务端状态**：当前清单 = 最近一次 `todo_write` 的 `items`（agent 在自己上下文里持有 + 回显验证）。`SideEffect::None`，不入库，只靠 ToolCall 审计留痕。
- **用途**：配合 L1「自主工作流」的「该拆就拆 / 收口前自检」——复杂多步任务开工写一份、随进度更新；单步 / 快问快答不必用（避免滥用）。
- 校验：`items` 空 / `content` 空白 / `status` 非法 / 项数 > 50 → `invalid_input`。
- **可见性（前端）**：UI 从 `todo_write` 的 `tool_end.outputSummary.items` 渲染 live checklist（无需独立 `AgentEvent`——复用既有 ToolEnd 信道）。

**Skill 子系统**（`SkillStore`，Infra）：
- skill = `<skills_dir>/<name>/SKILL.md`（YAML frontmatter `name`+`description` + markdown body；可选随附 `scripts/`/`references/`/`assets/`，当前只支持单 SKILL.md）。`skills_dir` = `<appData>/gangzi/skills/`，adapter 注入。
- **三级渐进披露**：① 索引（name+description）由 Infra 扫描后注入 system prompt（见 §System Prompt Tool 清单的 skill 索引段）；② `run_skill` fork 子 agent、以 SKILL.md 全文为 prompt（正文不进父上下文）；③ 子 agent 按 SKILL.md 用 `read_file`/`run_bash` 读 references / 跑 scripts。
- skill 不依赖、不编排领域 tool；正文不写 `<use_tool>` 标签。
- **简化（适配本项目）**：不支持 per-skill model/effort 覆盖（继承父）；`allowed-tools` frontmatter 可选（默认继承父工具集，写了收紧）。

#### 本地通用 tool / skill tool 完整 schema（Infra 自包含，真源在此）

```ts
// read_file —— 读任意 path（只读，不受工作区限制）
type ReadFileToolInput = { path: string; offset?: number; limit?: number };
type ReadFileToolOutput = { content: string; truncated?: boolean };
// 错误：not_found / invalid_input（目录/不可读/非文本）/ parse_error

// write_file —— 写文件（path 必须在 <workspace> 内）
type WriteFileToolInput = { path: string; content: string };
type WriteFileToolOutput = { bytesWritten: number };
// 错误：path_outside_workspace / invalid_input

// edit_file —— 定向替换（path 必须在 <workspace> 内）
type EditFileToolInput = { path: string; oldString: string; newString: string; replaceAll?: boolean };
type EditFileToolOutput = { replaced: number };
// 错误：path_outside_workspace / not_found / invalid_input（未命中 / replaceAll=false 时非唯一）

// run_bash —— 执行命令（cwd 默认 <workspace>，危险命令门禁）
type RunBashToolInput = { command: string; cwd?: string; timeoutMs?: number };
type RunBashToolOutput = { stdout: string; stderr: string; exitCode: number; truncated?: boolean };
// 错误：command_rejected / tool_timeout / invalid_input

// create_skill —— 写 <skills_dir>/<name>/SKILL.md（isSpawn=false）
type CreateSkillToolInput = { name: string; description: string; body: string };  // name 为 slug
type CreateSkillToolOutput = { path: string; created: boolean };
// 错误：invalid_input（name 非 slug / 越界 / 写盘失败）

// run_skill —— fork 子 agent 跑某 skill（isSpawn=true）
type RunSkillToolInput = { name: string; args?: string };
type RunSkillToolOutput = { name: string; result: string };   // 只回子 run 末轮结果
// 错误：not_found / invalid_input / 子 run 失败透传
```

```ts
// run_subagent —— fork 隔离子 agent 跑子任务（isSpawn=true）
type RunSubagentToolInput = {
  description: string;        // 一句话任务描述（进子 agent 任务注册表）
  prompt: string;            // 子 agent 的任务 prompt
  tools?: ToolName[];        // 可选：收紧子工具集（默认继承父；spawn-class 一律剔除）
  runInBackground?: boolean; // true=异步后台跑，立即返回 agentId；缺省 false=前台阻塞
};
type RunSubagentToolOutput = {
  agentId: string;
  result?: string;           // 前台：子 run 末轮结果文本；后台：先只回 agentId，完成经 <task-notification>
};
// 错误：invalid_input / 子 run 失败透传
```

#### `ToolName` / `InfraToolName`

```ts
// ToolName 是 ToolRegistry 里的已注册 tool 名，Infra 对 Runtime 注入的领域名保持 opaque。
// 领域 tool 名与 schema 由 agent-runtime-module.md §4 定义。
type ToolName = string;

type InfraToolName =
  | "read_file" | "write_file" | "edit_file" | "run_bash"
  | "run_subagent" | "create_skill" | "run_skill";
```

- spawn-class（`isSpawn=true`，子 agent 内被剔除）：`run_subagent` / `run_skill`。（Runtime 当前不注入 fork 类领域工具；临时复盘复用 `run_subagent` 收紧只读，见 agent-runtime §3。`isSpawn` 机制对未来任何新增 fork 类工具仍自动生效。）
- Runtime 新增领域 tool 必须先扩展 [agent-runtime-module.md §4](agent-runtime-module.md#4-工具) 或对应模块 spec，再通过 `ToolRegistry` 注册；Infra 不在本文件重复列举 Runtime tool。
- `allowedTools` / `RunSubagentToolInput.tools` 中出现未注册 tool name 时，按 `invalid_input` 拒绝；已注册 tool 的归属由注册方 spec 负责。

---

## 4. 上下文管理

上下文由四类内容构成，按**承载位置**分两处（见 §2 ContextBundle「单 lane」设计）：

| 类型 | 内容 | 承载位置 | 生命周期 |
|---|---|---|---|
| Identity / System | Agent 身份、运行纪律、tool 清单（由 SystemPromptBuilder 注入） | `ContextBundle.systemParts`（`kind=system`） | 长期，适合 cache |
| 实时上下文（L3） | trigger、账户、行情、新闻、投资策略、近期分析结果摘要 | `ContextBundle.systemParts`（`kind=realtime`，`droppable` 由 Runtime 标） | 每次 run 重建 |
| Chat Context | 用户最近对话、当前问题、历史 tool_result | 独立 `messages: AgentMessage[]`（repo 自动续接） | 只服务交互 |
| Review / Memory | 用户偏好、复盘建议、策略说明 | `ContextBundle.systemParts`（`kind=memory`） | 独立存储，按需注入 |

> 压缩的两个作用面：① **message-lane**（`messages` vec 里的多轮历史 + `<tool_result>`）—— MicroClear / Summarize / Drop 的主战场，run 过程中拉的实时数据都在这里；② **systemParts** —— realtime packet 是 run-fresh、量小，默认不做 run 内压缩（如需可按 `droppable` 清理）。

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
| soft limit | 估算 token 超过 `context_soft_limit_tokens` | 先 MicroClear（清理旧易腐 tool 结果，替换为 `<tool_result_stub />`，保留最近若干条）；仍过大时进入 Summarize / Drop |
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
- 投资策略写入结果（Runtime 的 `upsert_investment_strategy` 操作）
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
run_agent_turn(request, registry, context, providers, event_tx, repo, cancel) -> RunSummary;
// cancel: CancellationToken。被取消 → 停后续 turn + emit done(stop_reason=cancelled)（§3）。
// 不需要取消的入口传一个未触发的 token 即可。
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
  tokenBudget?: TokenBudget;          // 本 run 的 token 预算（Runtime 从 settings 传入）；缺省无上限
};

type TokenBudget = {
  runTokens: number;                  // 本 run 累计 token 上限（含 fork 子 run 回灌的 usage）
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

设置页通过 specta 强类型 command 操作（不裸调 invoke、apiKey 只提交不回读）：`agent_list_channels`（屏蔽 apiKey）/ `agent_add_channel` / `agent_update_channel` / `agent_remove_channel` / `agent_set_active_channel` / `agent_discover_models` / `agent_channel_presets`（返回内置快速预设）。

- `agent_update_channel`：按 `channel_id` 更新已有渠道（渠道名 / 消息格式 / Host / model / enabled，可选 apiKey）。**apiKey 语义**：input 的 apiKey 为空 / 省略 = **保留原有 key 不变**（与"只提交不回读"一致——前端不持有明文，编辑时不预填、留空即不改）；非空才覆盖。不修改 `is_active`（当前模型由 `agent_set_active_channel` 单独管）；未在 input 暴露的能力字段（如 thinking budget）保留原值。`channel_id` 不存在 → `not_found`。

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
- 投资策略生命周期。
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
