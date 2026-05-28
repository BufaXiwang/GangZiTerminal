// useIndustryHeatmap — 行业热度 top N。
//
// Spec: docs/design/quotes-module.md §4 industry_heatmap
//
// - polling：默认 60s（行业聚合变动比 quote 慢，可拉长间隔）。

import { useEffect, useRef, useState } from "react";
import { commands, type IndustryHeatmap } from "../bindings";

// Module-level cache：remount 时立即用上一份，无 loading 闪烁。
let cachedData: IndustryHeatmap | null = null;
let cachedAt = 0;

export interface UseIndustryHeatmapState {
  data: IndustryHeatmap | null;
  loading: boolean;
  error: string | null;
  lastUpdatedMs: number | null;
}

interface UseIndustryHeatmapOptions {
  enabled?: boolean;
  intervalMs?: number;
  topN?: number;
}

export function useIndustryHeatmap(
  opts: UseIndustryHeatmapOptions = {},
): UseIndustryHeatmapState {
  const { enabled = true, intervalMs = 60_000, topN = 5 } = opts;
  const [state, setState] = useState<UseIndustryHeatmapState>(() => ({
    data: cachedData,
    loading: cachedData === null,
    error: null,
    lastUpdatedMs: cachedAt || null,
  }));
  const reqIdRef = useRef(0);

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    const fetchOnce = (silent: boolean) => {
      const id = ++reqIdRef.current;
      if (!silent) setState((s) => ({ ...s, loading: cachedData === null }));
      void commands.fetchIndustryHeatmap(topN).then((res) => {
        if (cancelled || id !== reqIdRef.current) return;
        if (res.status === "error") {
          if (!silent) {
            setState((s) => ({
              ...s,
              loading: false,
              error: `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
            }));
          }
          return;
        }
        cachedData = res.data;
        cachedAt = Date.now();
        setState({
          data: res.data,
          loading: false,
          error: null,
          lastUpdatedMs: cachedAt,
        });
      });
    };
    const stale = cachedData === null || Date.now() - cachedAt > 60_000;
    fetchOnce(!stale);
    const timer = window.setInterval(() => fetchOnce(true), intervalMs);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [enabled, intervalMs, topN]);

  return state;
}
