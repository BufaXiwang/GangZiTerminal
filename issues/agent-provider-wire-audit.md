# Agent Provider Wire Audit

Date: 2026-05-30

Basis:

- `docs/design/references/agent/openai-responses.md`
- `docs/design/references/agent/openai-chat-completions.md`
- `docs/design/references/agent/anthropic-messages.md`
- `src-tauri/src/infrastructure/agent/`

## Executive Summary

The current provider wire is correct for basic streaming text calls across all three formats, and the basic image request shape is correct for OpenAI Responses, OpenAI Chat Completions, and Anthropic Messages.

There are still concrete correctness gaps:

1. Chat Completions uses deprecated `max_tokens` instead of `max_completion_tokens` for official OpenAI newer/o-series models.
2. Chat Completions always uses `system` messages for instructions, while the current OpenAI reference says o1/newer models should use `developer` messages.
3. Anthropic thinking is only partially wired: request-level `thinking` config is missing, and `redacted_thinking` is serialized in the wrong shape.
4. Real HTTP calls cannot dereference `payload://...` images because `HttpProvider` passes `PayloadStore = None`.
5. Streaming `TextDelta` is emitted twice: once by provider SSE parsing and once by `loop_executor`.
6. Usage events mix per-turn and cumulative semantics; Anthropic cache usage details are also dropped.
7. Responses stream handling is a minimal subset and does not handle incomplete/richer response status details.

## OpenAI Responses: `/v1/responses`

### Correct

Implementation:

- `HttpProvider` maps `WireFormat::Responses` to `/v1/responses`.
- Auth uses `Authorization: Bearer ...`.
- `OpenAIResponsesAdapter` sends:
  - `model`
  - `stream`
  - `input`
  - optional `instructions`
  - optional `max_output_tokens`

Reference alignment:

- `input` accepts message arrays and content blocks.
- `instructions` is valid.
- `max_output_tokens` is valid and includes visible output plus reasoning tokens.
- `input_text` is valid.
- `input_image.image_url` accepts a base64 data URL.
- Stream example includes `response.output_text.delta` and `response.completed`, both handled by current code.

### Gaps

The reference exposes a richer Responses API than the current adapter uses:

- `reasoning` config is available for GPT-5/o-series models, but current code never sends it.
- `stream_options.include_obfuscation` is available, but current code does not expose it.
- `tools` and `tool_choice` are available, but current project appears to intentionally avoid provider-native tools.
- `previous_response_id` / conversation state are available, but current project owns conversation state locally.

Stream handling is minimal:

- Current code handles `response.output_text.delta`, `response.reasoning_summary_text.delta`, `response.completed`, `response.failed`, and `response.error`.
- It ignores item lifecycle events such as `response.output_item.added`, `response.content_part.added`, `response.output_text.done`, `response.content_part.done`, and `response.output_item.done`.
- It does not explicitly handle incomplete responses or map `incomplete_details`.

Recommendation:

- Add `response.incomplete` / `incomplete_details` handling.
- Add channel config for Responses `reasoning` if reasoning models are supported as first-class channels.
- Keep provider-native tools disabled only if `agent-infra-module.md` explicitly remains text-protocol-only.

## OpenAI Chat Completions: `/v1/chat/completions`

### Correct

Implementation:

- `HttpProvider` maps `WireFormat::ChatCompletions` to `/v1/chat/completions`.
- Auth uses `Authorization: Bearer ...`.
- `OpenAIChatAdapter` sends:
  - `model`
  - `stream`
  - `messages`
  - `stream_options.include_usage` when streaming

Reference alignment:

- `messages` is the core request field.
- user content may be text string or content parts.
- image content part type is `image_url`.
- `stream_options.include_usage` is valid only with `stream: true`.
- The final usage chunk before `data: [DONE]` has empty `choices` and populated `usage`; current parser supports that.

### Issue: `max_tokens` vs `max_completion_tokens`

Current code:

- `OpenAIChatAdapter` writes `body["max_tokens"]`.

Reference:

- `max_completion_tokens` is the current completion cap.
- `max_tokens` is deprecated in favor of `max_completion_tokens`.
- `max_tokens` is not compatible with o-series models.

Impact:

- Official OpenAI newer/o-series Chat Completions calls can fail or behave incorrectly.
- Some OpenAI-compatible vendors may still require `max_tokens`.

Recommendation:

- Add channel-level token-limit strategy:
  - official OpenAI newer/o-series: `max_completion_tokens`
  - compatible vendors: `max_tokens` unless configured otherwise

### Issue: instruction role should be configurable

Current code:

- System context is always sent as `{ "role": "system", "content": ... }`.

Reference:

- The Chat Completions reference includes `developer` messages.
- It says o1 models and newer use developer messages for this purpose.

Impact:

- Current mapping is acceptable for many compatible providers.
- It is not the most accurate mapping for official OpenAI newer/o-series channels.

Recommendation:

- Add channel-level instruction role strategy:
  - `system` for legacy/compatible providers
  - `developer` for official OpenAI newer/o-series models

## Anthropic Messages: `/v1/messages`

### Correct

Implementation:

- `HttpProvider` maps `WireFormat::Messages` to `/v1/messages`.
- Auth headers include:
  - `x-api-key`
  - `anthropic-version`
- `AnthropicAdapter` sends:
  - `model`
  - `stream`
  - `max_tokens`
  - `messages`
  - optional top-level `system`

Reference alignment:

- `max_tokens` is required.
- `messages` is required.
- input messages have no `system` role; system prompt goes in top-level `system`.
- message content may be a string or content block array.
- text blocks use `type: "text"`.
- image blocks use `type: "image"` with base64 source.

### Issue: request-level thinking config is missing

Current code:

- `supports_thinking` only controls whether existing `AgentMessageBlock::Thinking` blocks are replayed.
- The request body never includes the top-level `thinking` field.

Reference:

- `thinking` is an optional request field.
- Enabled thinking can require `budget_tokens >= 1024` and less than `max_tokens`.
- Adaptive thinking can be sent as `{ "type": "adaptive" }`.
- `display` may be `summarized` or `omitted`.

Impact:

- Setting `supports_thinking = true` does not actually enable Anthropic extended thinking generation.

Recommendation:

- Add explicit Anthropic thinking config to channel configuration.
- Validate `budget_tokens < max_tokens`.
- Emit top-level `thinking` only when configured and supported by the selected model.

### Issue: `redacted_thinking` shape is wrong

Current code:

- `AnthropicAdapter` always builds `type: "thinking"` for thinking blocks.
- If metadata contains `redacted`, it inserts a `redacted` field inside that thinking object.

Reference:

- Normal thinking block shape is `{ type: "thinking", thinking, signature }`.
- Redacted thinking block shape is `{ type: "redacted_thinking", data }`.

Impact:

- Redacted thinking replay can produce invalid Anthropic wire.

Recommendation:

- Represent redacted thinking as its own block:
  - `type: "redacted_thinking"`
  - `data: ...`
- Keep normal thinking as:
  - `type: "thinking"`
  - `thinking: ...`
  - `signature: ...`

### Issue: usage drops cache details

Current code:

- Anthropic stream parsing reads `input_tokens` and `output_tokens`.
- `AgentEvent::Usage` always emits `cache_read_tokens: None` and `cache_write_tokens: None`.

Reference:

- Usage includes `cache_creation_input_tokens`.
- Usage includes `cache_read_input_tokens`.
- Usage also includes cache creation breakdown and `output_tokens_details.thinking_tokens`.

Impact:

- Cache usage and thinking-token observability are lost.

Recommendation:

- Parse and preserve cache read/write token counts.
- Decide whether `thinking_tokens` belongs in `UsageBreakdown` or separate provider metadata.

### Note: streaming signature events need the streaming reference

The local Anthropic file is the Messages create reference. It links to a separate streaming reference. This file confirms thinking/redacted block shapes and request-level thinking config, but it does not fully enumerate streaming delta event shapes such as thinking signatures.

Recommendation:

- Add the Anthropic Messages streaming reference to `docs/design/references/agent/` if signature streaming must be audited locally.

## Cross-Cutting Runtime Issues

### Real image calls cannot dereference payloads

Current code:

- `HttpProvider::next_turn` calls `build_request_body(messages, context, None)`.
- Image adapters require `PayloadStore` to dereference `payload://pl_xxx`.

Impact:

- Adapter-level image wire is correct.
- Production HTTP provider image calls fail before sending.

Recommendation:

- Inject `PayloadStore` into `HttpProvider`.
- Pass `Some(&payload_store)` into `build_request_body`.
- Add a production-path image request test.

### Streaming text is duplicated

Current code:

- Provider SSE parser emits `AgentEvent::TextDelta`.
- `loop_executor` then parses `outcome.text` and emits `AgentEvent::TextDelta` again.

Impact:

- Frontend can receive duplicate visible text.

Recommendation:

- Make one layer the owner of visible text emission.
- The other layer should only accumulate or parse without re-emitting visible text.

### Usage event semantics are ambiguous

Current code:

- Provider emits per-turn `Usage`.
- `loop_executor` emits cumulative run `Usage`.

Impact:

- Same event variant represents two aggregation levels.

Recommendation:

- Add a usage scope field, or split provider-turn usage and run-total usage into separate event variants.

## Priority

1. Fix duplicate `TextDelta` emission.
2. Add Chat Completions token-limit strategy.
3. Add Chat Completions instruction-role strategy.
4. Inject `PayloadStore` into `HttpProvider`.
5. Add Anthropic request-level thinking config and fix `redacted_thinking`.
6. Preserve Anthropic cache usage details.
7. Extend Responses stream status handling.

## Verification

This was a static wire audit. No build/tests were run. Earlier attempts in this environment showed `cargo` is not available on PATH.
