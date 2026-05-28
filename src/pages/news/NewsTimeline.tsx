// NewsTimeline — 纵向时间线（按日分组，倒序，滚动加载更多）。
//
// Spec: docs/design/frontend-design.md §4 资讯页 + §5 时间线
//
// C2 阶段：stub —— 把 items 简单按 publishedAt desc 排序并以平铺方式展示，
// 让 NewsPage 主结构能 build 通过。
// C3 会接管渲染 + 分组 + IntersectionObserver。

import type { FetchNewsItem } from "../../bindings";

export interface NewsTimelineProps {
  items: FetchNewsItem[];
  loading: boolean;
  loadingMore: boolean;
  hasMore: boolean;
  onLoadMore: () => void;
  onSelectItem: (id: string) => void;
  selectedId: string | null;
  registerSectionRef: (dateKey: string, el: HTMLElement | null) => void;
  onActiveDateChange: (dateKey: string) => void;
  query: string;
}

export function NewsTimeline({
  items,
  loading,
  hasMore,
  onLoadMore,
  loadingMore,
}: NewsTimelineProps) {
  return (
    <div className="news-timeline-placeholder placeholder-block">
      <div className="placeholder-title">资讯时间线（C3 渲染）</div>
      <div>当前已加载 {items.length} 条 · {loading ? "loading" : hasMore ? "还有更多" : "已到底"}</div>
      {hasMore && (
        <button className="btn" type="button" disabled={loadingMore} onClick={onLoadMore}>
          {loadingMore ? "加载中…" : "加载更多"}
        </button>
      )}
    </div>
  );
}
