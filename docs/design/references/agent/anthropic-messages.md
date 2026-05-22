# Anthropic Messages Channel Reference

> 本文档是 Agent 模块 Anthropic `/v1/messages` wire format 的渠道契约。模块级领域契约见 [../../agent-infra-module.md](../../agent-infra-module.md)。

## 定位

Anthropic Messages 是 Agent 支持的主 wire format 之一。channel adapter 负责 canonical request / event 与 Anthropic Messages API 的互转，不执行 tool loop，不管理业务上下文。

## 能力矩阵

| 能力 | 支持 |
|---|---:|
| Streaming | 必须 |
| Local tools | 必须 |
| Image input | 可选，取决于模型 |
| Thinking | 可选，取决于模型 |
| Server-side web_search | 可选，取决于配置 |
| Prompt cache hints | 可选 |

## Request Mapping

| Canonical | Anthropic Messages |
|---|---|
| system text | top-level `system` |
| user / assistant text | content `text` block |
| image | content `image` block |
| thinking | `thinking` block |
| redacted thinking | `redacted_thinking` block |
| ToolUse | assistant `tool_use` block |
| ToolResult | user `tool_result` block |
| local tool def | `tools[]` |
| server web_search | `web_search_20250305` tool |

规则：

- tool result 必须保留 `tool_use_id`，不能破坏配对。
- thinking block 的跨 turn 恢复必须保留 provider 所需签名；不能跨 wire format 强行转发。
- server-side web_search 只在配置启用时下发。

## Streaming Mapping

Anthropic stream 必须 normalize 成 `AgentEvent`：

| Anthropic event | AgentEvent |
|---|---|
| message start | `run_start` 已由 Agent loop 发出，channel 不重复发业务事件 |
| content text delta | `text_delta` |
| thinking delta | `thinking_delta` |
| tool_use content block complete | Agent loop 生成 `tool_start` |
| message stop | `done` 的 stop reason 输入 |
| usage | `usage` |

## Error Mapping

| Provider error | ErrorCode |
|---|---|
| rate limit | `provider_unavailable`，retryable |
| timeout | `provider_unavailable`，retryable |
| invalid request / schema | `invalid_input` |
| context too long | `provider_context_too_long` |

## 验收标准

- Anthropic stream 文本必须逐步映射为 `text_delta`。
- local tool use 必须保留原始顺序和 `tool_use_id`。
- tool timeout 作为 tool result error 返回给模型，而不是中断整个 run。
- context too long 必须触发 Agent 压缩策略或返回明确错误。
