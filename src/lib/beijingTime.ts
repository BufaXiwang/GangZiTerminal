// 北京时间(UTC+8)格式化工具。
//
// Spec: news-module.md §4（dateCounts 按 Asia/Shanghai 聚合）
//
// 资讯按发布时间归日 / 显示时分必须统一用北京时区：后端 date_counts 按北京(+8)
// GROUP BY，前端若按浏览器本地分组，在非 UTC+8 机器上日期段与计数会错位（条目
// 落到相邻日、nav 计数贴错天）。中国无夏令时，Asia/Shanghai 恒为 UTC+8。

const KEY_FMT = new Intl.DateTimeFormat("en-CA", {
  timeZone: "Asia/Shanghai",
  year: "numeric",
  month: "2-digit",
  day: "2-digit",
});
const HHMM_FMT = new Intl.DateTimeFormat("en-GB", {
  timeZone: "Asia/Shanghai",
  hour: "2-digit",
  minute: "2-digit",
  hour12: false,
});

/** Date → "YYYY-MM-DD"（北京日历日）。en-CA 输出即 ISO 形式。 */
export function beijingDateKey(d: Date): string {
  return KEY_FMT.format(d);
}

/** Date → "HH:mm"（北京时间）。 */
export function beijingHHmm(d: Date): string {
  return HHMM_FMT.format(d);
}

/** 今天的北京日历日 key。 */
export function beijingTodayKey(): string {
  return beijingDateKey(new Date());
}

/** 把 "YYYY-MM-DD" 往前/后挪 delta 天。纯日历运算（UTC 锚定，避开本地 TZ）。 */
export function shiftDateKey(key: string, delta: number): string {
  const [y, m, d] = key.split("-").map((s) => Number.parseInt(s, 10));
  const dt = new Date(Date.UTC(y, m - 1, d) + delta * 86_400_000);
  const yy = dt.getUTCFullYear();
  const mm = String(dt.getUTCMonth() + 1).padStart(2, "0");
  const dd = String(dt.getUTCDate()).padStart(2, "0");
  return `${yy}-${mm}-${dd}`;
}

/** "YYYY-MM-DD" 的星期几 index（0=周日），UTC 锚定，与日历日一致。 */
export function dateKeyWeekday(key: string): number {
  const [y, m, d] = key.split("-").map((s) => Number.parseInt(s, 10));
  return new Date(Date.UTC(y, m - 1, d)).getUTCDay();
}
