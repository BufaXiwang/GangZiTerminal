import { listen } from "@tauri-apps/api/event";
import { useEffect } from "react";
import { safeUnlisten } from "../lib/tauriEvents";
import type { AgentEvent, StreamingRunState } from "../types";

/**
 * 监听后端的 `agent-event` 事件流，把 in-progress 的 run 状态累积到本地。
 *
 * 后端 chat / briefing / review 三条 pipeline 用同一个事件名（agent::observer::AGENT_EVENT）
 * emit。前端按 run_id 区分归属，UI 渲染时按此 map 展示"正在进行中的 agent run"。
 *
 * `run_start` → 创建空状态；`text_delta` / `thinking` / `tool_*` 累加；
 * `done` / `error` 删除状态（最终消息会通过 chat-message-appended 进入 messages 列表）。
 */
export function useAgentEventStream({
  enabled,
  setStreamingRuns,
}: {
  enabled: boolean;
  setStreamingRuns: React.Dispatch<
    React.SetStateAction<Record<string, StreamingRunState>>
  >;
}) {
  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    let unlisten: (() => void) | null = null;

    listen<AgentEvent>("agent-event", (event) => {
      const ev = event.payload;
      setStreamingRuns((current) => applyEvent(current, ev));
    })
      .then((handler) => {
        if (cancelled) safeUnlisten(handler);
        else unlisten = handler;
      })
      .catch(() => undefined);

    return () => {
      cancelled = true;
      safeUnlisten(unlisten);
    };
  }, [enabled, setStreamingRuns]);
}

function applyEvent(
  state: Record<string, StreamingRunState>,
  ev: AgentEvent,
): Record<string, StreamingRunState> {
  switch (ev.type) {
    case "run_start":
      return {
        ...state,
        [ev.run_id]: {
          runId: ev.run_id,
          pipeline: ev.pipeline,
          model: ev.model,
          text: "",
          thinking: "",
          toolCalls: [],
        },
      };
    case "text_delta": {
      const cur = state[ev.run_id];
      if (!cur) return state;
      return { ...state, [ev.run_id]: { ...cur, text: cur.text + ev.delta } };
    }
    case "thinking": {
      const cur = state[ev.run_id];
      if (!cur) return state;
      return {
        ...state,
        [ev.run_id]: { ...cur, thinking: cur.thinking + ev.delta },
      };
    }
    case "tool_start": {
      const cur = state[ev.run_id];
      if (!cur) return state;
      return {
        ...state,
        [ev.run_id]: {
          ...cur,
          toolCalls: [
            ...cur.toolCalls,
            {
              id: ev.tool_use_id,
              name: ev.name,
              input: ev.input,
              serverSide: ev.server_side,
              status: "running",
            },
          ],
        },
      };
    }
    case "tool_end": {
      const cur = state[ev.run_id];
      if (!cur) return state;
      return {
        ...state,
        [ev.run_id]: {
          ...cur,
          toolCalls: cur.toolCalls.map((tc) =>
            tc.id === ev.tool_use_id
              ? {
                  ...tc,
                  status: ev.is_error ? "error" : "done",
                  durationMs: ev.duration_ms,
                }
              : tc,
          ),
        },
      };
    }
    case "done":
    case "error": {
      // 最终消息已经通过 chat-message-appended 落到 messages 列表
      // 这里清掉 in-progress 占位
      const next = { ...state };
      delete next[ev.run_id];
      return next;
    }
    default:
      // usage / compacted 暂不影响 streaming 状态，落表由后端 observer 写
      return state;
  }
}
