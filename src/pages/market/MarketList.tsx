// MarketList — 市场列表组件（table-like rows）。
//
// Spec: docs/design/frontend-design.md §4 市场页 + quotes-module.md §4 list_market
//
// 职责：
// - 渲染 ListMarketItem 数组成表格
// - 列：tsCode / name / price / change% / volume / amount / star
// - 排序：默认 amount 降序，可点列头切换
// - 选中行 → 调用 onSelect

import { ChevronDown, ChevronUp, Star } from "lucide-react";
import { useMemo } from "react";
import type { ListMarketItem, TsCode } from "../../bindings";

export type SortKey =
  | "tsCode"
  | "name"
  | "price"
  | "changePercent"
  | "volume"
  | "amount";
export type SortDir = "asc" | "desc";

interface MarketListProps {
  items: ListMarketItem[];
  selected: TsCode | null;
  onSelect: (tsCode: TsCode) => void;
  sortKey: SortKey;
  sortDir: SortDir;
  onSort: (key: SortKey) => void;
  starred: Set<TsCode>;
  onToggleStar: (tsCode: TsCode) => void;
  loading?: boolean;
}

interface ColumnDef {
  key: SortKey;
  label: string;
  align: "left" | "right";
  className?: string;
}

const COLUMNS: ColumnDef[] = [
  { key: "tsCode", label: "代码", align: "left" },
  { key: "name", label: "名称", align: "left" },
  { key: "price", label: "现价", align: "right" },
  { key: "changePercent", label: "涨跌幅", align: "right" },
  { key: "volume", label: "成交量", align: "right" },
  { key: "amount", label: "成交额", align: "right" },
];

function fmtNum(v: number | undefined | null, digits = 2): string {
  if (v == null || !Number.isFinite(v)) return "-";
  return v.toLocaleString("en-US", {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });
}

function fmtVolume(v: number | undefined | null): string {
  if (v == null || !Number.isFinite(v)) return "-";
  if (Math.abs(v) >= 1e8) return `${(v / 1e8).toFixed(2)}亿`;
  if (Math.abs(v) >= 1e4) return `${(v / 1e4).toFixed(2)}万`;
  return v.toLocaleString("en-US");
}

function fmtAmount(v: number | undefined | null): string {
  if (v == null || !Number.isFinite(v)) return "-";
  if (Math.abs(v) >= 1e8) return `${(v / 1e8).toFixed(2)}亿`;
  if (Math.abs(v) >= 1e4) return `${(v / 1e4).toFixed(2)}万`;
  return v.toFixed(0);
}

function changeClass(v: number | undefined | null): string {
  if (v == null) return "flat";
  if (v > 0) return "up";
  if (v < 0) return "down";
  return "flat";
}

function fmtPct(v: number | undefined | null): string {
  if (v == null || !Number.isFinite(v)) return "-";
  const sign = v > 0 ? "+" : "";
  return `${sign}${v.toFixed(2)}%`;
}

function sortValue(item: ListMarketItem, key: SortKey): string | number | null {
  switch (key) {
    case "tsCode":
      return item.tsCode;
    case "name":
      return item.name ?? "";
    case "price":
      return item.quote?.price ?? null;
    case "changePercent":
      return item.quote?.changePercent ?? null;
    case "volume":
      return item.quote?.volume ?? null;
    case "amount":
      return item.quote?.amount ?? null;
    default:
      return null;
  }
}

function compareSort(
  a: ListMarketItem,
  b: ListMarketItem,
  key: SortKey,
  dir: SortDir,
): number {
  const av = sortValue(a, key);
  const bv = sortValue(b, key);
  // null 永远排到末尾
  const aNull = av == null || (typeof av === "number" && !Number.isFinite(av));
  const bNull = bv == null || (typeof bv === "number" && !Number.isFinite(bv));
  if (aNull && bNull) return 0;
  if (aNull) return 1;
  if (bNull) return -1;
  let cmp = 0;
  if (typeof av === "number" && typeof bv === "number") {
    cmp = av - bv;
  } else {
    cmp = String(av).localeCompare(String(bv));
  }
  return dir === "asc" ? cmp : -cmp;
}

export function MarketList({
  items,
  selected,
  onSelect,
  sortKey,
  sortDir,
  onSort,
  starred,
  onToggleStar,
  loading,
}: MarketListProps) {
  const sorted = useMemo(() => {
    const copy = [...items];
    copy.sort((a, b) => compareSort(a, b, sortKey, sortDir));
    return copy;
  }, [items, sortKey, sortDir]);

  return (
    <div className="market-list">
      <div className="market-list-header" role="row">
        {COLUMNS.map((c) => (
          <button
            key={c.key}
            type="button"
            className={`market-list-th ${c.align} ${
              sortKey === c.key ? "active" : ""
            }`}
            onClick={() => onSort(c.key)}
          >
            <span>{c.label}</span>
            {sortKey === c.key &&
              (sortDir === "desc" ? (
                <ChevronDown size={12} />
              ) : (
                <ChevronUp size={12} />
              ))}
          </button>
        ))}
        <div className="market-list-th right star-col">自选</div>
      </div>
      <div className="market-list-body">
        {sorted.length === 0 && !loading && (
          <div className="market-list-empty">无标的</div>
        )}
        {sorted.map((item) => {
          const isSel = item.tsCode === selected;
          const isStarred = starred.has(item.tsCode);
          const pct = item.quote?.changePercent;
          return (
            <div
              key={item.tsCode}
              role="row"
              className={`market-list-row ${isSel ? "selected" : ""}`}
              onClick={() => onSelect(item.tsCode)}
            >
              <div className="market-list-cell left tabular">
                {item.tsCode}
              </div>
              <div className="market-list-cell left">
                <span className="instrument-name">{item.name}</span>
                {item.isSt && <span className="chip st-chip">ST</span>}
              </div>
              <div className="market-list-cell right tabular">
                {fmtNum(item.quote?.price)}
              </div>
              <div
                className={`market-list-cell right tabular ${changeClass(pct)}`}
              >
                {fmtPct(pct)}
              </div>
              <div className="market-list-cell right tabular">
                {fmtVolume(item.quote?.volume)}
              </div>
              <div className="market-list-cell right tabular">
                {fmtAmount(item.quote?.amount)}
              </div>
              <div className="market-list-cell right star-col">
                <button
                  type="button"
                  className={`star-btn ${isStarred ? "starred" : ""}`}
                  onClick={(e) => {
                    e.stopPropagation();
                    onToggleStar(item.tsCode);
                  }}
                  aria-label={isStarred ? "已加入自选" : "加入自选"}
                  title={isStarred ? "已加入自选" : "加入自选"}
                >
                  <Star
                    size={14}
                    fill={isStarred ? "currentColor" : "none"}
                    strokeWidth={1.5}
                  />
                </button>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
