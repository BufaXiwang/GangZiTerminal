// IndexCard — 顶部指数卡片（4 个核心指数）。
//
// Spec: docs/design/frontend-design.md §4 + quotes-module.md §2 core_indexes
//
// 内容：名称（小灰）+ 当前值（大字 tabular）+ 涨跌幅 %（红绿，A 股语义）。

import type { FetchDataItem } from "../../bindings";

interface IndexCardProps {
  label: string;
  tsCode: string;
  item: FetchDataItem | undefined;
  /** 是否当前选中（点击可定位到列表）；可选 */
  active?: boolean;
  onClick?: () => void;
}

function fmtPrice(v: number | undefined): string {
  if (v == null || !Number.isFinite(v)) return "-";
  return v.toFixed(2);
}

function fmtPct(v: number | undefined | null): string {
  if (v == null || !Number.isFinite(v)) return "-";
  const sign = v > 0 ? "+" : "";
  return `${sign}${v.toFixed(2)}%`;
}

function fmtChange(v: number | undefined | null): string {
  if (v == null || !Number.isFinite(v)) return "-";
  const sign = v > 0 ? "+" : "";
  return `${sign}${v.toFixed(2)}`;
}

function pctClass(v: number | undefined | null): string {
  if (v == null) return "flat";
  if (v > 0) return "up";
  if (v < 0) return "down";
  return "flat";
}

function priceFromQuote(q: FetchDataItem["quote"]): number | undefined {
  if (!q?.price) return undefined;
  const n = Number(q.price);
  return Number.isFinite(n) ? n : undefined;
}

function numberOf(v: string | number | null | undefined): number | undefined {
  if (v == null) return undefined;
  const n = typeof v === "number" ? v : Number(v);
  return Number.isFinite(n) ? n : undefined;
}

export function IndexCard({
  label,
  tsCode,
  item,
  active,
  onClick,
}: IndexCardProps) {
  const q = item?.quote;
  const price = priceFromQuote(q);
  const pct = q?.changePercent ?? undefined;
  const change = numberOf(q?.change);
  const tone = pctClass(pct);

  return (
    <button
      type="button"
      className={`metric-card index-card ${tone} ${active ? "active" : ""}`}
      onClick={onClick}
    >
      <div className="metric-card-head">
        <span className="metric-card-label">{label}</span>
        <span className="metric-card-sub tabular">{tsCode}</span>
      </div>
      <div className={`metric-card-value tabular ${tone}`}>
        {fmtPrice(price)}
      </div>
      <div className={`metric-card-foot tabular ${tone}`}>
        <span>{fmtChange(change)}</span>
        <span>{fmtPct(pct)}</span>
      </div>
    </button>
  );
}
