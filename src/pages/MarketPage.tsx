// MarketPage — 市场页主结构。
//
// Spec: docs/design/frontend-design.md §4 + docs/design/quotes-module.md §4
//
// 结构：
//   PageShell
//     control strip：tab 切换 (stock/index/fund) + 搜索 + 分页 + 刷新
//     workspace：左 70% 列表 + 右 30% 详情面板
//
// 数据流：
//   - listMarket({ category, includeQuote: true, limit, offset })
//   - 选中行 → 右侧 InstrumentDetail
//   - 加自选 → updateWatchlist({ add: [tsCode] })，本地维护已加入集合

import { ChevronLeft, ChevronRight, RefreshCcw, Search } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { PageShell } from "../components/PageShell";
import { MarketHeader } from "./market/MarketHeader";
import { InstrumentDetail } from "./market/InstrumentDetail";
import { MarketList, type SortDir, type SortKey } from "./market/MarketList";
import {
  commands,
  type InstrumentCategory,
  type ListMarketItem,
  type ListMarketPage,
  type TsCode,
} from "../bindings";

const TABS: { value: InstrumentCategory; label: string }[] = [
  { value: "stock", label: "股票" },
  { value: "index", label: "指数" },
  { value: "fund", label: "基金" },
];

const PAGE_SIZE = 50;

function formatTime(date: Date): string {
  return date.toLocaleTimeString("zh-CN", { hour12: false });
}

export default function MarketPage() {
  const [category, setCategory] = useState<InstrumentCategory>("stock");
  const [query, setQuery] = useState("");
  const [queryInput, setQueryInput] = useState("");
  const [offset, setOffset] = useState(0);
  const [items, setItems] = useState<ListMarketItem[]>([]);
  const [page, setPage] = useState<ListMarketPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null);
  const [selected, setSelected] = useState<TsCode | null>(null);
  const [starred, setStarred] = useState<Set<TsCode>>(new Set());
  const [sortKey, setSortKey] = useState<SortKey>("amount");
  const [sortDir, setSortDir] = useState<SortDir>("desc");
  const [refreshTick, setRefreshTick] = useState(0);

  // Pull list
  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    void commands
      .listMarket({
        category,
        query: query || undefined,
        includeQuote: true,
        limit: PAGE_SIZE,
        offset,
      })
      .then((res) => {
        if (cancelled) return;
        if (res.status === "error") {
          setError(
            `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
          );
          setLoading(false);
          return;
        }
        setItems(res.data.items);
        setPage(res.data.page);
        setLastUpdated(new Date());
        setLoading(false);
        // 自动选中第一行（如果当前 selection 不在新列表中）
        if (res.data.items.length > 0) {
          const exists = res.data.items.some((it) => it.tsCode === selected);
          if (!exists) setSelected(res.data.items[0].tsCode);
        } else {
          setSelected(null);
        }
      });
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [category, query, offset, refreshTick]);

  const handleSort = useCallback((key: SortKey) => {
    setSortKey((prevKey) => {
      if (prevKey === key) {
        setSortDir((d) => (d === "desc" ? "asc" : "desc"));
        return key;
      }
      setSortDir("desc");
      return key;
    });
  }, []);

  const handleToggleStar = useCallback((tsCode: TsCode) => {
    setStarred((prev) => {
      const next = new Set(prev);
      const isAdd = !next.has(tsCode);
      if (isAdd) next.add(tsCode);
      else next.delete(tsCode);
      void commands
        .updateWatchlist(isAdd ? { add: [tsCode] } : { remove: [tsCode] })
        .then((res) => {
          if (res.status === "error") {
            // 后端失败：回滚本地状态，但不阻断 UI
            // eslint-disable-next-line no-console
            console.error(
              "updateWatchlist failed:",
              res.error.code,
              res.error.message,
            );
            setStarred((cur) => {
              const r = new Set(cur);
              if (isAdd) r.delete(tsCode);
              else r.add(tsCode);
              return r;
            });
          }
        });
      return next;
    });
  }, []);

  const handleSearch = useCallback(() => {
    setOffset(0);
    setQuery(queryInput.trim());
  }, [queryInput]);

  const handleTab = useCallback((cat: InstrumentCategory) => {
    setCategory(cat);
    setOffset(0);
    setQuery("");
    setQueryInput("");
  }, []);

  const handleRefresh = useCallback(() => {
    setRefreshTick((t) => t + 1);
  }, []);

  const selectedItem = useMemo(
    () => items.find((it) => it.tsCode === selected) ?? null,
    [items, selected],
  );

  const status = error
    ? `加载失败：${error}`
    : loading
      ? "加载中"
      : lastUpdated
        ? `已更新 ${formatTime(lastUpdated)}`
        : "等待数据";
  const statusTone = error
    ? "error"
    : loading
      ? "loading"
      : lastUpdated
        ? "ok"
        : "stale";

  const controls = (
    <>
      <div className="segmented" role="tablist">
        {TABS.map((t) => (
          <button
            key={t.value}
            type="button"
            role="tab"
            aria-selected={category === t.value}
            className={`segmented-item ${category === t.value ? "active" : ""}`}
            onClick={() => handleTab(t.value)}
          >
            {t.label}
          </button>
        ))}
      </div>

      <div className="search-input">
        <Search size={14} className="search-icon" />
        <input
          type="search"
          placeholder="搜索代码 / 名称"
          value={queryInput}
          onChange={(e) => setQueryInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") handleSearch();
          }}
        />
      </div>
      <button className="btn" type="button" onClick={handleSearch}>
        搜索
      </button>

      <div className="control-spacer" />

      <div className="page-controls">
        <button
          type="button"
          className="btn ghost"
          disabled={offset === 0 || loading}
          onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}
        >
          <ChevronLeft size={14} />
        </button>
        <span className="muted tabular" style={{ fontSize: 12 }}>
          {offset + 1}–{offset + items.length}
          {page?.hasMore ? " · 更多" : ""}
        </span>
        <button
          type="button"
          className="btn ghost"
          disabled={!page?.hasMore || loading}
          onClick={() => setOffset(offset + PAGE_SIZE)}
        >
          <ChevronRight size={14} />
        </button>
      </div>

      <button
        type="button"
        className="btn"
        onClick={handleRefresh}
        disabled={loading}
        title="刷新"
      >
        <RefreshCcw size={14} />
      </button>
    </>
  );

  return (
    <PageShell
      title="市场"
      status={status}
      statusTone={statusTone}
      meta={<MarketHeader />}
      controls={controls}
    >
      <div className="market-workspace">
        <div className="market-workspace-list">
          {error ? (
            <div className="market-error">
              <div>{error}</div>
              <button className="btn" type="button" onClick={handleRefresh}>
                重试
              </button>
            </div>
          ) : (
            <MarketList
              items={items}
              selected={selected}
              onSelect={setSelected}
              sortKey={sortKey}
              sortDir={sortDir}
              onSort={handleSort}
              starred={starred}
              onToggleStar={handleToggleStar}
              loading={loading}
            />
          )}
        </div>
        <div className="market-workspace-detail">
          <InstrumentDetail item={selectedItem} />
        </div>
      </div>
    </PageShell>
  );
}
