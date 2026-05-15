import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useMemo, useState } from "react";
import { safeUnlisten } from "../lib/tauriEvents";
import type { MarketQuote } from "../types";

type SnapshotMap = Map<string, MarketQuote>;

export function useMarketQuotesFor(tsCodes: string[]) {
  const [quoteMap, setQuoteMap] = useState<SnapshotMap>(new Map());
  const key = useMemo(() => Array.from(new Set(tsCodes)).sort().join("|"), [tsCodes]);

  const hydrate = useCallback(async () => {
    const unique = key ? key.split("|") : [];
    if (unique.length === 0) {
      setQuoteMap(new Map());
      return;
    }
    try {
      const rows = await invoke<MarketQuote[]>("snapshot_market_quotes_for", {
        tsCodes: unique,
      });
      setQuoteMap(new Map(rows.map((r) => [r.tsCode, r])));
    } catch (err) {
      console.warn("snapshot_market_quotes_for 失败:", err);
    }
  }, [key]);

  useEffect(() => {
    void hydrate();
  }, [hydrate]);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    listen("market-quotes-refreshed", () => {
      if (!cancelled) void hydrate();
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
  }, [hydrate]);

  return { quoteMap };
}
