import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import { safeUnlisten } from "../lib/tauriEvents";
import type { NewsItem } from "../types";

/**
 * 资讯状态容器。
 *
 * 数据生命周期完全由后端 Tokio scheduler 驱动（news_refresh_loop）：
 * - 启动 2s 后首次拉取
 * - 之后按 refreshInterval 周期性拉取
 * - 每次拉完 emit `news-refreshed { fetchedCount, failedCount }`
 *
 * 前端只剩两件事：
 * 1. 监听事件后调 list_news_items 把 UI state 同步上
 * 2. 用户点"刷新资讯"时 invoke run_news_refresh
 */
export function useNewsRefresh() {
  const [items, setItems] = useState<NewsItem[]>([]);
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [lastUpdated, setLastUpdated] = useState<string | null>(null);

  const reloadFromDb = useCallback(async () => {
    const refreshed = await invoke<NewsItem[]>("list_news_items", { limit: 300 }).catch(
      () => [] as NewsItem[],
    );
    setItems(refreshed);
    setLastUpdated(new Date().toISOString());
  }, []);

  // 监听后端推送
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    listen("news-refreshed", () => {
      void reloadFromDb();
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
  }, [reloadFromDb]);

  // 手动刷新
  const refreshFeeds = useCallback(async () => {
    setIsRefreshing(true);
    try {
      await invoke("run_news_refresh");
      // 不需要在这里 reload——后端 emit news-refreshed 后上面的 listener 会接管
    } catch (err) {
      console.warn("run_news_refresh 失败:", err);
    } finally {
      setIsRefreshing(false);
    }
  }, []);

  return {
    items,
    setItems,
    isRefreshing,
    lastUpdated,
    refreshFeeds,
  };
}
