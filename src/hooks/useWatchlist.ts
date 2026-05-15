import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import { safeUnlisten } from "../lib/tauriEvents";
import type { WatchlistEntry } from "../types";

/**
 * 自选股管理（带 name/sector 元信息）。
 *
 * - `entries`：当前自选股列表（含 ts_code / code / name）
 * - `add(code)`：用户手动加自选；agent 通过 tool 走同一路径
 * - `remove(code)`：仅用户能调（agent 无 tool）
 * - `error`：上一次操作的错误信息
 *
 * 后端 IPC：
 * - `list_watchlist_with_info` 拉列表 + 元信息
 * - `add_watchlist_code` / `remove_watchlist_code` 增删（自动持久化到 KV）
 * - `watchlist-changed` event：其它入口变更后自动同步列表
 */
export function useWatchlist() {
  const [entries, setEntries] = useState<WatchlistEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const list = await invoke<WatchlistEntry[]>("list_watchlist_with_info");
      setEntries(list);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    listen("watchlist-changed", () => {
      if (!cancelled) void refresh();
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
  }, [refresh]);

  const add = useCallback(
    async (code: string) => {
      setBusy(true);
      setError(null);
      try {
        await invoke<string[]>("add_watchlist_code", { code });
        await refresh();
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        setBusy(false);
      }
    },
    [refresh],
  );

  const remove = useCallback(
    async (code: string) => {
      setBusy(true);
      setError(null);
      try {
        await invoke<string[]>("remove_watchlist_code", { code });
        await refresh();
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        setBusy(false);
      }
    },
    [refresh],
  );

  return { entries, error, busy, add, remove, refresh };
}
