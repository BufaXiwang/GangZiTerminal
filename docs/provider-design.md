# Provider 抽象设计

## 抽象轴：wire format，不是厂商

接 OpenAI 的过程暴露了一个事实：抽象轴不该是"厂商"，而该是 **wire format**。

| Provider 实现             | 端点                       | 谁在用                                              |
|--------------------------|---------------------------|---------------------------------------------------|
| `AnthropicProvider`      | `/v1/messages`            | Anthropic 官方、claude-relay、Bedrock              |
| `OpenAIResponsesProvider`| `/v1/responses`           | OpenAI 官方（gpt-5.5 / o3 推荐）                    |
| `OpenAIChatCompletionsProvider` | `/v1/chat/completions` | OpenAI、DeepSeek、火山方舟、vLLM、Ollama、本地模型 |

DeepSeek 不需要新 provider——它仿 Chat Completions，换 base_url 就行。火山方舟、Together、Groq、Cerebras、本地 vLLM 也是这个待遇。

加新 wire format 才需要新 provider 文件。这是理想的扩张曲线。

## Canonical Block 形态映射

内部消息以 Anthropic content-block 形态作为 canonical（最具表达力的超集）。三种 wire format 各自的翻译：

| canonical Block          | Anthropic Messages          | OpenAI Responses                      | OpenAI Chat Completions               |
|--------------------------|----------------------------|---------------------------------------|---------------------------------------|
| `Text`                   | `text` block               | `input_text` / `output_text` part     | `messages[].content` 字符串/array     |
| `Thinking`               | `thinking` block (有 sig)  | **丢弃**（v1）                         | **丢弃**                              |
| `RedactedThinking`       | `redacted_thinking`        | **丢弃**                               | **丢弃**                              |
| `Image`                  | `image` (base64 source)    | `input_image` (data URL)              | `image_url` (data URL)                |
| `ToolUse`                | `tool_use` block           | `function_call` 顶层 Item，带 `call_id`| `tool_calls[]` 在 assistant 消息内    |
| `ToolResult`             | `tool_result` block (user) | `function_call_output` 顶层 Item       | `role: "tool"` 单独消息               |
| `ServerSide(WebSearch)`  | `web_search_20250305`      | `{type:"web_search"}`                  | **丢弃**（端点不支持）                |

## 几个关键决策

### 1. Thinking / 推理内容暂不跨 provider 转发
OpenAI Responses 的 `reasoning` item 需要 server 给的不透明 `id`，跨调用恢复要求保持那个 id。我们的 canonical 没保留这个 id，跨 provider 把 thinking 原样塞进去会被拒。v1 简化处理：在 canonical → OpenAI 翻译时丢弃 `Thinking` / `RedactedThinking`。这意味着**思考内容不会跨 turn 持久化**，但每个 turn OpenAI 会重新思考——对 stateless 调用没影响。

后续要支持 thinking 跨 provider 的话，方案是给 `Thinking` block 加 `provider_specific_id: Option<String>` 字段，由各 provider 在 SSE 解码时填充并在 wire 翻译时仅当 provider 匹配才回写。

### 2. Tool 定义的 strict 模式默认关
OpenAI Responses 的 function 默认 `strict: true`，要求 schema 严格符合 JSON Schema subset（包括 `additionalProperties: false`）。我们已有的工具 schema 没全这么写——v1 全部以 `strict: false` 下发，避免"Strict mode requires X" 系列报错。后续可逐工具开。

### 3. Server-side web_search 的差异
- Anthropic 的 `web_search_20250305` 支持 `allowed_domains` / `blocked_domains` / `max_uses`。
- OpenAI Responses 的 `web_search` 当前只接 `{type: "web_search"}`，没有等价的域过滤。
- OpenAI Chat Completions 端点根本没有内置 web_search。

`AgentConfig::web_search_enabled()` 方法按当前 provider 返回正确的开关：
- Anthropic：`anthropic.enable_native_web_search`
- OpenAI Responses：`openai.enable_web_search`
- OpenAI Chat：恒为 `false`

`ToolDef::ServerSide(AnthropicWebSearch)` 在 Anthropic 路径原生序列化；在 OpenAI Responses 路径降级为无参的 `{type: "web_search"}`；在 OpenAI Chat 路径被 `filter_map` 丢弃。

### 4. Tool ordering 在并行执行时按原始 index 回填
模型给的 `tool_use` 顺序在某些边界情况比 `tool_use_id` 更可靠（OpenAI Chat Completions 的 `tool_calls[]` 数组、Anthropic content_block 的 index）。`execute_tools_parallel` 用 `FuturesUnordered` 并行调度但记录每个 future 的原始 index，最后按 index 排序回填——并行性能 + 顺序稳定性兼得。

### 5. Token 估算 calibrated for CJK
旧的 `len()/4` 对中文是 3 字节/字符，估算严重偏高。`agent::context::estimate_tokens` 把字符按 ASCII / 非 ASCII 分两类：ASCII 4 字符/token，CJK 2 字符/token。误差从 ±30% 降到 ±15%，对 soft_limit 这种软警戒线足够。

## ContextManager 的两级压缩

`agent::context::compact_if_needed` 在每次 provider 调用前跑一次：

1. **NoOp**：估算 ≤ soft_limit → 原样返回
2. **Micro**：把 `keep_last_n*2` 之外的老消息里大段（>200 字符）`tool_result` 内容截成 stub，保留 `tool_use_id`。降到 soft_limit 就停。
3. **Drop**：micro 还不够 → 从最老消息开始整条丢，prepend `[N earlier messages omitted]` 占位 user message。降到 soft_limit 就停。
4. **HardLimit**：drop 完仍 > hard_limit → 调用方应该 abort run，不要再调 provider（必撞 4xx context_too_long）。

什么时候该升级到"用便宜模型摘要"：当 user 反复跑完整天的 chat session 看到 Drop 频繁丢失上下文时。届时新增 `summary_compact(messages, compact_provider, model)` 方法，在 micro 之后、drop 之前插一层。

## RunSummary 与 stop_reason 处理

`RunSummary.stop_reason` 是 pipeline 检查输出可用性的关键。当前规则：

- **chat pipeline**：所有 stop_reason 都接受。`MaxTokens` 时在 assistant 文本末尾追加"被 max_tokens 截断"提示，让用户知道为什么戛然而止。
- **briefing/review pipeline**：JSON 输出协议——`MaxTokens` 时直接报错"无法解析为完整 JSON"，不让 `parse_briefing` 拿到半段 JSON 抛 serde 错。

## Tool 超时

每个 tool 调用包了 `tokio::time::timeout(tool_timeout, ...)`，默认 30s（`AgentRuntimeConfig.tool_timeout_secs`）。超时返回 `is_error=true` + "工具 X 调用超时" 文案——agent 下一轮看到错误自己决定是否换工具或放弃。这避免了 NewsNow / Eastmoney 单次卡死把整个 run 拖死。

## 观测：agent_episodes 表关键字段

每次 run 落一行：
- `provider` 列存 wire format 名（`anthropic` / `openai_responses` / `openai_chat_completions`），不是模型名
- `model` 列存模型 id（`claude-opus-4-7` / `gpt-5.5-thinking` 等）
- `cache_read_tokens` 是判断 prompt cache 是否真实生效的核心指标——同一 chat 第二轮起应该 > 0

## 还没做但记一下

- **Retry / backoff**：`ProviderError::RateLimited` / `Transient` 已经分类但 loop 直接上抛。下一步给 loop 加退避重试（指数 backoff，最多 2 次）。
- **Summary compact**：上面提到。
- **Provider capabilities**：当前抽象不暴露 capabilities（supports_thinking、max_context_tokens 等）。代码里硬编码"thinking 仅 Anthropic 路径用"是丑的。等到第三个 wire format 加入时（推测是 Gemini 或 Bedrock Converse）再抽。
- **Cache breakpoint 在 OpenAI 路径不可控**：OpenAI 的 prompt cache 是 implicit 的（按 prefix 自动缓存），用户没法精细控制断点。canonical 的 `cache_control: bool` 字段在 OpenAI provider 路径被忽略——这是 wire format 差异，不是 bug。
