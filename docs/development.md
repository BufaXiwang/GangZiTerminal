# Development

## Commands

Run from the project root:

```bash
npm install
npm run build                                         # frontend build
cargo check --manifest-path src-tauri/Cargo.toml      # rust build
cargo test  --manifest-path src-tauri/Cargo.toml      # rust unit tests (200+)
npm run tauri -- dev                                  # one-shot dev (foreground)
```

## Tmux Runtime

Standard local dev uses the fixed `gangzi-terminal` tmux session. The Tauri dev process keeps Vite + Rust watcher running together:

```bash
npm run tmux:start
npm run tmux:logs
npm run tmux:restart
npm run tmux:stop
```

Attach for interactive log streaming:

```bash
tmux attach -t gangzi-terminal
```

## Storage

SQLite is the single source of truth. Database lives at the Tauri app data dir:

```
~/Library/Application Support/com.local.gangzi-terminal/gangzi-terminal.sqlite3
```

Logs (rotated daily) live at:

```
~/Library/Application Support/com.local.gangzi-terminal/logs/gangzi-terminal.YYYY-MM-DD.log
```

Schema migrations live in `src-tauri/src/infrastructure/db/migrations.rs`. Tables and their current writers:

| Table | Owner / writer |
|---|---|
| `app_state` (KV) | frontend UI settings via adapters; account watchlist; agent provider config; `update_memory` / `remove_memory` tools |
| `chat_messages` | chat pipeline only |
| `news_items` | news refresh + typed news repository status transitions |
| `simulated_positions` | account pipeline/service via Account aggregate + PositionRepo |
| `position_events` | account pipeline/service via Account aggregate + PositionRepo |
| `article_contents` | news article extractor cache |
| `agent_runs` | `pipeline::agent::observer` run-level audit |
| `agent_run_turns` | `pipeline::agent::observer::TurnAccumulator` per-turn audit |

`analysis_records` and old briefing/review chat rows are removed by migration. `agent_tasks` remains a historical table only; no runtime path writes it.

`app_state` keys currently in use:

- `gangzi-terminal.watchlist` — written by backend watchlist commands
- `gangzi-terminal.auto-refresh` / `.refresh-interval` / `.auto-agent` / `.buffer-size` / `.active-view` — written by frontend `useAppState`
- `gangzi-terminal.investor-memory` — written by chat agent memory tools
- `agent.config` — written by `set_agent_config` Tauri command

## Agent / Provider Configuration

Set via Settings → AI 配置 (or directly in `app_state.agent.config`):

```json
{
  "provider": "anthropic" | "openai_responses" | "openai_chat_completions",
  "anthropic": {
    "baseUrl": "https://api.anthropic.com",
    "token": "cr_xxx or sk-ant-xxx",
    "models": { "chat": "claude-sonnet-4-6", "briefing": "claude-opus-4-7", "review": "claude-opus-4-7", "compact": "claude-haiku-4-5" },
    "enableNativeWebSearch": true,
    "enableThinking": false,
    "thinkingBudgetTokens": 4000
  },
  "openai": {
    "baseUrl": "https://api.openai.com",
    "token": "sk-xxx",
    "models": { "chat": "gpt-5.5-instant", "briefing": "gpt-5.5-thinking", "review": "gpt-5.5-thinking", "compact": "gpt-5.5-nano" },
    "reasoningEffort": "low" | "medium" | "high" | null,
    "enableWebSearch": false
  },
  "agent": {
    "maxTurnsPerRun": 12,
    "maxSearchCallsPerRun": 5,
    "contextSoftLimitTokens": 80000,
    "contextHardLimitTokens": 160000,
    "compactKeepLastNTurns": 6,
    "toolTimeoutSecs": 30,
    "contextSummarizeThreshold": 120000,
    "summarizeMaxConsecutiveFailures": 3
  }
}
```

`briefing` / `review` model slots are still present in config for future pipeline reuse, but no current runtime path starts those pipelines. Current agent entry is chat.

The `provider` field selects which wire format `build_provider` constructs. All three formats share the same `domain::agent::types::AgentRequest` IR; the wire serializer is the only thing that changes. See [provider-design.md](provider-design.md) for the canonical Block ↔ wire format mapping.

`build_provider` wraps the concrete provider in `RetryingProvider` (exponential backoff + jitter, max 5 attempts on `RateLimited` / `Transient` / 5xx).

Token storage: tokens are stored plaintext in SQLite. The Settings page displays `prefix...(N chars)` masked previews; submitting the masked string preserves the existing token.

## Known Gotchas

- Public NewsNow instances may be Cloudflare-protected. A page URL may work while API calls fail. Try a different feed if news refresh sees most failures.
- Do not use `killall node`; it can terminate unrelated user processes. Find the exact PID:
  ```bash
  lsof -nP -iTCP:<port> -sTCP:LISTEN
  kill <pid>
  ```
- Keep `src-tauri/.cargo/config.toml`; it moves Cargo target output outside `src-tauri` to avoid Tauri watcher rebuild loops.
- Tauri needs a valid PNG icon with correct RGBA dimensions.
- **Provider misconfig** is the #1 source of "agent does not respond". Check Settings → AI 配置 and logs under the app data dir.
- **Cache hit verification**: with prompt caching working, the second chat turn should show non-zero `cache_read_tokens` in `agent_runs`.
- **Stuck loops**: `FailureCounter` aggregates consecutive failures per loop and emits `loop-degraded` status at 5/10/20 milestones.

## Architecture Boundary (don't violate)

Backend owns all writes and all AI invocation. Frontend is a render shell that:

1. Listens for Tauri events (`agent-status`, `agent-event`, `positions-changed`, `news-refreshed`, `quotes-refreshed`, `chat-message-appended`)
2. Refetches via read-only / intent-level Tauri commands exposed from `src-tauri/src/adapters/*_commands.rs`
3. Dispatches user intent via commands such as `send_chat_message_now`, `run_news_refresh`, `run_market_quote_refresh_cmd`, `reset_simulation_account`, `add_watchlist_code`, `remove_watchlist_code`, `fetch_article_content`, `get_agent_config`, `set_agent_config`

Patterns that should NOT be reintroduced:

- Frontend `useEffect` that writes business data to SQLite
- Frontend `setInterval` / `setTimeout` for scheduling — use `src-tauri/src/pipeline/scheduler.rs` Tokio loops
- Frontend constructing or parsing AI prompts
- `localStorage` for any persistence
- Direct Tauri commands for raw table writes (`replace_*`, `append_chat_message`, etc.)
- Adding ad-hoc HTTP calls to model APIs in pipeline code — go through `infrastructure::agent::provider::ChatProvider`
- Adding new schedulers in React — extend `pipeline/scheduler.rs`

## Adding a Provider

To add a new wire format (e.g. Gemini, Bedrock Converse):

1. New file under `src-tauri/src/infrastructure/agent/provider/` implementing `ChatProvider`. Translate canonical `Block` to your wire format on serialize, your event stream to `ProviderEvent` on receive.
2. Extend `domain::agent::ProviderKind` if the provider is a new kind.
3. Extend `pipeline::agent::config` and `build_provider` dispatch.
4. Update SettingsPage with a new tab in the provider switcher.
5. Update `docs/provider-design.md` mapping table.

Do not shoehorn into an existing wire format. Keep one provider per wire format; OpenAI-compatible vendors reuse `OpenAIChatCompletionsProvider` only when they implement that wire format.

## Adding a Tool

1. Add a struct in `src-tauri/src/adapters/agent_tools/<category>.rs` implementing the `Tool` trait from `crate::pipeline::agent::tools` (the trait + `ToolContext` + `ToolRegistry` live in the pipeline layer; concrete tools live in adapters as protocol adapters).
2. Register it in `build_chat_registry` (`src-tauri/src/adapters/agent_tools/mod.rs`).
3. Update prompt copy in `src-tauri/src/pipeline/agent/identity.md` if the tool changes how the agent should think about its job.

The tool's `execute()` is wrapped in a `tool_timeout_secs` timeout. Return `(content, is_error: true)` for soft failures so the agent can self-correct on the next turn instead of crashing the run.

## Handoff Validation

Before saying a code change is complete:

```bash
npm run build
cargo check --manifest-path src-tauri/Cargo.toml
cargo test  --manifest-path src-tauri/Cargo.toml
```

For NewsNow changes, test `/api/latest` and at least one `/api/s?id=...` endpoint.
For quote changes, test TDX / Eastmoney fallback paths with index + stock codes.
For provider changes:
- Set base_url + token in Settings → AI 配置, send a chat message, verify `cache_read_tokens > 0` in `agent_runs` after the second turn
- For OpenAI Responses: verify the `input` array preserves canonical Block order (`Text`, `ToolUse`, `Text` → `message`, `function_call`, `message`)
- For retry: temporarily inject `ProviderError::Transient` and confirm at least one retry log line at level `WARN` with `attempt=N`

For pipeline changes that touch account DB writes, prefer extending `pipeline/account/service.rs` so Account aggregate event generation and PositionRepo persistence stay in one place.
