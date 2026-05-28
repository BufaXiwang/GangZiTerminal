// NewsTimeline — 纵向时间线（按日分组，倒序，滚动加载更多）。
//
// Spec: docs/design/frontend-design.md §4 资讯页 + §5 时间线
//       docs/design/news-module.md §4 fetch_news（默认按 publishedAt desc）
//
// 行为：
//   - items 按 publishedAt desc 排序（无 publishedAt 的归到末尾 "未知日期"）
//   - 按日期分组，每组顶部一个粘性 heading
//   - 每条 row：HH:mm | 标题 + 摘要 | source chip + 状态 flag
//   - IntersectionObserver 监听 sentinel → 到底自动 onLoadMore
//   - IntersectionObserver 监听 section header → 当前 viewport 顶部那天 = activeDate
//   - 父组件通过 registerSectionRef(dateKey, el) 拿到每天的 anchor 用于"点 nav 跳转"
//
// 红涨绿跌不适用资讯；只有 has-article 用绿色调，warning 用 state-warn 暖色。

import { AlertTriangle, BookOpen } from "lucide-react";
import { useEffect, useMemo, useRef } from "react";
import type { FetchNewsItem } from "../../bindings";
import { formatDateKey } from "./NewsDateNav";

const WEEKDAY_LABEL = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];

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

interface DateGroup {
  dateKey: string;
  label: string;
  items: FetchNewsItem[];
}

const UNKNOWN_KEY = "__unknown__";

function formatDateLabel(dateKey: string): string {
  if (dateKey === UNKNOWN_KEY) return "未知日期";
  // dateKey 是 YYYY-MM-DD
  const [y, m, d] = dateKey.split("-").map((s) => Number.parseInt(s, 10));
  if (Number.isNaN(y)) return dateKey;
  const date = new Date(y, m - 1, d);
  const wd = WEEKDAY_LABEL[date.getDay()];
  const today = new Date();
  const isToday =
    date.getFullYear() === today.getFullYear() &&
    date.getMonth() === today.getMonth() &&
    date.getDate() === today.getDate();
  return `${y}-${String(m).padStart(2, "0")}-${String(d).padStart(2, "0")} ${wd}${isToday ? " · 今天" : ""}`;
}

function formatHHmm(iso?: string): string {
  if (!iso) return "--:--";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "--:--";
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

function highlight(text: string, query: string): React.ReactNode {
  const q = query.trim();
  if (!q) return text;
  const idx = text.toLowerCase().indexOf(q.toLowerCase());
  if (idx === -1) return text;
  return (
    <>
      {text.slice(0, idx)}
      <mark>{text.slice(idx, idx + q.length)}</mark>
      {text.slice(idx + q.length)}
    </>
  );
}

export function NewsTimeline({
  items,
  loading,
  loadingMore,
  hasMore,
  onLoadMore,
  onSelectItem,
  selectedId,
  registerSectionRef,
  onActiveDateChange,
  query,
}: NewsTimelineProps) {
  // === sort + group ===
  const groups: DateGroup[] = useMemo(() => {
    const sorted = [...items].sort((a, b) => {
      const ta = a.publishedAt ? new Date(a.publishedAt).getTime() : -1;
      const tb = b.publishedAt ? new Date(b.publishedAt).getTime() : -1;
      return tb - ta;
    });
    const byKey: Record<string, FetchNewsItem[]> = {};
    const order: string[] = [];
    for (const it of sorted) {
      let key = UNKNOWN_KEY;
      if (it.publishedAt) {
        const d = new Date(it.publishedAt);
        if (!Number.isNaN(d.getTime())) key = formatDateKey(d);
      }
      if (!byKey[key]) {
        byKey[key] = [];
        order.push(key);
      }
      byKey[key].push(it);
    }
    return order.map((k) => ({
      dateKey: k,
      label: formatDateLabel(k),
      items: byKey[k],
    }));
  }, [items]);

  // === auto-load on sentinel visible ===
  const sentinelRef = useRef<HTMLDivElement | null>(null);
  const onLoadMoreRef = useRef(onLoadMore);
  onLoadMoreRef.current = onLoadMore;

  useEffect(() => {
    const node = sentinelRef.current;
    if (!node || !hasMore) return;
    const obs = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) {
            onLoadMoreRef.current();
          }
        }
      },
      { rootMargin: "200px" },
    );
    obs.observe(node);
    return () => obs.disconnect();
  }, [hasMore, groups.length]);

  // === active date tracking via header intersect ===
  const headerRefs = useRef<Map<string, HTMLElement>>(new Map());
  const onActiveDateChangeRef = useRef(onActiveDateChange);
  onActiveDateChangeRef.current = onActiveDateChange;

  useEffect(() => {
    const obs = new IntersectionObserver(
      (entries) => {
        // 选最靠上的可见 header 的 dateKey
        let topMost: { dateKey: string; y: number } | null = null;
        for (const entry of entries) {
          if (!entry.isIntersecting) continue;
          const key = (entry.target as HTMLElement).dataset.dateKey;
          if (!key) continue;
          const y = entry.boundingClientRect.top;
          if (topMost === null || y < topMost.y) {
            topMost = { dateKey: key, y };
          }
        }
        if (topMost) onActiveDateChangeRef.current(topMost.dateKey);
      },
      { rootMargin: "0px 0px -70% 0px", threshold: [0, 1] },
    );
    for (const el of headerRefs.current.values()) obs.observe(el);
    return () => obs.disconnect();
  }, [groups.map((g) => g.dateKey).join(",")]);

  const registerHeaderRef = (key: string, el: HTMLElement | null) => {
    if (el) headerRefs.current.set(key, el);
    else headerRefs.current.delete(key);
    // 同时回传给父组件作为 jump anchor
    registerSectionRef(key, el);
  };

  // === render ===
  if (loading && items.length === 0) {
    return (
      <div className="news-timeline-loading">加载中…</div>
    );
  }

  if (items.length === 0) {
    return (
      <div className="news-timeline-empty">
        <div className="placeholder-title" style={{ marginBottom: 4 }}>
          无资讯
        </div>
        <div>调整过滤条件、清空搜索关键字或等待 scheduler 刷新</div>
      </div>
    );
  }

  return (
    <div className="news-timeline">
      {groups.map((g) => (
        <section
          key={g.dateKey}
          className="news-day-section"
          data-date-key={g.dateKey}
        >
          <header
            className="news-day-head"
            data-date-key={g.dateKey}
            ref={(el) => registerHeaderRef(g.dateKey, el)}
          >
            <span className="news-day-label">{g.label}</span>
            <span className="news-day-count">{g.items.length} 条</span>
          </header>
          {g.items.map((it) => {
            const hasArticle = Boolean(it.articleExcerpt) || Boolean(it.article);
            const hasWarn = it.warnings.length > 0;
            return (
              <div
                key={it.id}
                className={`news-row ${selectedId === it.id ? "selected" : ""}`}
                onClick={() => onSelectItem(it.id)}
                role="button"
                tabIndex={0}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    onSelectItem(it.id);
                  }
                }}
              >
                <div className="news-row-time tabular">
                  {formatHHmm(it.publishedAt)}
                </div>
                <div className="news-row-body">
                  <h3 className="news-row-title">{highlight(it.title, query)}</h3>
                  {it.summary && (
                    <div className="news-row-summary">
                      {highlight(it.summary, query)}
                    </div>
                  )}
                </div>
                <div className="news-row-meta">
                  <span className="news-row-source">{it.source}</span>
                  {(hasArticle || hasWarn) && (
                    <div className="news-row-flags">
                      {hasArticle && (
                        <span className="news-row-flag has-article" title="本地已抽取正文">
                          <BookOpen size={10} /> 正文
                        </span>
                      )}
                      {hasWarn && (
                        <span
                          className="news-row-flag warn"
                          title={it.warnings.join(", ")}
                        >
                          <AlertTriangle size={10} /> {it.warnings.length}
                        </span>
                      )}
                    </div>
                  )}
                </div>
              </div>
            );
          })}
        </section>
      ))}

      {/* sentinel for auto-load */}
      {hasMore && <div ref={sentinelRef} style={{ height: 1 }} />}

      <div className="news-load-more">
        {hasMore ? (
          <button
            className="btn"
            type="button"
            disabled={loadingMore}
            onClick={onLoadMore}
          >
            {loadingMore ? "加载中…" : "加载更多"}
          </button>
        ) : (
          <span className="muted" style={{ fontSize: 12 }}>
            — 已到底 ({items.length} 条) —
          </span>
        )}
      </div>
    </div>
  );
}
