// account/format.ts — 模拟账户页共用格式化工具。
//
// Spec: docs/design/frontend-design.md §2 视觉系统（数字右对齐 / 等宽 / 红涨绿跌 / 空值 -）
//
// 后端 Money / Price / Amount 序列化为 string（保留精度），Volume / Shares 是 number。
// 前端展示时统一转 number 走 Intl.NumberFormat；解析失败显示 `-`。

export function toNumber(v: unknown): number | null {
  if (v == null) return null;
  if (typeof v === "number") return Number.isFinite(v) ? v : null;
  if (typeof v === "string" && v.trim() !== "") {
    const n = Number(v);
    return Number.isFinite(n) ? n : null;
  }
  return null;
}

export function fmtNum(v: unknown, digits = 2): string {
  const n = toNumber(v);
  if (n == null) return "-";
  return n.toLocaleString("en-US", {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });
}

export function fmtMoneyShort(v: unknown): string {
  const n = toNumber(v);
  if (n == null) return "-";
  if (Math.abs(n) >= 1e8) return `${(n / 1e8).toFixed(2)}亿`;
  if (Math.abs(n) >= 1e4) return `${(n / 1e4).toFixed(2)}万`;
  return n.toFixed(2);
}

export function fmtAmountShort(v: unknown): string {
  // 与 fmtMoneyShort 类似，但成交额通常不需要小数
  const n = toNumber(v);
  if (n == null) return "-";
  if (Math.abs(n) >= 1e8) return `${(n / 1e8).toFixed(2)}亿`;
  if (Math.abs(n) >= 1e4) return `${(n / 1e4).toFixed(2)}万`;
  return n.toFixed(0);
}

export function fmtVolume(v: unknown): string {
  const n = toNumber(v);
  if (n == null) return "-";
  if (Math.abs(n) >= 1e8) return `${(n / 1e8).toFixed(2)}亿`;
  if (Math.abs(n) >= 1e4) return `${(n / 1e4).toFixed(2)}万`;
  return n.toLocaleString("en-US");
}

export function fmtPct(v: unknown): string {
  const n = toNumber(v);
  if (n == null) return "-";
  const sign = n > 0 ? "+" : "";
  return `${sign}${n.toFixed(2)}%`;
}

export function fmtSignedMoney(v: unknown): string {
  const n = toNumber(v);
  if (n == null) return "-";
  const sign = n > 0 ? "+" : "";
  return `${sign}${fmtMoneyShort(n)}`;
}

export function fmtShares(v: unknown): string {
  const n = toNumber(v);
  if (n == null) return "-";
  return n.toLocaleString("en-US", { maximumFractionDigits: 0 });
}

export function changeClass(v: unknown): "up" | "down" | "flat" {
  const n = toNumber(v);
  if (n == null) return "flat";
  if (n > 0) return "up";
  if (n < 0) return "down";
  return "flat";
}

/** signed money 类（盈亏色）— null 视为 flat。 */
export function pnlClass(v: unknown): "up" | "down" | "flat" {
  return changeClass(v);
}

export function fmtDateTime(iso?: string | null): string {
  if (!iso) return "-";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString("zh-CN", { hour12: false });
}

export function fmtTimeShort(iso?: string | null): string {
  if (!iso) return "-";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleTimeString("zh-CN", { hour12: false });
}
