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
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { FetchNewsItem } from "../../bindings";
import { formatDateKey } from "./NewsDateNav";
import { NewsRowMenu } from "./NewsRowMenu";
import { beijingHHmm, beijingTodayKey, dateKeyWeekday } from "../../lib/beijingTime";

const WEEKDAY_LABEL = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];

export interface NewsTimelineProps {
  items: FetchNewsItem[];
  loading: boolean;
  loadingMore: boolean;
  hasMore: boolean;
  onLoadMore: () => void;
  registerSectionRef: (dateKey: string, el: HTMLElement | null) => void;
  onActiveDateChange: (dateKey: string) => void;
  query: string;
  /** sourceId → 友好展示名，row 上的 source chip 用；缺失时回落到 ID。 */
  sourceNames?: Record<string, string>;
}

interface DateGroup {
  dateKey: string;
  label: string;
  items: FetchNewsItem[];
}

const UNKNOWN_KEY = "__unknown__";

function formatDateLabel(dateKey: string): string {
  if (dateKey === UNKNOWN_KEY) return "未知日期";
  // dateKey 是北京日历日 YYYY-MM-DD
  const [y] = dateKey.split("-").map((s) => Number.parseInt(s, 10));
  if (Number.isNaN(y)) return dateKey;
  const wd = WEEKDAY_LABEL[dateKeyWeekday(dateKey)];
  const isToday = dateKey === beijingTodayKey();
  return `${dateKey} ${wd}${isToday ? " · 今天" : ""}`;
}

function formatHHmm(iso?: string): string {
  if (!iso) return "--:--";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "--:--";
  return beijingHHmm(d);
}

// 按 source 字符串散列到 0..4，给每条 row 一条稳定的左侧色条 + source tag 配色，
// 视觉上把同源资讯归簇。5 个色相都取自暖纸面 palette，不破坏整体调性。
function sourceAccent(source: string): number {
  let h = 0;
  for (let i = 0; i < source.length; i++) h = (h * 31 + source.charCodeAt(i)) >>> 0;
  return h % 5;
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

/** 单条资讯行：正文行内折叠 3 行；正文真溢出时**整行可点**展开/收起（不必点按钮）。 */
function NewsRow({
  it,
  query,
  sourceName,
  onContextMenu,
}: {
  it: FetchNewsItem;
  query: string;
  sourceName: string;
  onContextMenu: (it: FetchNewsItem, x: number, y: number) => void;
}) {
  const acc = sourceAccent(it.source);
  const warnings = it.warnings ?? [];
  const hasWarn = warnings.length > 0;
  // 优先全文（展开要看完整正文）；无全文回落 excerpt / summary。
  const body = it.article?.content ?? it.articleExcerpt ?? it.summary ?? "";
  const bodyRef = useRef<HTMLDivElement>(null);
  const [open, setOpen] = useState(false);
  const [overflow, setOverflow] = useState(false);
  // 只在折叠态测量真实溢出（open 时 clamp 解除会让 scrollHeight==clientHeight）。
  useLayoutEffect(() => {
    if (open) return;
    const el = bodyRef.current;
    if (el) setOverflow(el.scrollHeight > el.clientHeight + 1);
  }, [body, open]);

  return (
    <div
      className={`news-row acc-${acc}${open ? " expanded" : ""}${overflow ? " clickable" : ""}`}
      onClick={() => overflow && setOpen((v) => !v)}
      onContextMenu={(e) => {
        e.preventDefault();
        onContextMenu(it, e.clientX, e.clientY);
      }}
      role={overflow ? "button" : undefined}
      tabIndex={overflow ? 0 : undefined}
      onKeyDown={(e) => {
        if (overflow && (e.key === "Enter" || e.key === " ")) {
          e.preventDefault();
          setOpen((v) => !v);
        }
      }}
    >
      <div className="news-row-time tabular">
        <span className="news-row-dot" />
        {formatHHmm(it.publishedAt ?? undefined)}
      </div>
      <div className="news-row-body">
        <h3 className="news-row-title">
          {highlight(it.title, query)}
          {it.article && (
            <BookOpen size={13} className="news-row-article-icon" aria-label="已抽取正文" />
          )}
          {hasWarn && (
            <AlertTriangle size={13} className="news-row-warn-icon" aria-label={warnings.join(", ")} />
          )}
        </h3>
        {body && (
          <div ref={bodyRef} className={`news-row-body-text${open ? " open" : ""}`}>
            {highlight(body, query)}
          </div>
        )}
        {/* 提示文字（视觉），整行点击即切换，无需点它 */}
        {overflow && <span className="news-row-expand">{open ? "收起" : "展开"}</span>}
      </div>
      <span className={`news-src-tag acc-${acc}`}>{sourceName}</span>
    </div>
  );
}

export function NewsTimeline({
  items,
  loading,
  loadingMore,
  hasMore,
  onLoadMore,
  registerSectionRef,
  onActiveDateChange,
  query,
  sourceNames = {},
}: NewsTimelineProps) {
  // === 右键菜单（打开原文 / 复制链接） ===
  const [menu, setMenu] = useState<{ url: string | null; x: number; y: number } | null>(null);

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
          {g.items.map((it) => (
            <NewsRow
              key={it.id}
              it={it}
              query={query}
              sourceName={sourceNames[it.source] ?? it.source}
              onContextMenu={(item, x, y) =>
                setMenu({ url: item.url ?? null, x, y })
              }
            />
          ))}
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

      {menu && (
        <NewsRowMenu
          x={menu.x}
          y={menu.y}
          url={menu.url}
          onClose={() => setMenu(null)}
        />
      )}
    </div>
  );
}
