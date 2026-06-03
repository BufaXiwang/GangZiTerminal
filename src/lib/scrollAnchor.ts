// scrollAnchor — 元素锚定补偿，根治"列表头部/远端发生增删 → 视口跳动"。
//
// Spec: docs/design/frontend-design.md §4「窗口有界」
//       docs/design/news-module.md §`fetch_news` 双向 keyset 读取「窗口有界（滑动窗口）」
//         — 「裁剪/prepend 都必须做滚动锚定（按当前视口顶部的真实行补偿 scrollTop）」
//
// 为什么用元素锚定而不是 scrollHeight 差值：
//   列表用 content-visibility:auto，屏外行的行高是浏览器**估计值**，scrollHeight 差值会带误差；
//   而「当前视口顶部第一条可见行」的真实 rect.top 是精确的——mutation 前后锚到同一条行，
//   按它的位移补偿 scrollTop，视口纹丝不动。
//
// 用法：
//   const anchor = captureTopAnchor(container, "[data-news-id]");  // mutation 前
//   ... setItems(...)（裁头部 / prepend）...
//   restoreTopAnchor(container, anchor, "[data-news-id]");          // DOM 更新后（useLayoutEffect）

/** 锚点：mutation 前视口顶部第一条可见行的标识 + 其相对视口顶的 rect.top。 */
export interface TopAnchor {
  /** 行元素的稳定 id（DOM attribute 取出的字符串）。 */
  id: string;
  /** mutation 前该行 getBoundingClientRect().top。 */
  top: number;
}

/**
 * 找当前视口顶部第一条**可见行**作为锚。
 * itemSelector 选出的元素必须带可读 id（默认走 `data-news-id`，或自定义 getId）。
 * 返回 null 表示无可锚行（空列表 / 容器缺失）。
 */
export function captureTopAnchor(
  container: HTMLElement | null,
  itemSelector: string,
  getId: (el: HTMLElement) => string | null = (el) => el.dataset.newsId ?? null,
): TopAnchor | null {
  if (!container) return null;
  const containerTop = container.getBoundingClientRect().top;
  const rows = container.querySelectorAll<HTMLElement>(itemSelector);
  for (const row of rows) {
    const rect = row.getBoundingClientRect();
    // 第一条底边已越过容器顶 = 视口里露出的最上面那条。
    if (rect.bottom > containerTop) {
      const id = getId(row);
      if (id != null) return { id, top: rect.top };
    }
  }
  return null;
}

/**
 * DOM 更新后，按 id 找回同一条行的新 rect.top，补偿 scrollTop = 旧 top 与新 top 的差。
 * 锚行已被裁掉 / 找不到时静默跳过（不补偿）。
 */
export function restoreTopAnchor(
  container: HTMLElement | null,
  anchor: TopAnchor | null,
  itemSelector: string,
  getId: (el: HTMLElement) => string | null = (el) => el.dataset.newsId ?? null,
): void {
  if (!container || !anchor) return;
  const rows = container.querySelectorAll<HTMLElement>(itemSelector);
  for (const row of rows) {
    if (getId(row) === anchor.id) {
      const newTop = row.getBoundingClientRect().top;
      const delta = newTop - anchor.top;
      if (delta !== 0) container.scrollTop += delta;
      return;
    }
  }
}
