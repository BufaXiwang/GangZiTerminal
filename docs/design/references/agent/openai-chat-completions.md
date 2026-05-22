# OpenAI Chat Completions Channel Reference

> 本文档是 Agent 模块 OpenAI-compatible `/v1/chat/completions` wire format 的渠道契约。模块级领域契约见 [../../agent-infra-module.md](../../agent-infra-module.md)。

## 定位

OpenAI Chat Completions 是 OpenAI-compatible 模型渠道：OpenAI、DeepSeek、火山方舟、vLLM、Ollama、本地模型等都可以通过该 wire format 接入。

抽象轴是 wire format，不是厂商。新增兼容厂商通常只新增 channel config，不新增 provider 实现。

## 能力矩阵

| 能力 | 支持 |
|---|---:|
| Streaming | 必须 |
| Local tools | 必须，若模型支持 tool_calls |
| Image input | 可选，取决于模型 |
| Thinking | 不作为 canonical thinking 恢复 |
| Server-side web_search | 不支持 |
| Prompt cache hints | 不保证 |

## Request Mapping

| Canonical | Chat Completions |
|---|---|
| system / user / assistant text | `messages[]` |
| image | `content[]` 中 `image_url` data URL |
| ToolUse | assistant message `tool_calls[]` |
| ToolResult | `role = "tool"` message |
| server web_search | 丢弃 / 禁用 |
| thinking / redacted thinking | 丢弃 |

规则：

- `tool_calls[].id` 必须映射到 canonical tool call id。
- 多个 tool call 按 provider 返回顺序执行和回填。
- 不支持 tools 的模型不能作为需要自动交易的主 Agent 渠道。
- server-side web_search 不下发到 Chat Completions。

## Streaming Mapping

| Chat stream delta | AgentEvent |
|---|---|
| content delta | `text_delta` |
| tool_calls delta complete | Agent loop 生成 `tool_start` |
| finish_reason | `done` 的 stop reason 输入 |
| usage | `usage`，如果 provider 返回 |
| error | `error` |

## Error Mapping

| Provider error | ErrorCode |
|---|---|
| rate limit | `provider_unavailable`，retryable |
| timeout | `provider_unavailable`，retryable |
| invalid tool schema | `invalid_input` |
| context too long | `provider_context_too_long` |
| tools unsupported | `provider_unavailable` 或 channel disabled |

## 验收标准

- Chat Completions tool calls 必须按原始 index 稳定回填。
- 不支持 tool_calls 的模型不能执行 `operate_account`。
- Chat Completions 路径不得下发 server-side web_search。
- compatible provider 差异通过 channel config 处理，不污染 Agent domain。
