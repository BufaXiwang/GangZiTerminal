import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useState } from "react";
import type { StrategyCardRow } from "./useAgentRuntimeState";

/**
 * 读 / 写 StrategyCard（spec agent-runtime §9）。
 *
 * - `cards`：当前已知策略卡列表
 * - `upsert`：显式调整或新增（model.suggestedChange 不会自动调用）
 */
export function useStrategyCards(statusFilter?: "active" | "paused") {
  const [cards, setCards] = useState<StrategyCardRow[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      const resp = await invoke<{ items: StrategyCardRow[] }>("fetch_strategy_cards", {
        request: statusFilter ? { status: statusFilter } : null,
      });
      setCards(resp?.items ?? []);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [statusFilter]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const upsert = useCallback(
    async (request: {
      strategyId?: string;
      baseVersion?: number;
      name: string;
      description: string;
      status: "active" | "paused";
      config: unknown;
      reason: string;
    }) => {
      const resp = await invoke<{
        accepted: boolean;
        strategyId: string;
        version: number;
        reason?: string;
      }>("upsert_strategy_card", { request });
      await refresh();
      return resp;
    },
    [refresh],
  );

  return { cards, loading, error, refresh, upsert };
}
