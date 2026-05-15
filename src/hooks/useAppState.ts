import { invoke } from "@tauri-apps/api/core";
import { useEffect, useRef, useState } from "react";

const WRITE_DEBOUNCE_MS = 200;

/**
 * State hook backed by SQLite app_state（唯一持久化存储）。
 *
 * 行为：
 * - 第一次 mount：从 app_state 异步取，没有则用 fallback；
 * - `loaded` 在 hydrate 完成后变 true（成功或失败都置）；
 * - 加载完成前不写回，避免把 fallback 覆盖真实值；
 * - 写入做 200ms 去抖，连续 setValue 只刷一次盘；
 * - unmount 时如果有 pending 写，立即 flush 一次（best-effort）。
 *
 * 不使用 localStorage——避免和 SQLite 双源漂移。
 */
export function useAppState<T>(
  key: string,
  fallback: T,
  /** 把 DB 里读到的旧值结构补齐到当前 schema；不传则原样返回。 */
  normalize?: (raw: T) => T,
): [T, (value: T | ((prev: T) => T)) => void, boolean] {
  const [value, setValueRaw] = useState<T>(fallback);
  const [loaded, setLoaded] = useState(false);
  const writeTimerRef = useRef<number | null>(null);
  const pendingValueRef = useRef<T>(fallback);

  // ---- hydrate ----
  useEffect(() => {
    let cancelled = false;
    invoke<T | null>("load_app_state", { key })
      .then((stored) => {
        if (cancelled) return;
        if (stored !== null && stored !== undefined) {
          setValueRaw(normalize ? normalize(stored) : stored);
        }
        setLoaded(true);
      })
      .catch(() => {
        if (!cancelled) setLoaded(true);
      });
    return () => {
      cancelled = true;
    };
    // 故意只依赖 key——normalize 不进依赖（多数情况下是稳定函数；如果不稳定，组件应自己 useCallback）
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  // ---- debounced write ----
  useEffect(() => {
    if (!loaded) return;
    pendingValueRef.current = value;
    if (writeTimerRef.current !== null) {
      window.clearTimeout(writeTimerRef.current);
    }
    writeTimerRef.current = window.setTimeout(() => {
      writeTimerRef.current = null;
      void invoke("save_app_state", { key, value: pendingValueRef.current }).catch(() => undefined);
    }, WRITE_DEBOUNCE_MS);
    return () => {
      // 不在这里 flush；下面那个 unmount 专用 effect 才负责 flush。
    };
  }, [key, value, loaded]);

  // ---- unmount: best-effort flush of any pending debounced write ----
  useEffect(() => {
    return () => {
      if (writeTimerRef.current !== null) {
        window.clearTimeout(writeTimerRef.current);
        writeTimerRef.current = null;
        void invoke("save_app_state", { key, value: pendingValueRef.current }).catch(() => undefined);
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return [value, setValueRaw, loaded];
}
