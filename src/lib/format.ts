export function getTimeValue(value?: unknown) {
  if (!value) return 0;
  if (typeof value === "number") {
    if (!Number.isFinite(value)) return 0;
    return value > 0 && value < 10_000_000_000 ? value * 1000 : value;
  }
  if (value instanceof Date) {
    const time = value.getTime();
    return Number.isFinite(time) ? time : 0;
  }
  if (typeof value !== "string") return 0;
  const numericValue = Number(value);
  if (Number.isFinite(numericValue) && /^\d+$/.test(value)) {
    return value.length <= 10 ? numericValue * 1000 : numericValue;
  }
  return Date.parse(value) || 0;
}

export function formatDate(value?: unknown) {
  if (!value) return "时间未知";
  const time = getTimeValue(value);
  if (!time) return typeof value === "string" ? value : "时间未知";
  return new Intl.DateTimeFormat("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(time));
}

export function formatNumber(value: unknown) {
  if (typeof value !== "number" || !Number.isFinite(value)) return "--";
  return value.toFixed(2);
}

export function formatSigned(value: unknown) {
  if (typeof value !== "number" || !Number.isFinite(value)) return "--";
  return `${value > 0 ? "+" : ""}${value.toFixed(2)}`;
}

export function formatCount(value: number) {
  return Number.isFinite(value) && value > 0 ? Math.round(value).toLocaleString("zh-CN") : "--";
}

export function formatAmount(value: unknown) {
  if (typeof value !== "number" || !Number.isFinite(value)) return "--";
  if (Math.abs(value) >= 100000000) return `${(value / 100000000).toFixed(2)}亿`;
  if (Math.abs(value) >= 10000) return `${(value / 10000).toFixed(2)}万`;
  return value.toFixed(0);
}
