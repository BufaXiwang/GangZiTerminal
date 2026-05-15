# Development

## Commands

Run from the project root:

```bash
npm install
npm run build                                         # frontend build
cargo check --manifest-path src-tauri/Cargo.toml      # rust build
cargo test  --manifest-path src-tauri/Cargo.toml      # rust unit tests (currently 119 across agent/loop/provider/context/observer + memory/trade/prompt/security)
npm run tauri -- dev                                  # one-shot dev (foreground)
```

## Tmux Runtime

Standard local dev uses the fixed `gangzi-terminal` tmux session — the tauri dev process keeps Vite + Rust watcher running together:

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

All schema lives in `src-tauri/src/db.rs::migrate`. Tables and their writers:

| Table | Owner / writer |
|---|---|
| `app_state` (KV) | `useAppState` (frontend) for UI settings; pipelines for memory + last_briefing_at; `agent::config` for provider config |
| `chat_messages` | `db::append_chat_message` (chat path), `commit_briefing` / `commit_review` (transactional pipelines) |
| `news_items` | News refresh (write) + briefing claim/consume/revert |
| `analysis_records` | `commit_briefing` (insert new) + `commit_review` (update review field) |
| `simulated_positions` | `commit_briefing` (open) + `commit_review` (close-on-invalidated) + `close_positions` (auto-close from quote refresh) + reset |
| `position_events` | All pipelines that touch positions; written via `commit_briefing` / `commit_review` (transactional) or `db::append_position_event` (close_positions / reset) |
| `article_contents` | `article::fetch_article_content` (cache write) |
| `agent_runs` | `observer::start_run` + `observer::finalize` — per-run token totals / turns / stop_reason / error |
| `agent_run_turns` | `observer::TurnAccumulator` — per-turn token deltas + tool counts; lets ops grep "which turn went wrong" |

The `agent_tasks` table schema is preserved for backward compatibility but no runtime path writes to it.

`app_state` keys currently in use:

- `gangzi-terminal.watchlist` — written by backend `add_watchlist_code` / `remove_watchlist_code`
- `gangzi-terminal.auto-refresh` / `.refresh-interval` / `.auto-agent` / `.buffer-size` / `.briefing-interval` / `.active-view` — written by frontend `useAppState` (UI settings)
- `gangzi-terminal.investor-memory` — written by briefing pipeline (via `commit_briefing` app_state_writes); chat pipeline uses `update_memory` / `remove_memory` tools that write directly mid-loop
- `gangzi-terminal.last-briefing-at` — written by briefing pipeline (in `commit_briefing` and the failure path)
- `agent.config` — written by `set_agent_config` Tauri command (Settings → AI 配置)

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
    "baseUrl": "https://api.openai.com",  // or any OpenAI-compatible relay
    "token": "sk-xxx",
    "models": { "chat": "gpt-5.5-instant", "briefing": "gpt-5.5-thinking", "review": "gpt-5.5-thinking", "compact": "gpt-5.5-nano" },
    "reasoningEffort": "low" | "medium" | "high" | null,
    "enableWebSearch": false  // Responses-only built-in
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

The `provider` field selects which wire format `build_provider` constructs. All three formats share the same `agent::types::AgentRequest` IR — the wire serializer is the only thing that changes. See [provider-design.md](provider-design.md) for the canonical Block ↔ wire format mapping.

`build_provider` always wraps the concrete provider in `RetryingProvider` (exponential backoff + jitter, max 5 attempts on `RateLimited`/`Transient`/5xx).

Token storage: tokens are stored plaintext in SQLite — the threat model is "don't accidentally git commit", not "defend against an attacker with disk access". The Settings page displays `prefix…(N chars)` masked previews; submitting the masked string preserves the existing token.

## Known Gotchas

- Public NewsNow instances may be Cloudflare-protected. A page URL may work while API calls fail. Try a different feed if news refresh sees most failures.
- Do not use `killall node`; it can terminate unrelated user processes. Find the exact PID:
  ```bash
  lsof -nP -iTCP:<port> -sTCP:LISTEN
  kill <pid>
  ```
- Keep `src-tauri/.cargo/config.toml`; it moves Cargo target output outside `src-tauri` to avoid Tauri watcher rebuild loops.
- Tauri needs a valid PNG icon with correct RGBA dimensions.
- **Provider misconfig** is the #1 source of "schedulers silently sit idle". Briefing scan emits `agent-status: missing-config` when `cfg.ensure_ready()` fails — check the status bar AND `~/Library/Application Support/com.local.gangzi-terminal/logs/gangzi-terminal.*.log` for `provider stream failed`.
- **Cache hit verification**: with prompt caching working, the second turn of a chat (or any subsequent run with shared system+tools prefix) should show non-zero `cache_read_tokens` in `agent_runs`. If it's always 0, double-check that `system` blocks have stable text and the Anthropic `cache_control` breakpoints are positioned correctly.
- **Stuck loops**: `FailureCounter` aggregates consecutive failures per loop and emits `loop-degraded` status at 5/10/20 milestones. If you see one of these, look at the `consecutive=N` value in `tracing::warn!` log lines for that loop name.

## Architecture Boundary (don't violate)

Backend owns all writes and all AI invocation. Frontend is a render shell that:

1. Listens for Tauri events (`agent-status`, `agent-event`, `briefing-published`, `review-published`, `positions-changed`, `news-refreshed`, `quotes-refreshed`, `chat-message-appended`)
2. Refetches via the read-only `list_*` / `count_*` / `load_app_state` Tauri commands
3. Renders, and dispatches user input via dedicated trigger commands (`run_briefing_now`, `run_review_now`, `send_chat_message_now`, `run_news_refresh`, `run_quote_refresh`, `reset_simulation_account`, `add_watchlist_code`, `remove_watchlist_code`, `fetch_article_content`, `get_agent_config`, `set_agent_config`)

Patterns that should NOT be reintroduced:

- ❌ Frontend `useEffect` that writes business data to SQLite
- ❌ Frontend `setInterval` / `setTimeout` for scheduling — use `src-tauri/src/scheduler.rs` Tokio loops
- ❌ Frontend constructing or parsing AI prompts
- ❌ `localStorage` for any persistence
- ❌ Direct Tauri commands for raw table writes (`replace_*`, `append_chat_message`, etc.) — those are Rust-internal helpers, deliberately not on the IPC surface
- ❌ Adding ad-hoc HTTP calls to model APIs in pipeline code — go through `agent::provider::ChatProvider`
- ❌ Adding new schedulers in React — extend `scheduler.rs`

## Adding a Provider

To add a new wire format (e.g. Gemini, Bedrock Converse):

1. New file `src-tauri/src/agent/provider/<name>.rs` implementing `ChatProvider`. Translate canonical `Block` to your wire format on serialize, your event stream to `ProviderEvent` on receive.
2. Extend `agent::config::ProviderKind` enum + add `<name>ProviderConfig` field on `AgentConfig`.
3. Extend `build_provider` dispatch.
4. Update SettingsPage with a new tab in the provider switcher.
5. Update `docs/provider-design.md` mapping table.

Don't shoehorn into an existing wire format. Keep one provider per wire format — DeepSeek/火山/vLLM all reuse `OpenAIChatCompletionsProvider` because they implement the OpenAI Chat Completions wire format.

## Adding a Tool

1. Add a struct in `src-tauri/src/agent/tools/<category>.rs` implementing `Tool`.
2. Register it in `build_chat_registry` (chat path) or `build_readonly_registry` (briefing/review).
3. Update prompt copy in `src-tauri/src/identity.md` if the tool changes how the agent should think about its job.

The tool's `execute()` will be wrapped in a `tool_timeout_secs` timeout automatically. Return `(content, is_error: true)` for soft failures so the agent can self-correct on the next turn instead of crashing the run.

## Handoff Validation

Before saying a code change is complete:

```bash
npm run build
cargo check --manifest-path src-tauri/Cargo.toml
cargo test  --manifest-path src-tauri/Cargo.toml
```

For NewsNow changes, test `/api/latest` and at least one `/api/s?id=...` endpoint.
For quote changes, test the Eastmoney push2 endpoint with index + stock codes.
For provider changes:
- Set base_url + token in Settings → AI 配置, send a chat message, verify `cache_read_tokens > 0` in `agent_runs` after the second turn
- For OpenAI Responses: verify the `input` array preserves canonical Block order ([Text, ToolUse, Text...] → [message, function_call, message])
- For retry: temporarily inject `ProviderError::Transient` and confirm at least one retry log line at level `WARN` with `attempt=N`

For pipeline changes that touch DB writes, prefer extending `commit_briefing` / `commit_review` over adding new `db::*` public functions — keeps the atomicity boundary one place.
