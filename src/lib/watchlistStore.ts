// watchlistStore — 跨页面共享自选标的集合（zustand）。
//
// Spec: docs/design/account-module.md §2 自选模型 / §4 fetch_account.include.watchlist
//        + update_watchlist (add / remove / update_note)
//
// 设计原则（与 AGENTS.md 一致）：
// - 前端不持有业务真源；后端 Account 才是真源。
// - 这个 store 只是 fetch_account 自选段的本地缓存 + UI 同步桥（市场页星标 ↔ 模拟账户自选）。
// - 任何 add / remove 都先 optimistic 更新本地集合，再调后端 update_watchlist；
//   后端失败回滚。
// - 不缓存行情字段（quote）；行情留给页面拉 fetch_account 时刷新。

import { create } from "zustand";
import { commands, type TsCode, type WatchlistItemView } from "../bindings";

interface WatchlistState {
  /** 当前自选 ts_code 集合，便于 O(1) 判断。 */
  codes: Set<TsCode>;
  /** 最近一次 fetch_account 拿到的完整 watchlist 视图（含 quote），由订阅页面消费。 */
  items: WatchlistItemView[];
  loaded: boolean;
  loading: boolean;
  error: string | null;

  /** 拉一次 fetch_account 初始化（一般 App mount 时调一次；其他页面手动刷新可再调）。 */
  load: () => Promise<void>;
  /** 直接以新视图覆盖（页面已经调 fetch_account 时复用结果，避免重复请求）。 */
  setItems: (items: WatchlistItemView[]) => void;
  /** 添加自选；optimistic + 后端失败回滚。返回是否成功。 */
  add: (tsCode: TsCode, note?: string) => Promise<boolean>;
  /** 删除自选；optimistic + 后端失败回滚。返回是否成功。 */
  remove: (tsCode: TsCode) => Promise<boolean>;
}

function itemsToSet(items: WatchlistItemView[]): Set<TsCode> {
  return new Set(items.map((it) => it.tsCode));
}

export const useWatchlistStore = create<WatchlistState>((set, get) => ({
  codes: new Set<TsCode>(),
  items: [],
  loaded: false,
  loading: false,
  error: null,

  load: async () => {
    if (get().loading) return;
    set({ loading: true, error: null });
    const res = await commands.fetchAccount({
      include: { watchlist: true },
    });
    if (res.status === "error") {
      set({
        loading: false,
        error: `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
      });
      return;
    }
    const items = res.data.watchlist ?? [];
    set({
      items,
      codes: itemsToSet(items),
      loaded: true,
      loading: false,
      error: null,
    });
  },

  setItems: (items) =>
    set({
      items,
      codes: itemsToSet(items),
      loaded: true,
    }),

  add: async (tsCode, note) => {
    const prev = get().codes;
    if (prev.has(tsCode)) return true;
    // optimistic 本地添加（items 列表暂不加占位项，由调用方下次 fetch_account 刷新行情视图）
    const next = new Set(prev);
    next.add(tsCode);
    set({ codes: next });
    const res = await commands.updateWatchlist(
      note
        ? { action: "add", ts_code: tsCode, note }
        : { action: "add", ts_code: tsCode },
    );
    if (res.status === "error" || !res.data.accepted) {
      // rollback
      const rollback = new Set(get().codes);
      rollback.delete(tsCode);
      set({ codes: rollback });
      // eslint-disable-next-line no-console
      console.error(
        "watchlist add failed:",
        res.status === "error" ? res.error : res.data,
      );
      return false;
    }
    return true;
  },

  remove: async (tsCode) => {
    const prev = get().codes;
    if (!prev.has(tsCode)) return true;
    const next = new Set(prev);
    next.delete(tsCode);
    // 同步移除 items（让 UI 立即响应）
    const nextItems = get().items.filter((it) => it.tsCode !== tsCode);
    set({ codes: next, items: nextItems });
    const res = await commands.updateWatchlist({ action: "remove", ts_code: tsCode });
    if (res.status === "error" || !res.data.accepted) {
      // rollback
      const rollback = new Set(get().codes);
      rollback.add(tsCode);
      set({ codes: rollback, items: get().items });
      // eslint-disable-next-line no-console
      console.error(
        "watchlist remove failed:",
        res.status === "error" ? res.error : res.data,
      );
      return false;
    }
    return true;
  },
}));
