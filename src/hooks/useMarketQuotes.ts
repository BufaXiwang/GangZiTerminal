import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import { safeUnlisten } from "../lib/tauriEvents";
import type { MarketQuote } from "../types";

/**
 * 实时行情快照——后端由 Account subscriptions + core indexes 刷新 MARKET_SNAPSHOT。
 *
 * 数据流：
 * 1. 冷启动 invoke `snapshot_market_quotes` 拿当前 in-memory snapshot（可能为空）
 * 2. listen `market-quotes-refreshed` event——只是"该刷了"通知，**不带 payload**
 *    收到后再 invoke `snapshot_market_quotes` 拿最新全量
 * 3. 页面不手动刷新；行情由后端 scheduler 的 active set / universe loop 写快照并广播。
 *
 * 为什么 event 不带 quote payload：单次 7000 条 quote 序列化 ~1.5MB，
 * emit 频率高时前端阻塞；改成"通知 + 拉"模型——后端写完 in-memory 后通知前端，
 * 前端按需 invoke 拿快照（仍是同步内存读，毫秒级返回）。
 */
type SnapshotMap = Map<string, MarketQuote>; // key = tsCode

export function useMarketQuotes() {
  const [quoteMap, setQuoteMap] = useState<SnapshotMap>(new Map());
  const [lastRefreshed, setLastRefreshed] = useState<string | null>(null);

  const hydrate = useCallback(async () => {
    try {
      const rows = await invoke<MarketQuote[]>("snapshot_market_quotes");
      setQuoteMap(new Map(rows.map((r) => [r.tsCode, r])));
    } catch (e) {
      console.warn("snapshot_market_quotes 失败:", e);
    }
  }, []);

  // 冷启动 hydrate
  useEffect(() => {
    void hydrate();
  }, [hydrate]);

  // listen event → 重新 hydrate
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    listen<{ capturedAt: string; total: number; success: number }>(
      "market-quotes-refreshed",
      (event) => {
        if (cancelled) return;
        setLastRefreshed(event.payload?.capturedAt ?? new Date().toISOString());
        void hydrate();
      },
    )
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

  return { quoteMap, lastRefreshed };
}
