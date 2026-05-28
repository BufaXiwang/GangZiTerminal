// MarketPage — 市场页（重设计版）。
//
// Spec: docs/design/frontend-design.md §4 + docs/design/quotes-module.md §4
//
// 结构：
//   PageShell
//     control strip：3 tab（默认/涨跌/成交） + 分类 filter chips（股票/指数/基金）
//                  + 搜索（query） + "添加自选" 6 位代码 input + 分页 + 刷新
//     workspace：
//       metrics row：4 指数卡片 + 市场宽度卡片 + 行业热度卡片
//       下方：左 list（紧凑双行）+ 右 detail（精简 header + K 线占主区）
//
// 数据流：
//   - listMarket({ category, includeQuote: true, limit, offset }) → 列表
//   - useCoreIndexes / useMarketBreadth / useIndustryHeatmap → 顶部指标
//   - 顶部 IndexCard 点击 → 自动添加并选中该 ts_code（如果不在当前列表，临时占位）

import { ChevronLeft, ChevronRight, Plus, RefreshCcw, Search } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { PageShell } from "../components/PageShell";
import { useWatchlistStore } from "../lib/watchlistStore";
import { InstrumentDetail } from "./market/InstrumentDetail";
import { MarketList, type SortDir, type SortKey } from "./market/MarketList";
import { MarketMetricsRow } from "./market/MarketMetricsRow";
import { CORE_INDEXES } from "../lib/useCoreIndexes";
import {
  commands,
  type InstrumentCategory,
  type ListMarketItem,
  type ListMarketPage,
  type TsCode,
} from "../bindings";

interface TabDef {
  key: SortKey;
  label: string;
}

const SORT_TABS: TabDef[] = [
  { key: "default", label: "默认" },
  { key: "changePercent", label: "涨跌" },
  { key: "amount", label: "成交" },
];

const CATEGORY_FILTERS: { value: InstrumentCategory; label: string }[] = [
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
  const [addCodeInput, setAddCodeInput] = useState("");
  const [offset, setOffset] = useState(0);
  const [items, setItems] = useState<ListMarketItem[]>([]);
  const [page, setPage] = useState<ListMarketPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null);
  const [selected, setSelected] = useState<TsCode | null>(null);
  const [externalItem, setExternalItem] = useState<ListMarketItem | null>(null);
  const starred = useWatchlistStore((s) => s.codes);
  const addWatch = useWatchlistStore((s) => s.add);
  const removeWatch = useWatchlistStore((s) => s.remove);
  const [sortKey, setSortKey] = useState<SortKey>("default");
  const [sortDir] = useState<SortDir>("desc");
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
        // 自动选第一行（如果当前 selection 不在新列表中且没有 externalItem）
        if (res.data.items.length > 0) {
          const exists = res.data.items.some((it) => it.tsCode === selected);
          if (!exists && !externalItem) {
            setSelected(res.data.items[0].tsCode);
          }
        }
      });
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [category, query, offset, refreshTick]);

  const handleToggleStar = useCallback(
    (tsCode: TsCode) => {
      if (starred.has(tsCode)) {
        void removeWatch(tsCode);
      } else {
        void addWatch(tsCode);
      }
    },
    [starred, addWatch, removeWatch],
  );

  const handleSearch = useCallback(() => {
    setOffset(0);
    setQuery(queryInput.trim());
  }, [queryInput]);

  const handleAddCode = useCallback(async () => {
    const code = addCodeInput.trim();
    if (!code) return;
    // 简单校验：6 位数字（用户体验上接受 sh/sz 前缀，由后端规范化）
    if (!/^\d{6}$/.test(code) && !/^\d{6}\.(SH|SZ|BJ)$/i.test(code)) {
      // 静默忽略；spec 没规定错误提示样式
      return;
    }
    // 通过 listMarket query 反查 → 取第一条 → addWatch
    const res = await commands.listMarket({
      query: code,
      includeQuote: false,
      limit: 5,
      offset: 0,
    });
    if (res.status === "ok" && res.data.items.length > 0) {
      const found = res.data.items[0];
      await addWatch(found.tsCode);
      setAddCodeInput("");
    }
  }, [addCodeInput, addWatch]);

  const handleCategoryFilter = useCallback((cat: InstrumentCategory) => {
    setCategory(cat);
    setOffset(0);
  }, []);

  const handleRefresh = useCallback(() => {
    setRefreshTick((t) => t + 1);
  }, []);

  // 顶部 IndexCard 点击 → 切到指数分类 + 选中
  const handleSelectIndex = useCallback(
    async (tsCode: string) => {
      setSelected(tsCode);
      // 如不在 items 里，主动 fetch_data 拉一份 quote 以填充 detail
      const info = CORE_INDEXES.find((c) => c.tsCode === tsCode);
      if (!info) return;
      const res = await commands.fetchData({
        tsCodes: [tsCode],
        include: { quote: true },
      });
      if (res.status === "ok" && res.data.items.length > 0) {
        const fdItem = res.data.items[0];
        // 拼一个 ListMarketItem-shape 给 detail 用
        setExternalItem({
          tsCode: fdItem.tsCode,
          name: fdItem.name ?? info.label,
          category: "index",
          market: tsCode.endsWith(".SH") ? "SH" : tsCode.endsWith(".SZ") ? "SZ" : "BJ",
          source: "tdx",
          updatedAt: new Date().toISOString(),
          quote: fdItem.quote
            ? {
                tradeDate: fdItem.quote.tradeDate ?? null,
                price: fdItem.quote.price ? Number(fdItem.quote.price) : null,
                change: fdItem.quote.change ? Number(fdItem.quote.change) : null,
                changePercent: fdItem.quote.changePercent ?? null,
                open: fdItem.quote.open ? Number(fdItem.quote.open) : null,
                high: fdItem.quote.high ? Number(fdItem.quote.high) : null,
                low: fdItem.quote.low ? Number(fdItem.quote.low) : null,
                previousClose: fdItem.quote.previousClose
                  ? Number(fdItem.quote.previousClose)
                  : null,
                volume: fdItem.quote.volume ?? null,
                amount: fdItem.quote.amount ? Number(fdItem.quote.amount) : null,
              }
            : null,
        } as ListMarketItem);
      }
    },
    [],
  );

  // selected → 找具体 item：先从 externalItem 命中，再从 items 找
  const selectedItem = useMemo(() => {
    if (externalItem && externalItem.tsCode === selected) return externalItem;
    return items.find((it) => it.tsCode === selected) ?? null;
  }, [items, selected, externalItem]);

  // 选中行不是外部 item 时清空 externalItem
  useEffect(() => {
    if (externalItem && selected !== externalItem.tsCode) {
      setExternalItem(null);
    }
  }, [selected, externalItem]);

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
        {SORT_TABS.map((t) => (
          <button
            key={t.key}
            type="button"
            role="tab"
            aria-selected={sortKey === t.key}
            className={`segmented-item ${sortKey === t.key ? "active" : ""}`}
            onClick={() => setSortKey(t.key)}
          >
            {t.label}
          </button>
        ))}
      </div>

      <div className="category-filter-chips">
        {CATEGORY_FILTERS.map((c) => (
          <button
            key={c.value}
            type="button"
            className={`chip filter-chip ${category === c.value ? "active" : ""}`}
            onClick={() => handleCategoryFilter(c.value)}
          >
            {c.label}
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

      <div className="search-input add-code-input">
        <input
          type="text"
          inputMode="numeric"
          maxLength={9}
          placeholder="6 位代码加自选"
          value={addCodeInput}
          onChange={(e) => setAddCodeInput(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") void handleAddCode();
          }}
        />
        <button
          type="button"
          className="btn ghost btn-add-code"
          onClick={() => void handleAddCode()}
          title="加入自选"
          aria-label="加入自选"
        >
          <Plus size={14} />
        </button>
      </div>

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
      controls={controls}
    >
      <div className="market-page">
        <MarketMetricsRow selected={selected} onSelectIndex={handleSelectIndex} />
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
      </div>
    </PageShell>
  );
}
