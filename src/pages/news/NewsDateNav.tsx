// NewsDateNav — 顶部横向日期 chip 滑动器。
//
// Spec: docs/design/frontend-design.md §4 资讯页（时间线推荐结构）
//
// 展示最近 N 天（含今天）的 chip，每个 chip：
//   - 日期（MM-DD）+ "周X"
//   - 该天的资讯条数徽标
//   - 点击 → 触发 onSelect(date) 让父级把主时间线滚到对应日期段
//   - 当前 viewport 的日期高亮（由父组件决定 activeDate）
//
// 不发请求，纯展示组件。

import { useEffect, useMemo, useRef } from "react";

const WEEKDAY_LABEL = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];

export interface NewsDateNavProps {
  /** 显示最近 N 天，默认 14。 */
  daysBack?: number;
  /** key: YYYY-MM-DD, value: 该日条数。 */
  countsByDate?: Record<string, number>;
  /** 当前选中（高亮）的日期 YYYY-MM-DD，可为 null。 */
  activeDate?: string | null;
  onSelect: (dateKey: string) => void;
}

function formatDateKey(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function formatMonthDay(d: Date): string {
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${m}-${day}`;
}

export function NewsDateNav({
  daysBack = 14,
  countsByDate = {},
  activeDate,
  onSelect,
}: NewsDateNavProps) {
  const days = useMemo(() => {
    const now = new Date();
    const out: { key: string; date: Date; label: string; weekday: string; isToday: boolean }[] = [];
    for (let i = 0; i < daysBack; i++) {
      const d = new Date(now);
      d.setDate(now.getDate() - i);
      out.push({
        key: formatDateKey(d),
        date: d,
        label: formatMonthDay(d),
        weekday: WEEKDAY_LABEL[d.getDay()],
        isToday: i === 0,
      });
    }
    return out;
  }, [daysBack]);

  const scrollerRef = useRef<HTMLDivElement | null>(null);
  const activeChipRef = useRef<HTMLButtonElement | null>(null);

  // 当 activeDate 变化时，把高亮 chip 滚到可视区域内
  useEffect(() => {
    if (!activeChipRef.current) return;
    activeChipRef.current.scrollIntoView({
      behavior: "smooth",
      block: "nearest",
      inline: "center",
    });
  }, [activeDate]);

  return (
    <div className="news-date-nav" ref={scrollerRef}>
      {days.map((d) => {
        const count = countsByDate[d.key] ?? 0;
        const isActive = activeDate === d.key;
        return (
          <button
            key={d.key}
            type="button"
            ref={isActive ? activeChipRef : null}
            className={`news-date-chip ${isActive ? "active" : ""} ${d.isToday ? "today" : ""}`}
            onClick={() => onSelect(d.key)}
            aria-pressed={isActive}
            title={d.key}
          >
            <div className="news-date-chip-top">
              <span className="news-date-chip-md tabular">{d.label}</span>
              <span className="news-date-chip-wd">
                {d.isToday ? "今天" : d.weekday}
              </span>
            </div>
            <div className="news-date-chip-count">
              {count > 0 ? <span className="news-date-chip-badge">{count}</span> : <span className="news-date-chip-badge empty">—</span>}
            </div>
          </button>
        );
      })}
    </div>
  );
}

export { formatDateKey };
