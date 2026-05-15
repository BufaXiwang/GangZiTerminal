import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import { safeUnlisten } from "../lib/tauriEvents";
import type { MarketInstrument } from "../types";

/**
 * 全市场静态档案——股票 + 指数 + 基金。
 *
 * 数据来自后端 stocks/indexes/funds 三表，scheduler 启动 + 每日 08:30 自动刷新。
 *
 * 数据流：
 * 1. 挂载时 invoke 一次（可能空，scheduler 还没拉完）
 * 2. listen `market-instruments-refreshed` 事件 → 自动 re-invoke
 * 3. 这样冷启动 stocks 表空 → scheduler 拉完 emit event → 前端立即拿到
 */
export function useMarketInstruments() {
  const [instruments, setInstruments] = useState<MarketInstrument[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const fetchOnce = useCallback(async () => {
    try {
      const rows = await invoke<MarketInstrument[]>("list_market_instruments");
      setInstruments(rows);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  // 初次加载
  useEffect(() => {
    void fetchOnce();
  }, [fetchOnce]);

  // 后端档案表刷新完成后自动 re-fetch
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    listen("market-instruments-refreshed", () => {
      if (cancelled) return;
      void fetchOnce();
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
  }, [fetchOnce]);

  return { instruments, loading, error };
}
