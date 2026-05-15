import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import { fallbackMarketOverview } from "../lib/market";
import { safeUnlisten } from "../lib/tauriEvents";
import type { MarketOverview, StockQuote } from "../types";

/**
 * 行情 / 大盘的前端状态容器。
 *
 * **数据来源**（重构后）：
 * - `quotes` 数组：旧兼容字段，当前返回空数组。
 * - `marketOverview`：通过 IPC `get_market_overview` 拉一次 + 监听 event 同步刷新。
 *
 * 实时报价由 Account subscriptions + core indexes 驱动，后端维护 MARKET_SNAPSHOT。
 */

export function useQuotes() {
  const [marketOverview, setMarketOverview] = useState<MarketOverview>(fallbackMarketOverview);
  const [isRefreshingQuotes, setIsRefreshingQuotes] = useState(false);

  const refreshMarketOverview = useCallback(async () => {
    setIsRefreshingQuotes(true);
    try {
      const overview = await invoke<MarketOverview>("get_market_overview");
      setMarketOverview(overview);
    } catch (err) {
      console.warn("get_market_overview 失败:", err);
    } finally {
      setIsRefreshingQuotes(false);
    }
  }, []);

  // 首次加载 + 监听实时行情刷新事件（完成后顺手刷一次 overview）
  useEffect(() => {
    void refreshMarketOverview();
  }, [refreshMarketOverview]);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    listen("market-quotes-refreshed", () => {
      if (cancelled) return;
      void refreshMarketOverview();
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
  }, [refreshMarketOverview]);

  // 兼容字段——quotes 已废弃，保留空数组让旧 caller 不破坏
  const quotes: StockQuote[] = [];

  return {
    quotes,
    marketOverview,
    isRefreshingQuotes,
    refreshQuotes: refreshMarketOverview,
    refreshMarketOverview,
  };
}
