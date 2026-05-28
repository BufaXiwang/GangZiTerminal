// MarketList — 紧凑虚拟化列表 (react-window v2 List)。
//
// Spec: docs/design/frontend-design.md §4 市场页 + quotes-module.md §4 list_market
//
// 行：左 name（带可选自选小标记 + ST 标）+ tsCode (灰小字)
//     右 price (tabular) + change% (红绿)
//
// 7500+ 标的用 react-window v2 List 虚拟化 —— 渲染只创建 ~30 个 DOM 节点。
// 切 tab 性能从 ~1-2s 降到 <100ms。
//
// 自选入口：右键 row 弹 context menu（外层 MarketPage 监听）。

import { Star } from "lucide-react";
import { useMemo } from "react";
import { List } from "react-window";
import type { CSSProperties } from "react";
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
  /** 右键 row → 弹自选 context menu */
  onContextMenu?: (tsCode: TsCode, clientX: number, clientY: number) => void;
  loading?: boolean;
}

const ROW_HEIGHT = 54;

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

interface RowProps {
  items: ListMarketItem[];
  selected: TsCode | null;
  starred: Set<TsCode>;
  onSelect: (tsCode: TsCode) => void;
  onContextMenu?: (tsCode: TsCode, x: number, y: number) => void;
}

function Row({
  index,
  style,
  items,
  selected,
  starred,
  onSelect,
  onContextMenu,
}: { index: number; style: CSSProperties } & RowProps) {
  const item = items[index];
  if (!item) return null;
  const isSel = item.tsCode === selected;
  const isStarred = starred.has(item.tsCode);
  const pct = item.quote?.changePercent;
  const tone = changeClass(pct);
  return (
    <div
      style={style}
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

  const rowProps: RowProps = useMemo(
    () => ({ items: sorted, selected, starred, onSelect, onContextMenu }),
    [sorted, selected, starred, onSelect, onContextMenu],
  );

  if (sorted.length === 0 && !loading) {
    return (
      <div className="market-list">
        <div className="market-list-empty">无标的</div>
      </div>
    );
  }

  return (
    <div className="market-list">
      <div className="market-list-body market-list-virtual">
        <List<RowProps>
          rowComponent={Row}
          rowCount={sorted.length}
          rowHeight={ROW_HEIGHT}
          rowProps={rowProps}
          // List 自动撑满父容器（取 defaultHeight fallback；ResizeObserver 内部跟踪）
          style={{ height: "100%", width: "100%" }}
        />
      </div>
    </div>
  );
}
