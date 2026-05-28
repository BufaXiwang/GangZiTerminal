// useIndustryHeatmap — 行业热度 top N。
//
// Spec: docs/design/quotes-module.md §4 industry_heatmap
//
// - polling：默认 60s（行业聚合变动比 quote 慢，可拉长间隔）。

import { useEffect, useRef, useState } from "react";
import { commands, type IndustryHeatmap } from "../bindings";

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
  const [state, setState] = useState<UseIndustryHeatmapState>({
    data: null,
    loading: false,
    error: null,
    lastUpdatedMs: null,
  });
  const reqIdRef = useRef(0);

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    const fetchOnce = () => {
      const id = ++reqIdRef.current;
      setState((s) => ({ ...s, loading: true }));
      void commands.fetchIndustryHeatmap(topN).then((res) => {
        if (cancelled || id !== reqIdRef.current) return;
        if (res.status === "error") {
          setState((s) => ({
            ...s,
            loading: false,
            error: `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
          }));
          return;
        }
        setState({
          data: res.data,
          loading: false,
          error: null,
          lastUpdatedMs: Date.now(),
        });
      });
    };
    fetchOnce();
    const timer = window.setInterval(fetchOnce, intervalMs);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [enabled, intervalMs, topN]);

  return state;
}
