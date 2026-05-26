import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import { safeUnlisten } from "../lib/tauriEvents";

/**
 * Agent Runtime 聚合读模型（fetch_agent_state）。
 *
 * 监听 `agent-run-started` / `agent-run-finished`、`positions-changed`
 * 等事件，按需 refetch。第一阶段：仅按需手动 refresh 以及 mount 时拉一次。
 */
export type AgentRuntimeStateInclude = {
  runs?: boolean;
  episodes?: boolean;
  reviews?: boolean;
  strategies?: boolean;
};

export type AgentRunRow = {
  runId: string;
  profileId: string;
  triggerKind: string;
  triggerPayload?: unknown;
  provider: string;
  wireFormat: string;
  model: string;
  status: "queued" | "running" | "completed" | "failed" | "cancelled";
  startedAt?: string | null;
  endedAt?: string | null;
  error?: string | null;
  createdAt: string;
  updatedAt: string;
};

export type DecisionEpisodeRow = {
  episodeId: string;
  runId: string;
  triggerKind: string;
  symbols: string[];
  thesis: string;
  action: string;
  actionStatus: string;
  blockedReason?: string | null;
  confidence?: number | null;
  riskPlan?: unknown;
  strategyIds: string[];
  evidenceRefs: unknown[];
  createdAt: string;
};

export type DecisionReviewRow = {
  reviewId: string;
  episodeId: string;
  trigger: string;
  result?: unknown;
  conclusion: string;
  suggestedChange?: unknown;
  evidenceRefs: unknown[];
  warnings?: unknown;
  createdAt: string;
};

export type StrategyCardRow = {
  strategyId: string;
  version: number;
  name: string;
  description: string;
  status: "active" | "paused";
  config: unknown;
  createdAt: string;
  updatedAt: string;
};

export type AgentRuntimeState = {
  runs: AgentRunRow[];
  episodes: DecisionEpisodeRow[];
  reviews: DecisionReviewRow[];
  strategies: StrategyCardRow[];
};

const EMPTY: AgentRuntimeState = {
  runs: [],
  episodes: [],
  reviews: [],
  strategies: [],
};

export function useAgentRuntimeState(include?: AgentRuntimeStateInclude, limit = 50) {
  const [state, setState] = useState<AgentRuntimeState>(EMPTY);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const resp = await invoke<AgentRuntimeState>("fetch_agent_state", {
        request: {
          include: include ?? { runs: true, episodes: true, reviews: true, strategies: true },
          limit,
        },
      });
      setState(resp ?? EMPTY);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [include, limit]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    let cancelled = false;
    let h1: (() => void) | null = null;
    let h2: (() => void) | null = null;
    listen("agent-run-started", () => {
      if (!cancelled) void refresh();
    })
      .then((h) => {
        if (cancelled) safeUnlisten(h);
        else h1 = h;
      })
      .catch(() => undefined);
    listen("agent-run-finished", () => {
      if (!cancelled) void refresh();
    })
      .then((h) => {
        if (cancelled) safeUnlisten(h);
        else h2 = h;
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
      safeUnlisten(h1);
      safeUnlisten(h2);
    };
  }, [refresh]);

  return { state, loading, error, refresh };
}
