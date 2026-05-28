// marketListCache — 模块级 cache，避免切 tab 后回到市场页重新 IPC 拉 7500 标的。
//
// 数据来源：listMarket 完全是本地 DB + MARKET_SNAPSHOT 读，不调 TDX。背景
// scheduler 自己 15s/60s tick 维护 snapshot 新鲜度；前端 cache 只是省 IPC。
//
// 策略：stale-while-revalidate。
//   - 永久保留 cache（直到 app 关闭或显式 invalidate）
//   - getCachedList 返回 { items, ageMs, stale } 三态
//   - stale = ageMs > REVALIDATE_AFTER_MS（30s）→ caller 显示 cache + 后台 refetch
//   - 不 stale → caller 直接用，不发 IPC
//
// 配合 MarketPage 实现"切 tab 瞬开 + 后台静默更新"。

import type { ListMarketItem } from "../bindings";

interface Entry {
  items: ListMarketItem[];
  updatedAt: number;
}

/** 超过这个值视为 stale，触发后台 revalidate（但仍展示旧 cache）。 */
const REVALIDATE_AFTER_MS = 30_000;

const cache = new Map<string, Entry>();

function key(category: string, query: string): string {
  return `${category}|${query}`;
}

export interface CacheHit {
  items: ListMarketItem[];
  ageMs: number;
  /** 是否需要 caller 后台 refetch（true = 超过 REVALIDATE_AFTER_MS） */
  stale: boolean;
}

/** 永远不会返回 null（除非 key 不存在）；过期不算 null，让 caller 决定怎么处理。 */
export function getCachedList(
  category: string,
  query: string,
): CacheHit | null {
  const e = cache.get(key(category, query));
  if (!e) return null;
  const ageMs = Date.now() - e.updatedAt;
  return { items: e.items, ageMs, stale: ageMs > REVALIDATE_AFTER_MS };
}

export function setCachedList(
  category: string,
  query: string,
  items: ListMarketItem[],
): void {
  cache.set(key(category, query), { items, updatedAt: Date.now() });
}

/** 强制刷新（用户点刷新按钮）调用，清掉所有缓存。 */
export function invalidateAllListCache(): void {
  cache.clear();
}
