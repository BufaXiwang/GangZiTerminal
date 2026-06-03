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

import { useEffect, useMemo, useRef, useState } from "react";
import {
  beijingDateKey,
  beijingTodayKey,
  dateKeyWeekday,
  shiftDateKey,
} from "../../lib/beijingTime";

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

// 资讯归日统一用北京日历日（与后端 dateCounts 同口径）。
function formatDateKey(d: Date): string {
  return beijingDateKey(d);
}

function monthDayOfKey(key: string): string {
  return key.slice(5); // "YYYY-MM-DD" → "MM-DD"
}

export function NewsDateNav({
  daysBack = 14,
  countsByDate = {},
  activeDate,
  onSelect,
}: NewsDateNavProps) {
  // 跨午夜自动滚动「今天」：beijingTodayKey() 只在依赖变化时重算，若 app 跨天开着不刷新，
  // 顶部「今天」会卡在昨天。用一个每分钟检查的 tick，日历日真变了才 setState 触发重算。
  const [todayKey, setTodayKey] = useState(() => beijingTodayKey());
  useEffect(() => {
    const id = window.setInterval(() => {
      const k = beijingTodayKey();
      setTodayKey((prev) => (prev === k ? prev : k));
    }, 60_000);
    return () => window.clearInterval(id);
  }, []);

  const days = useMemo(() => {
    const today = todayKey;
    const out: { key: string; label: string; weekday: string; isToday: boolean }[] = [];
    for (let i = 0; i < daysBack; i++) {
      const key = shiftDateKey(today, -i);
      out.push({
        key,
        label: monthDayOfKey(key),
        weekday: WEEKDAY_LABEL[dateKeyWeekday(key)],
        isToday: i === 0,
      });
    }
    return out;
  }, [daysBack, todayKey]);

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
        const empty = count === 0;
        return (
          <button
            key={d.key}
            type="button"
            role="tab"
            ref={isActive ? activeStopRef : null}
            className={`news-date-timeline-stop${isActive ? " active" : ""}${d.isToday ? " today" : ""}${empty ? " empty" : ""}`}
            onClick={() => {
              if (!empty) onSelect(d.key);
            }}
            disabled={empty}
            aria-pressed={isActive}
            title={
              empty
                ? `${d.key} · 暂无资讯`
                : `${d.key} · ${count} 条`
            }
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
