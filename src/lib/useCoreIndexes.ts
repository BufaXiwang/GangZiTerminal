// useCoreIndexes — 拉取顶部 4 个核心指数 quote，用于市场页顶部指标行。
//
// Spec: docs/design/quotes-module.md §2 core_indexes + §4 fetch_data
//
// - 标的：000001.SH 上证 / 399001.SZ 深证 / 399006.SZ 创业板 / 000300.SH 沪深300
// - polling：默认 30s 一次；调用方可关 enabled。
// - 失败时保留上一份数据，单字段缺失时由调用方渲染 "-"。

import { useEffect, useRef, useState } from "react";
import { commands, type FetchDataItem } from "../bindings";

// 模块级 cache：组件 remount 时立即喂上一份，避免 mount → IPC → loading 闪烁。
// React 切 tab 时该缓存让顶部指标卡瞬间出来，背景 polling 自然 refresh。
let cachedItems: FetchDataItem[] | null = null;
let cachedAt = 0;

export interface CoreIndexInfo {
  tsCode: string;
  label: string;
}

export const CORE_INDEXES: CoreIndexInfo[] = [
  { tsCode: "000001.SH", label: "上证指数" },
  { tsCode: "399001.SZ", label: "深证成指" },
  { tsCode: "399006.SZ", label: "创业板指" },
  { tsCode: "000300.SH", label: "沪深300" },
];

export interface UseCoreIndexesState {
  items: FetchDataItem[];
  loading: boolean;
  error: string | null;
  lastUpdatedMs: number | null;
}

interface UseCoreIndexesOptions {
  enabled?: boolean;
  /** polling 间隔（ms）。默认 30000。 */
  intervalMs?: number;
}

export function useCoreIndexes(
  opts: UseCoreIndexesOptions = {},
): UseCoreIndexesState {
  const { enabled = true, intervalMs = 30_000 } = opts;
  // 初始 state 直接从 module cache 来；切 tab remount 瞬时渲染。
  const [state, setState] = useState<UseCoreIndexesState>(() => ({
    items: cachedItems ?? [],
    loading: cachedItems === null,
    error: null,
    lastUpdatedMs: cachedAt || null,
  }));
  const reqIdRef = useRef(0);

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    const fetchOnce = (silent: boolean) => {
      const id = ++reqIdRef.current;
      if (!silent) setState((s) => ({ ...s, loading: cachedItems === null }));
      void commands
        .fetchData({
          tsCodes: CORE_INDEXES.map((c) => c.tsCode),
          include: { quote: true },
        })
        .then((res) => {
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
          cachedItems = res.data.items;
          cachedAt = Date.now();
          setState({
            items: res.data.items,
            loading: false,
            error: null,
            lastUpdatedMs: cachedAt,
          });
        });
    };
    // 有 cache 且 < 30s → 静默 refetch；否则正常 loading
    const stale = cachedItems === null || Date.now() - cachedAt > 30_000;
    fetchOnce(!stale);
    const timer = window.setInterval(() => fetchOnce(true), intervalMs);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [enabled, intervalMs]);

  return state;
}
