import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import { safeUnlisten } from "../lib/tauriEvents";
import type { DomainAccountSnapshot } from "../types";

/**
 * ACCOUNT_SNAPSHOT in-memory 缓存的前端镜像。
 *
 * 数据流：
 * 1. mount 时 invoke `get_account_snapshot` 拉一次
 * 2. listen `account-snapshot-updated` event —— 后端每次写或定时刷新都会 emit
 * 3. 收到 event 后 re-invoke 拉最新
 *
 * 后端写触发（自动）：
 * - AccountService 任何写操作完成（立即）
 * - quotes refresh 完成（MARKET_SNAPSHOT 更新后立即重估）
 * - 兜底定时 10s 盘中 / 60s 盘外
 *
 * 启动初期 1-2 秒内可能返 null（cache 还没填充），前端显示 loading。
 */
export function useAccountSnapshot() {
  const [snapshot, setSnapshot] = useState<DomainAccountSnapshot | null>(null);

  const refresh = useCallback(async () => {
    try {
      const snap = await invoke<DomainAccountSnapshot | null>("get_account_snapshot");
      setSnapshot(snap);
    } catch (err) {
      console.warn("get_account_snapshot 失败:", err);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    listen("account-snapshot-updated", () => {
      if (!cancelled) void refresh();
    })
      .then((h) => {
        if (cancelled) safeUnlisten(h);
        else unlisten = h;
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
      safeUnlisten(unlisten);
    };
  }, [refresh]);

  return { snapshot, refresh };
}
