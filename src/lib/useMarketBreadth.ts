// useMarketBreadth — 市场宽度（涨/跌/平 + 涨停/跌停）。
//
// Spec: docs/design/quotes-module.md §4 market_breadth
//
// - polling：默认 30s。
// - 后端纯只读聚合，不触发 provider；调用便宜。

import { useEffect, useRef, useState } from "react";
import { commands, type MarketBreadth } from "../bindings";

// Module-level cache：remount 时立即用上一份，无 loading 闪烁。
let cachedData: MarketBreadth | null = null;
let cachedAt = 0;

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
  const [state, setState] = useState<UseMarketBreadthState>(() => ({
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
      void commands.fetchMarketBreadth().then((res) => {
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
    const stale = cachedData === null || Date.now() - cachedAt > 30_000;
    fetchOnce(!stale);
    const timer = window.setInterval(() => fetchOnce(true), intervalMs);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [enabled, intervalMs]);

  return state;
}
