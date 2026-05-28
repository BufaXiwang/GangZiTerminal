// NewsDateNav — 顶部横向时间轴（点-线样式）。
//
// Spec: docs/design/frontend-design.md §4 资讯页（时间线推荐结构）
//
// 视觉：
//   日期1     日期2     日期3     ...
//     ●────────●────────●────────...
//   (count)  (count)  (count)
//
//   - active 点放大并高亮，背景圈
//   - today 点带 brand 描边
//   - 条数 0 显示 "—" 灰色；> 0 显示数字
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

  const activeStopRef = useRef<HTMLButtonElement | null>(null);

  useEffect(() => {
    if (!activeStopRef.current) return;
    activeStopRef.current.scrollIntoView({
      behavior: "smooth",
      block: "nearest",
      inline: "center",
    });
  }, [activeDate]);

  return (
    <div className="news-date-timeline" role="tablist">
      <div className="news-date-timeline-line" aria-hidden="true" />
      {days.map((d) => {
        const count = countsByDate[d.key] ?? 0;
        const isActive = activeDate === d.key;
        return (
          <button
            key={d.key}
            type="button"
            role="tab"
            ref={isActive ? activeStopRef : null}
            className={`news-date-timeline-stop${isActive ? " active" : ""}${d.isToday ? " today" : ""}`}
            onClick={() => onSelect(d.key)}
            aria-pressed={isActive}
            title={`${d.key}${count > 0 ? ` · ${count} 条` : ""}`}
          >
            <span className="news-date-timeline-label tabular">
              {d.isToday ? "今天" : d.label}
            </span>
            <span className="news-date-timeline-wd">
              {d.isToday ? d.label : d.weekday}
            </span>
            <span className="news-date-timeline-dot" aria-hidden="true" />
            <span className={`news-date-timeline-count${count === 0 ? " empty" : ""}`}>
              {count > 0 ? count : "—"}
            </span>
          </button>
        );
      })}
    </div>
  );
}

export { formatDateKey };
