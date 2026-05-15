import { listen } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import { safeUnlisten } from "../lib/tauriEvents";
import type { QuotesFetchStatus } from "../types";

/**
 * 监听后端 `quotes-fetch-status` 事件——只在行情拉取出问题时收到。
 *
 * 成功路径后端不发事件，所以"没问题"的稳态就是 `latest = null`。下一次失败再到
 * 时覆盖；用户主动 dismiss 也清成 null。**不做自动过期**——失败信息本身就是排
 * 障线索，留在界面上比悄悄消失更有用，由下一次 invoke 的结果自然推进。
 */
export function useQuotesFetchStatus() {
  const [latest, setLatest] = useState<QuotesFetchStatus | null>(null);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    listen<QuotesFetchStatus>("quotes-fetch-status", (event) => {
      setLatest(event.payload);
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
  }, []);

  const dismiss = useCallback(() => setLatest(null), []);

  return { latest, dismiss };
}
