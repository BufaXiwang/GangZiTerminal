import { Search, Star, StarOff } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { KlineChart } from "./KlineChart";
import { useMarketInstruments } from "../hooks/useMarketInstruments";
import { useMarketQuotes } from "../hooks/useMarketQuotes";
import { useWatchlist } from "../hooks/useWatchlist";
import type { InstrumentCategory, MarketInstrument, MarketQuote } from "../types";

type Category = InstrumentCategory;
type SortKey = "change" | "volume";

const CORE_INDICES: Array<{ tsCode: string; name: string }> = [
  { tsCode: "000001.SH", name: "上证指数" },
  { tsCode: "399001.SZ", name: "深证成指" },
  { tsCode: "399006.SZ", name: "创业板指" },
  { tsCode: "000688.SH", name: "科创50" },
];

const CATEGORY_TABS: Array<[Category, string]> = [
  ["stock", "股票"],
  ["index", "指数"],
  ["fund", "基金"],
];

const SORT_TABS: Array<[SortKey, string]> = [
  ["change", "涨跌"],
  ["volume", "成交量"],
];

// 列表只渲染屏幕内 + 缓冲——全市场 7000+ 不能一次性 DOM。
// 简单 fixed-row virtualization：行高固定 42px。
const ROW_HEIGHT = 56;
const OVERSCAN = 6;

type RowMenu = {
  x: number;
  y: number;
  code: string;
  name: string;
  inWatchlist: boolean;
};

export function TodayPage() {
  const { instruments, loading: instrumentsLoading } = useMarketInstruments();
  const { quoteMap, lastRefreshed } = useMarketQuotes();
  const { entries: watchlistEntries, add: addWatchlist, remove: removeWatchlist } = useWatchlist();

  const [selectedTsCode, setSelectedTsCode] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [category, setCategory] = useState<Category>("stock");
  const [sortKey, setSortKey] = useState<SortKey>("change");
  const [rowMenu, setRowMenu] = useState<RowMenu | null>(null);

  // 自选股 code 集合（用于 row indicator + 菜单状态判断）
  const watchedCodes = useMemo(
    () => new Set(watchlistEntries.map((e) => e.code)),
    [watchlistEntries],
  );

  // 列表 derive：分类 → 过滤 → 排序。默认股票按涨跌幅从高到低。
  const filteredRows = useMemo(() => {
    let next = instruments.filter((i) => i.category === category);
    const trimmed = filter.trim().toLowerCase();
    if (trimmed) {
      next = next.filter(
        (i) =>
          i.code.includes(trimmed) ||
          (i.name && i.name.toLowerCase().includes(trimmed)),
      );
    }
    next = [...next].sort((a, b) => {
      const aQuote = quoteMap.get(a.tsCode);
      const bQuote = quoteMap.get(b.tsCode);
      const aValue = sortKey === "change" ? aQuote?.changePercent : aQuote?.volume;
      const bValue = sortKey === "change" ? bQuote?.changePercent : bQuote?.volume;
      const aRank = Number.isFinite(aValue) ? (aValue as number) : -Infinity;
      const bRank = Number.isFinite(bValue) ? (bValue as number) : -Infinity;
      if (bRank !== aRank) return bRank - aRank;
      return a.code.localeCompare(b.code);
    });
    return next;
  }, [instruments, quoteMap, category, filter, sortKey]);

  // 列表上下文：当前选中
  const selectedInstrument = useMemo(
    () =>
      selectedTsCode
        ? instruments.find((i) => i.tsCode === selectedTsCode) ?? null
        : null,
    [instruments, selectedTsCode],
  );
  const selectedQuote = selectedTsCode ? quoteMap.get(selectedTsCode) ?? null : null;

  const summary = useMemo(
    () => buildMarketSummary(instruments, quoteMap),
    [instruments, quoteMap],
  );

  // 默认选中：第一条；过滤/切分类后如果当前项不可见，跟随列表切换。
  useEffect(() => {
    if (filteredRows.length === 0) {
      if (selectedTsCode !== null) setSelectedTsCode(null);
      return;
    }
    const stillVisible = selectedTsCode
      ? filteredRows.some((row) => row.tsCode === selectedTsCode)
      : false;
    if (!stillVisible) setSelectedTsCode(filteredRows[0]?.tsCode ?? null);
  }, [filteredRows, selectedTsCode]);

  // 右键菜单关闭——只在以下情况关：
  // 1. 左键 mousedown 在菜单**外部**（含点 row、点空白、点其它 UI）
  // 2. ESC
  // 右键 (button=2) 不关——让 row 的 onContextMenu 自然覆盖到新 row。
  // 用 mousedown 而非 click 是为了避免 React root delegation 在 stopPropagation
  // 之后 native click 仍冒泡到 document 把菜单关掉。
  useEffect(() => {
    if (!rowMenu) return;
    const onMouseDown = (e: MouseEvent) => {
      if (e.button === 2) return;
      const target = e.target as Element | null;
      if (target && target.closest(".today-row-menu")) return;
      setRowMenu(null);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setRowMenu(null);
    };
    document.addEventListener("mousedown", onMouseDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onMouseDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [rowMenu]);

  // 虚拟列表 scroll 状态
  const [scrollTop, setScrollTop] = useState(0);
  const [viewportH, setViewportH] = useState(600);
  const startIdx = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN);
  const endIdx = Math.min(
    filteredRows.length,
    Math.ceil((scrollTop + viewportH) / ROW_HEIGHT) + OVERSCAN,
  );
  const visibleRows = filteredRows.slice(startIdx, endIdx);
  const padTop = startIdx * ROW_HEIGHT;
  const padBottom = (filteredRows.length - endIdx) * ROW_HEIGHT;

  return (
    <section className="page-shell today-page">
      <TodayOverview
        summary={summary}
        lastRefreshed={lastRefreshed}
      />

      <div className="today-layout">
        <aside className="today-sidebar">
          <div className="today-category-tabs">
            {CATEGORY_TABS.map(([key, label]) => (
              <button
                key={key}
                type="button"
                className={category === key ? "active" : ""}
                onClick={() => {
                  setCategory(key);
                  setSelectedTsCode(null);   // 切类别时让默认选中重选
                  setScrollTop(0);
                }}
              >
                {label}
              </button>
            ))}
          </div>

          <div className="today-filter">
            <Search size={13} />
            <input
              placeholder="按名称或代码筛选"
              value={filter}
              onChange={(event) => setFilter(event.target.value)}
            />
          </div>

          <div className="today-market-meta">
            <small>
              {instrumentsLoading
                ? "档案加载中…"
                : `共 ${filteredRows.length.toLocaleString()} 条`}
            </small>
          </div>

          <div className="today-sort-row">
            {SORT_TABS.map(([key, label]) => (
              <button
                key={key}
                type="button"
                className={sortKey === key ? "active" : ""}
                onClick={() => {
                  setSortKey(key);
                  setSelectedTsCode(null);
                  setScrollTop(0);
                }}
              >
                {label}
              </button>
            ))}
          </div>

          <div
            className="today-stock-list"
            onScroll={(e) => setScrollTop((e.target as HTMLDivElement).scrollTop)}
            ref={(el) => {
              if (el && el.clientHeight !== viewportH) {
                setViewportH(el.clientHeight);
              }
            }}
          >
            {filteredRows.length === 0 ? (
              <p className="muted">
                {instrumentsLoading ? "档案加载中…" : "没有匹配。"}
              </p>
            ) : (
              <>
                <div style={{ height: padTop }} />
                {visibleRows.map((inst) => (
                  <Row
                    key={inst.tsCode}
                    inst={inst}
                    quote={quoteMap.get(inst.tsCode) ?? null}
                    active={selectedTsCode === inst.tsCode}
                    inWatchlist={watchedCodes.has(inst.code)}
                    onSelect={() => setSelectedTsCode(inst.tsCode)}
                    onContextMenu={(e) => {
                      e.preventDefault();
                      setRowMenu({
                        x: e.clientX,
                        y: e.clientY,
                        code: inst.code,
                        name: inst.name || inst.code,
                        inWatchlist: watchedCodes.has(inst.code),
                      });
                    }}
                  />
                ))}
                <div style={{ height: padBottom }} />
              </>
            )}
          </div>
        </aside>

        <div className="today-main">
          {selectedInstrument ? (
            <KlineChart
              code={selectedInstrument.code}
              tsCode={selectedInstrument.tsCode}
              name={selectedInstrument.name}
              category={selectedInstrument.category}
              meta={
                selectedQuote
                  ? {
                      price: selectedQuote.price,
                      changePercent: selectedQuote.changePercent,
                      amount: selectedQuote.amount,
                      low: selectedQuote.low,
                      high: selectedQuote.high,
                    }
                  : undefined
              }
            />
          ) : (
            <div className="today-empty">
              <p>从左侧选一条查看 K 线和详情。</p>
            </div>
          )}
        </div>
      </div>

      {rowMenu && (
        <div
          className="today-row-menu"
          style={{ top: rowMenu.y, left: rowMenu.x }}
          onClick={(e) => e.stopPropagation()}
          onContextMenu={(e) => e.preventDefault()}
          role="menu"
        >
          <button
            type="button"
            onClick={() => {
              if (rowMenu.inWatchlist) {
                void removeWatchlist(rowMenu.code);
              } else {
                void addWatchlist(rowMenu.code);
              }
              setRowMenu(null);
            }}
          >
            {rowMenu.inWatchlist ? (
              <>
                <StarOff size={14} />
                <span>从自选移除</span>
              </>
            ) : (
              <>
                <Star size={14} />
                <span>加入自选</span>
              </>
            )}
            <small>{rowMenu.name}</small>
          </button>
        </div>
      )}
    </section>
  );
}

type MarketSummary = {
  indices: Array<{
    tsCode: string;
    code: string;
    name: string;
    quote: MarketQuote | null;
  }>;
  breadth: {
    rise: number;
    fall: number;
    flat: number;
    covered: number;
    totalStocks: number;
  };
  sectors: Array<{
    name: string;
    count: number;
    avgChangePercent: number;
    rise: number;
    fall: number;
  }>;
  latestCapturedAt: number | null;
};

function TodayOverview({
  summary,
  lastRefreshed,
}: {
  summary: MarketSummary;
  lastRefreshed: string | null;
}) {
  const { breadth } = summary;
  const breadthTotal = Math.max(1, breadth.covered);
  const riseWidth = `${Math.round((breadth.rise / breadthTotal) * 100)}%`;
  const fallWidth = `${Math.round((breadth.fall / breadthTotal) * 100)}%`;
  const refreshLabel =
    lastRefreshed ??
    (summary.latestCapturedAt ? new Date(summary.latestCapturedAt).toISOString() : null);

  return (
    <section className="today-overview-panel">
      <div className="today-overview-head">
        <div>
          <h2>今日市场</h2>
          <p>
            {refreshLabel
              ? `行情快照 ${formatRefreshTime(refreshLabel)}`
            : "等待行情快照"}
          </p>
        </div>
      </div>

      <div className="today-overview-grid">
        {summary.indices.map((index) => (
          <article className="today-index-card" key={index.tsCode}>
            <small>{index.name}</small>
            <strong>{formatPrice(index.quote?.price ?? null)}</strong>
            <em className={quoteTone(index.quote?.changePercent ?? null)}>
              {formatPercent(index.quote?.changePercent ?? null)}
            </em>
          </article>
        ))}

        <article className="today-breadth-card">
          <small>市场宽度</small>
          <strong>
            {breadth.rise.toLocaleString()} / {breadth.fall.toLocaleString()}
          </strong>
          <div className="today-breadth-bar" aria-hidden="true">
            <span className="rise" style={{ width: riseWidth }} />
            <span className="fall" style={{ width: fallWidth }} />
          </div>
          <p>
            平盘 {breadth.flat.toLocaleString()} · 覆盖{" "}
            {breadth.covered.toLocaleString()} / {breadth.totalStocks.toLocaleString()}
          </p>
        </article>

        <article className="today-sector-card">
          <div className="today-sector-title">
            <small>热门板块</small>
          </div>
          {summary.sectors.length === 0 ? (
            <p className="today-sector-empty">等待全市场行情</p>
          ) : (
            <div className="today-sector-list">
              {summary.sectors.map((sector) => (
                <div className="today-sector-row" key={sector.name}>
                  <span>{sector.name}</span>
                  <em className={quoteTone(sector.avgChangePercent)}>
                    {formatPercent(sector.avgChangePercent)}
                  </em>
                </div>
              ))}
            </div>
          )}
        </article>
      </div>
    </section>
  );
}

function Row({
  inst,
  quote,
  active,
  inWatchlist,
  onSelect,
  onContextMenu,
}: {
  inst: MarketInstrument;
  quote: MarketQuote | null;
  active: boolean;
  inWatchlist: boolean;
  onSelect: () => void;
  onContextMenu: (e: React.MouseEvent) => void;
}) {
  const tone = quoteTone(quote?.changePercent ?? null);
  return (
    <button
      type="button"
      className={`today-stock-row tone-${tone}${active ? " active" : ""}`}
      onClick={onSelect}
      onContextMenu={onContextMenu}
      style={{ height: ROW_HEIGHT - 4 }}
    >
      <span className="today-stock-bar" aria-hidden="true" />
      <div className="today-stock-name">
        <strong>
          {inst.name || inst.code}
          {inWatchlist && (
            <Star
              size={11}
              strokeWidth={2.5}
              className="today-stock-watch"
              aria-label="已加入自选"
            />
          )}
        </strong>
        <small>
          {inst.code}
          {inst.category !== "stock" && (
            <span className={`today-stock-badge ${inst.category}`}>
              {inst.category === "index" ? "指" : "基"}
            </span>
          )}
        </small>
      </div>
      <div className="today-stock-price">
        <span className="price">{formatPrice(quote?.price ?? null)}</span>
        <div className="change-row">
          <em className={`change-abs ${tone}`}>
            {formatChange(quote?.change ?? null)}
          </em>
          <em className={`change-pct ${tone}`}>
            {formatPercent(quote?.changePercent ?? null)}
          </em>
        </div>
      </div>
    </button>
  );
}

function buildMarketSummary(
  instruments: MarketInstrument[],
  quoteMap: Map<string, MarketQuote>,
): MarketSummary {
  const instrumentMap = new Map(instruments.map((inst) => [inst.tsCode, inst]));
  const indices = CORE_INDICES.map((index) => {
    const inst = instrumentMap.get(index.tsCode);
    return {
      tsCode: index.tsCode,
      code: inst?.code ?? index.tsCode.slice(0, 6),
      name: inst?.name || index.name,
      quote: quoteMap.get(index.tsCode) ?? null,
    };
  });

  let rise = 0;
  let fall = 0;
  let flat = 0;
  let covered = 0;
  let totalStocks = 0;
  let latestCapturedAt: number | null = null;
  const sectorAgg = new Map<
    string,
    { sum: number; count: number; rise: number; fall: number }
  >();

  for (const quote of quoteMap.values()) {
    if (isFiniteNumber(quote.capturedAt)) {
      latestCapturedAt = Math.max(latestCapturedAt ?? 0, quote.capturedAt);
    }
  }

  for (const inst of instruments) {
    if (inst.category !== "stock") continue;
    totalStocks += 1;
    const pct = quoteMap.get(inst.tsCode)?.changePercent;
    if (!isFiniteNumber(pct)) continue;

    covered += 1;
    if (pct > 0.0001) rise += 1;
    else if (pct < -0.0001) fall += 1;
    else flat += 1;

    const sector = inst.sector?.trim();
    if (!sector) continue;
    const next = sectorAgg.get(sector) ?? { sum: 0, count: 0, rise: 0, fall: 0 };
    next.sum += pct;
    next.count += 1;
    if (pct > 0.0001) next.rise += 1;
    if (pct < -0.0001) next.fall += 1;
    sectorAgg.set(sector, next);
  }

  const sectors = Array.from(sectorAgg.entries())
    .map(([name, stats]) => ({
      name,
      count: stats.count,
      avgChangePercent: stats.sum / Math.max(1, stats.count),
      rise: stats.rise,
      fall: stats.fall,
    }))
    .sort((a, b) => b.avgChangePercent - a.avgChangePercent)
    .slice(0, 5);

  return {
    indices,
    breadth: { rise, fall, flat, covered, totalStocks },
    sectors,
    latestCapturedAt,
  };
}

function formatPrice(value: number | null): string {
  if (!isFiniteNumber(value)) return "—";
  return value.toFixed(value < 10 ? 3 : 2);
}

function formatPercent(value: number | null): string {
  if (!isFiniteNumber(value)) return "—";
  const sign = value > 0 ? "+" : "";
  return `${sign}${value.toFixed(2)}%`;
}

/// 涨跌额——带符号、最多 2 位小数（高价位股不至于占太宽）。
function formatChange(value: number | null): string {
  if (!isFiniteNumber(value)) return "—";
  const abs = Math.abs(value);
  const digits = abs < 10 ? 3 : 2;
  const sign = value > 0 ? "+" : value < 0 ? "" : "";
  return `${sign}${value.toFixed(digits)}`;
}

function quoteTone(value: number | null): "up" | "down" | "flat" {
  if (!isFiniteNumber(value)) return "flat";
  if (value > 0.0001) return "up";
  if (value < -0.0001) return "down";
  return "flat";
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function formatRefreshTime(iso: string): string {
  try {
    const d = new Date(iso);
    return `${d.getHours().toString().padStart(2, "0")}:${d.getMinutes().toString().padStart(2, "0")}:${d.getSeconds().toString().padStart(2, "0")}`;
  } catch {
    return iso;
  }
}
