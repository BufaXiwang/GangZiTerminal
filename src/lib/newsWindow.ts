// newsWindow — 双向 keyset 窗口流的核心状态机（纯函数）。
//
// Spec: news-module.md §4「双向 keyset 读取」(line 281-287)
//       frontend-design.md §4 资讯页（双向窗口流 / 窗口有界）
//
// 资讯列表是「全局倒序时间线」的一段连续窗口（按 publishedAt desc）。
// 这里把 NewsPage 里内联在 setItems 回调中的合并 / 去重 / 裁剪 / 游标 / hasMore
// 判定提炼成纯函数，便于单测覆盖。NewsPage 的三个方向 handler 组合调用这些函数，
// 行为与内联版本等价：
//   - 向更早（下滑）：mergeOlder + clampWindow(side:"head")
//   - 向更新（上滑）：mergeNewer + clampWindow(side:"tail")
//   - 锚定到某天 / 初始加载：replaceWindow
//   - live 刷新（最新端 prepend）：mergeNewer 的去重 prepend + clampWindow(side:"tail")
//
// 去重一律按 id：闭区间游标会带回边界条（同一 publishedAt 多条 / 边界时刻那条），
// 必须按 id Set 去重避免重复。

/** 窗口流需要的最小元素契约：稳定 id + 可空发布时间。 */
export interface WindowItem {
  id: string;
  publishedAt?: string | null;
}

/**
 * 向更早扩展：把新一页 append 到尾部，按 id 去重（丢弃已存在的边界条）。
 * 返回新数组（不修改入参）。
 */
export function mergeOlder<T extends WindowItem>(prev: T[], batch: T[]): T[] {
  const seen = new Set(prev.map((x) => x.id));
  return [...prev, ...batch.filter((x) => !seen.has(x.id))];
}

/**
 * 向更新扩展：后端按 order:"asc" 返回升序批，reverse 成倒序后 prepend 到头部，
 * 按 id 去重（丢弃已存在的边界条）。返回新数组（不修改入参）。
 * live 刷新（已是倒序的 fresh 批）可先自行 reverse 或直接传升序——本函数只负责 reverse+prepend+dedupe。
 */
export function mergeNewer<T extends WindowItem>(prev: T[], ascBatch: T[]): T[] {
  const desc = [...ascBatch].reverse(); // asc → desc，准备 prepend
  const seen = new Set(prev.map((x) => x.id));
  const fresh = desc.filter((x) => !seen.has(x.id));
  return [...fresh, ...prev];
}

/** clampWindow 裁剪端：'head' = 最新端（数组头）；'tail' = 最早端（数组尾）。 */
export type ClampSide = "head" | "tail";

/**
 * 窗口有界：items 超过 max 时，从指定端裁掉多出的条，保持连续倒序窗口。
 * side:"head" 裁数组头部（最新端），side:"tail" 裁数组尾部（最早端）。
 * 返回 { items, trimmed }：trimmed=true 表示发生了裁剪 → 调用方应把被裁端 hasMore 置回 true。
 * 未超限时返回原数组（同一引用）+ trimmed:false。
 */
export function clampWindow<T>(
  items: T[],
  max: number,
  side: ClampSide,
): { items: T[]; trimmed: boolean } {
  if (items.length <= max) return { items, trimmed: false };
  const clamped =
    side === "head"
      ? items.slice(items.length - max) // 裁头部，保留尾部 max 条
      : items.slice(0, max); // 裁尾部，保留头部 max 条
  return { items: clamped, trimmed: true };
}

/** 锚定替换：跳日期 / 初始加载 / 筛选变化时整体替换窗口为新批。 */
export function replaceWindow<T extends WindowItem>(batch: T[]): T[] {
  return batch;
}

/**
 * 最老游标：窗口最末一条的 publishedAt（向更早扩展用 publishedTo）。
 * 空窗口 / 末条缺 publishedAt 返回 null（无法做游标，停止下扩）。
 */
export function oldestCursor<T extends WindowItem>(items: T[]): string | null {
  const last = items[items.length - 1];
  return last?.publishedAt ?? null;
}

/**
 * 最新游标：窗口首条的 publishedAt（向更新扩展用 publishedFrom）。
 * 空窗口 / 首条缺 publishedAt 返回 null。
 */
export function newestCursor<T extends WindowItem>(items: T[]): string | null {
  const first = items[0];
  return first?.publishedAt ?? null;
}

/**
 * hasMore 判定：本次请求返回的批长 == pageSize → 该方向可能还有，置 true。
 * 不满页 → 已到端，置 false。
 */
export function hasMoreFromBatch(batchLength: number, pageSize: number): boolean {
  return batchLength >= pageSize;
}
