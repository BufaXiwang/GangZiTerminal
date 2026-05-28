// MarketList — 紧凑列表（每行 2 行：名称 + 代码 / 价格 + 涨跌幅）。
//
// Spec: docs/design/frontend-design.md §4 市场页 + quotes-module.md §4 list_market
//
// 行：左 name（带可选自选小标记 + ST 标）+ tsCode (灰小字)
//     右 price (tabular) + change% (红绿)
//
// 自选入口：右键 row 弹 context menu（外层 MarketPage 监听），不再在行内显示 star 按钮。

import { Star } from "lucide-react";
import { useMemo } from "react";
import type { ListMarketItem, TsCode } from "../../bindings";

export type SortKey = "default" | "changePercent" | "amount";
export type SortDir = "asc" | "desc";

interface MarketListProps {
  items: ListMarketItem[];
  selected: TsCode | null;
  onSelect: (tsCode: TsCode) => void;
  sortKey: SortKey;
  sortDir: SortDir;
  starred: Set<TsCode>;
  /** 右键 row → 弹自选 context menu；caller 拿 mouse 位置渲染浮层 */
  onContextMenu?: (tsCode: TsCode, clientX: number, clientY: number) => void;
  loading?: boolean;
}

function fmtNum(v: number | undefined | null, digits = 2): string {
  if (v == null || !Number.isFinite(v)) return "-";
  return v.toLocaleString("en-US", {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });
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

function sortValue(item: ListMarketItem, key: SortKey): number | null {
  switch (key) {
    case "changePercent":
      return item.quote?.changePercent ?? null;
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
  if (key === "default") return 0;
  const av = sortValue(a, key);
  const bv = sortValue(b, key);
  const aNull = av == null || !Number.isFinite(av);
  const bNull = bv == null || !Number.isFinite(bv);
  if (aNull && bNull) return 0;
  if (aNull) return 1;
  if (bNull) return -1;
  const cmp = (av as number) - (bv as number);
  return dir === "asc" ? cmp : -cmp;
}

export function MarketList({
  items,
  selected,
  onSelect,
  sortKey,
  sortDir,
  starred,
  onContextMenu,
  loading,
}: MarketListProps) {
  const sorted = useMemo(() => {
    if (sortKey === "default") return items;
    const copy = [...items];
    copy.sort((a, b) => compareSort(a, b, sortKey, sortDir));
    return copy;
  }, [items, sortKey, sortDir]);

  return (
    <div className="market-list">
      <div className="market-list-body">
        {sorted.length === 0 && !loading && (
          <div className="market-list-empty">无标的</div>
        )}
        {sorted.map((item) => {
          const isSel = item.tsCode === selected;
          const isStarred = starred.has(item.tsCode);
          const pct = item.quote?.changePercent;
          const tone = changeClass(pct);
          return (
            <div
              key={item.tsCode}
              role="row"
              className={`market-list-row compact ${isSel ? "selected" : ""}`}
              onClick={() => onSelect(item.tsCode)}
              onContextMenu={(e) => {
                e.preventDefault();
                onContextMenu?.(item.tsCode, e.clientX, e.clientY);
              }}
            >
              <div className="row-left">
                <div className="row-name">
                  {isStarred && (
                    <Star
                      size={11}
                      className="row-star-indicator"
                      fill="currentColor"
                      strokeWidth={0}
                      aria-label="已加自选"
                    />
                  )}
                  <span className="instrument-name">{item.name}</span>
                  {item.isSt && <span className="chip st-chip">ST</span>}
                </div>
                <div className="row-code tabular">{item.tsCode}</div>
              </div>
              <div className="row-right">
                <div className={`row-price tabular ${tone}`}>
                  {fmtNum(item.quote?.price)}
                </div>
                <div className={`row-pct tabular ${tone}`}>{fmtPct(pct)}</div>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
