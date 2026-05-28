// useMarketBreadth — 市场宽度（涨/跌/平 + 涨停/跌停）。
//
// Spec: docs/design/quotes-module.md §4 market_breadth
//
// - polling：默认 30s。
// - 后端纯只读聚合，不触发 provider；调用便宜。

import { useEffect, useRef, useState } from "react";
import { commands, type MarketBreadth } from "../bindings";

export interface UseMarketBreadthState {
  data: MarketBreadth | null;
  loading: boolean;
  error: string | null;
  lastUpdatedMs: number | null;
}

interface UseMarketBreadthOptions {
  enabled?: boolean;
  intervalMs?: number;
}

export function useMarketBreadth(
  opts: UseMarketBreadthOptions = {},
): UseMarketBreadthState {
  const { enabled = true, intervalMs = 30_000 } = opts;
  const [state, setState] = useState<UseMarketBreadthState>({
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
      void commands.fetchMarketBreadth().then((res) => {
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
  }, [enabled, intervalMs]);

  return state;
}
