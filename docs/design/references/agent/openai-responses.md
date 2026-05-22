# OpenAI Responses Channel Reference

> 本文档是 Agent 模块 OpenAI `/v1/responses` wire format 的渠道契约。模块级领域契约见 [../../agent-infra-module.md](../../agent-infra-module.md)。

## 定位

OpenAI Responses 是 Agent 支持的主 wire format 之一。channel adapter 负责 canonical request / event 与 Responses API 的互转，不执行 tool loop，不管理业务上下文。

## 能力矩阵

| 能力 | 支持 |
|---|---:|
| Streaming | 必须 |
| Local tools | 必须 |
| Image input | 可选，取决于模型 |
| Thinking / reasoning | 可选，按 provider 差异处理 |
| Server-side web_search | 可选，参数能力受限 |
| Prompt cache hints | OpenAI implicit cache，adapter 不控制断点 |

## Request Mapping

| Canonical | OpenAI Responses |
|---|---|
| text | `input_text` / `output_text` |
| image | `input_image` |
| ToolUse | `function_call` item |
| ToolResult | `function_call_output` item |
| server web_search | `{ type: "web_search" }` |
| thinking / redacted thinking | 第一阶段丢弃，不跨 provider 转发 |

规则：

- OpenAI Responses function schema 默认 `strict = false`，直到工具 schema 全部满足 strict subset。
- `function_call.call_id` 必须映射到 canonical tool call id。
- `function_call_output` 必须按 call id 回填。
- Responses web_search 不支持 Anthropic 等价的 allowed / blocked domains；adapter 不伪造域过滤。

## Streaming Mapping

| Responses stream item | AgentEvent |
|---|---|
| output text delta | `text_delta` |
| reasoning delta / summary | `thinking_delta`，如果 provider 暴露且配置允许 |
| function_call complete | Agent loop 生成 `tool_start` |
| usage | `usage` |
| completed | `done` 的 stop reason 输入 |
| error | `error` |

## Error Mapping

| Provider error | ErrorCode |
|---|---|
| rate limit | `provider_unavailable`，retryable |
| timeout | `provider_unavailable`，retryable |
| invalid function schema | `invalid_input` |
| context too long | `provider_context_too_long` |
| unsupported web_search | `provider_unavailable` 或禁用 server-side tool |

## 验收标准

- Responses tool call 必须转换为 canonical `ToolUse` 并进入统一 ToolRegistry。
- tool output 必须用原 `call_id` 回填。
- thinking / reasoning 不可安全恢复时必须丢弃，不能导致 provider 4xx。
- web_search 参数差异必须在 channel adapter 内处理，不泄漏到 Agent 业务逻辑。
