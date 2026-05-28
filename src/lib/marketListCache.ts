// marketListCache — 模块级 cache，避免切 tab 后回到市场页重新 IPC 拉 7500 标的。
//
// 缓存键：`${category}|${query}`。TTL 60s 内同样 key 直接命中。
// 后台 polling（实时 quote、市场宽度等）独立运行；此 cache 只是减少 listMarket
// IPC 调用。

import type { ListMarketItem } from "../bindings";

interface Entry {
  items: ListMarketItem[];
  updatedAt: number;
}

const TTL_MS = 60_000;
const cache = new Map<string, Entry>();

function key(category: string, query: string): string {
  return `${category}|${query}`;
}

export function getCachedList(
  category: string,
  query: string,
): { items: ListMarketItem[]; ageMs: number } | null {
  const e = cache.get(key(category, query));
  if (!e) return null;
  const ageMs = Date.now() - e.updatedAt;
  if (ageMs > TTL_MS) return null;
  return { items: e.items, ageMs };
}

export function setCachedList(
  category: string,
  query: string,
  items: ListMarketItem[],
): void {
  cache.set(key(category, query), { items, updatedAt: Date.now() });
}

/** 强制刷新（用户点刷新按钮 / 行情更新事件）调用，清掉所有缓存。 */
export function invalidateAllListCache(): void {
  cache.clear();
}
