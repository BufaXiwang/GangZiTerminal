// MarketPage — 市场页（紧凑版，控制条下移到列表头部）。
//
// Spec: docs/design/frontend-design.md §4 + docs/design/quotes-module.md §4
//
// 结构：
//   PageShell（顶部仅 标题 + lastUpdated + 全局刷新）
//     metrics row：4 指数卡片 + 市场宽度 + 行业热度
//     workspace：
//       list panel（自包含控制条）：
//         header：
//           row1: sort tabs（默认/涨跌/成交）  + category chips（股票/指数/基金）
//           row2: 搜索 input
//           row3: 6 位代码 + 添加自选
//         body：列表（不分页；listMarket limit 大值一次性拉到）
//       detail panel：精简 header + K 线为主
//
// 默认选中 `000001.SH`（上证指数）—— 这只有 K 线数据（K-line warmup 已覆盖核心指数）。
// 选中非核心标的会显示"暂无 K 线数据"（后续加 on-demand refresh）。

import { Search } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { useWatchlistStore } from "../lib/watchlistStore";
import { InstrumentDetail } from "./market/InstrumentDetail";
import { MarketList, type SortDir, type SortKey } from "./market/MarketList";
import { MarketMetricsRow } from "./market/MarketMetricsRow";
import { RowContextMenu } from "./market/RowContextMenu";
import { CORE_INDEXES } from "../lib/useCoreIndexes";
import {
  getCachedList,
  setCachedList,
  invalidateAllListCache,
} from "../lib/marketListCache";
import { perf } from "../lib/perfLog";
import {
  commands,
  type InstrumentCategory,
  type ListMarketItem,
  type TsCode,
} from "../bindings";

interface TabDef {
  key: SortKey;
  label: string;
}

const SORT_TABS: TabDef[] = [
  { key: "changePercent", label: "涨跌" },
  { key: "amount", label: "成交" },
];

const CATEGORY_OPTIONS: { value: InstrumentCategory; label: string }[] = [
  { value: "stock", label: "股票" },
  { value: "index", label: "指数" },
  { value: "fund", label: "基金" },
];

// 不分页：一次性拉到（universe ~7500，listMarket 是 in-memory + DB 读，不会爆）
const LIST_LIMIT = 10000;

const DEFAULT_SELECTED: TsCode = "000001.SH";

function formatTime(date: Date): string {
  return date.toLocaleTimeString("zh-CN", { hour12: false });
}

export default function MarketPage() {
  perf(`MarketPage render start ${performance.now().toFixed(1)}`);
  const [category, setCategory] = useState<InstrumentCategory>("stock");
  const [query, setQuery] = useState("");
  const [queryInput, setQueryInput] = useState("");
  const [items, setItems] = useState<ListMarketItem[]>([]);
  const [menuState, setMenuState] = useState<{ tsCode: TsCode; x: number; y: number } | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null);
  const [selected, setSelected] = useState<TsCode | null>(DEFAULT_SELECTED);
  const [externalItem, setExternalItem] = useState<ListMarketItem | null>(null);
  const starred = useWatchlistStore((s) => s.codes);
  const addWatch = useWatchlistStore((s) => s.add);
  const removeWatch = useWatchlistStore((s) => s.remove);
  // 默认按成交量降序排（amount desc）
  const [sortKey, setSortKey] = useState<SortKey>("amount");
  const [sortDir] = useState<SortDir>("desc");
  const [refreshTick, setRefreshTick] = useState(0);
  // universe scope refresh 进度事件触发的 throttle refetch（spec §5 全市场刷新执行契约）
  // Leading-edge：第一个事件立即 refetch，之后至少 500ms 间隔，让冷启动期间 UI 持续流式填充。
  const lastProgressRefetchRef = useRef<number>(0);
  const pendingProgressTimerRef = useRef<number | null>(null);

  useEffect(() => {
    perf(`MarketPage mount at ${performance.now().toFixed(1)}`);
    return () => {
      perf(`MarketPage unmount at ${performance.now().toFixed(1)}`);
    };
  }, []);

  // 订阅 universe refresh progress event — pipeline 每 80 只 emit 一次（一轮 ~94 次）。
  // Throttle 2.5s：冷启动仍每 2.5s 增量填充（比 25s 空窗好得多）；稳态下一轮刷新
  // 只重拉 ~6 次而非 ~30 次，避免整列表高频重渲染拖慢交互。
  // 终态由 trailing-edge timer 兜底，保证最后一拨数据也刷到。
  useEffect(() => {
    const THROTTLE_MS = 2500;
    let unlisten: (() => void) | null = null;
    const doRefetch = () => {
      lastProgressRefetchRef.current = Date.now();
      invalidateAllListCache();
      setRefreshTick((t) => t + 1);
    };
    void listen<unknown>("market-quotes-refresh-progress", (event) => {
      const p = (event.payload as { payload?: { completed?: number; success?: number; total?: number } } | undefined)?.payload;
      perf(
        `progress event completed=${p?.completed ?? "?"} success=${p?.success ?? "?"} total=${p?.total ?? "?"}`,
      );
      const elapsed = Date.now() - lastProgressRefetchRef.current;
      if (elapsed >= THROTTLE_MS) {
        // leading edge：立即刷
        doRefetch();
      } else if (pendingProgressTimerRef.current == null) {
        // trailing edge：保证最后一次也能 refetch
        pendingProgressTimerRef.current = window.setTimeout(() => {
          pendingProgressTimerRef.current = null;
          doRefetch();
        }, THROTTLE_MS - elapsed);
      }
    }).then((un) => {
      unlisten = un;
    });
    return () => {
      if (pendingProgressTimerRef.current != null) {
        window.clearTimeout(pendingProgressTimerRef.current);
      }
      unlisten?.();
    };
  }, []);

  // Pull list（不分页，一次性拉）— stale-while-revalidate 策略：
  //   1. cache 命中 → 立即渲染（瞬间）
  //   2. cache stale (>30s) → 后台静默 refetch，不显示 loading
  //   3. cache miss → 显示 loading + IPC fetch
  // 后端 listMarket 是纯本地 DB + snapshot 读，没有 TDX 调用；cache 安全。
  useEffect(() => {
    let cancelled = false;

    const doFetch = (silent: boolean) => {
      if (!silent) {
        setLoading(true);
        setError(null);
      }
      const t0 = performance.now();
      perf(`listMarket IPC start (silent=${silent})`);
      void commands
        .listMarket({
          category,
          query: query || undefined,
          includeQuote: true,
          limit: LIST_LIMIT,
          offset: 0,
        })
        .then((res) => {
          perf(
            `listMarket IPC done in ${(performance.now() - t0).toFixed(1)}ms (items=${res.status === "ok" ? res.data.items.length : "err"})`,
          );
          if (cancelled) return;
          if (res.status === "error") {
            if (!silent) {
              setError(
                `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
              );
              setLoading(false);
            }
            return;
          }
          setItems(res.data.items);
          setCachedList(category, query, res.data.items);
          setLastUpdated(new Date());
          if (!silent) setLoading(false);
        });
    };

    const cached = getCachedList(category, query);
    if (cached) {
      perf(
        `listMarket cache HIT items=${cached.items.length} age=${cached.ageMs}ms stale=${cached.stale}`,
      );
      setItems(cached.items);
      setLastUpdated(new Date(Date.now() - cached.ageMs));
      setLoading(false);
      setError(null);
      if (cached.stale) doFetch(true);
    } else {
      perf("listMarket cache MISS → IPC");
      doFetch(false);
    }

    return () => {
      cancelled = true;
    };
  }, [category, query, refreshTick]);

  // 声明热点集（spec §5 热点档）：核心指数 + 自选 + 成交额 top-80（可见列表头）。
  // 后端 3s tick 高频刷这批（≤120）。debounce 800ms；items 每次 progress 重拉会变，
  // 故约每 2.5s 推一次（命令很轻，只覆盖一个 Vec）。MarketPage keep-alive 常驻，
  // 切到别的 tab 也持续维护热点集，自选/指数始终高频。
  useEffect(() => {
    const top = [...items]
      .filter((it) => it.quote?.amount != null)
      .sort(
        (a, b) => (Number(b.quote?.amount) || 0) - (Number(a.quote?.amount) || 0),
      )
      .slice(0, 80)
      .map((it) => it.tsCode);
    const codes = Array.from(
      new Set([
        ...CORE_INDEXES.map((c) => c.tsCode),
        ...Array.from(starred),
        ...top,
      ]),
    ).slice(0, 120);
    const t = window.setTimeout(() => {
      void commands.setQuoteHotset(codes);
    }, 800);
    return () => window.clearTimeout(t);
  }, [items, starred]);

  // 选中核心指数时加载并**持续刷新**其 quote（externalItem 形式）。
  // 依赖 [selected, refreshTick]：refreshTick 在 universe progress 事件时 +1
  //（2.5s 节流），让指数详情头报价与顶部指数卡同源、同节奏刷新，避免详情头
  // 选中后冻结、与卡片显示不同价格。非核心指数 selected 时该 effect 早退，
  // 详情走 list item（已随 progress 刷新）。
  useEffect(() => {
    if (!selected) return;
    const info = CORE_INDEXES.find((c) => c.tsCode === selected);
    if (!info) return;
    let cancelled = false;
    void commands
      .fetchData({ tsCodes: [selected], include: { quote: true } })
      .then((res) => {
        if (cancelled) return;
        if (res.status === "ok" && res.data.items.length > 0) {
          setExternalItem(buildExternalItem(res.data.items[0], info.label));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [selected, refreshTick]);

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
    setQuery(queryInput.trim());
  }, [queryInput]);

  const handleRowContextMenu = useCallback(
    (tsCode: TsCode, x: number, y: number) => {
      setMenuState({ tsCode, x, y });
    },
    [],
  );

  const handleCategoryFilter = useCallback((cat: InstrumentCategory) => {
    setCategory(cat);
  }, []);

  const handleRefresh = useCallback(() => {
    invalidateAllListCache();
    setRefreshTick((t) => t + 1);
  }, []);

  // 选中指数：只 setSelected，quote 由上面的 [selected, refreshTick] effect 统一加载 + 刷新。
  const handleSelectIndex = useCallback((tsCode: string) => {
    setSelected(tsCode);
  }, []);

  const selectedItem = useMemo(() => {
    if (externalItem && externalItem.tsCode === selected) return externalItem;
    return items.find((it) => it.tsCode === selected) ?? null;
  }, [items, selected, externalItem]);

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

  // 不再用 PageShell 的 section-head；状态时间放进 MetricsRow 右上角小字。
  return (
    <section className="market-page-section">
      <div className="market-page">
        <MarketMetricsRow
          selected={selected}
          onSelectIndex={handleSelectIndex}
          statusText={status}
        />
        <div className="market-workspace">
          <div className="market-workspace-list">
            <div className="list-header">
              {/* 单行：左 category dropdown + 右 sort tab（涨跌 / 成交） */}
              <div className="list-header-row">
                <select
                  className="list-category-select"
                  value={category}
                  onChange={(e) =>
                    handleCategoryFilter(e.target.value as InstrumentCategory)
                  }
                >
                  {CATEGORY_OPTIONS.map((c) => (
                    <option key={c.value} value={c.value}>
                      {c.label}
                    </option>
                  ))}
                </select>
                <div className="list-sort-tabs" role="tablist">
                  {SORT_TABS.map((t) => (
                    <button
                      key={t.key}
                      type="button"
                      role="tab"
                      aria-selected={sortKey === t.key}
                      className={`list-sort-tab ${sortKey === t.key ? "active" : ""}`}
                      onClick={() => setSortKey(t.key)}
                    >
                      {t.label}
                    </button>
                  ))}
                </div>
              </div>
              <div className="list-header-row">
                <div className="list-search-input">
                  <Search size={13} className="search-icon" />
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
              </div>
            </div>
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
                onContextMenu={handleRowContextMenu}
                loading={loading}
              />
            )}
          </div>
          <div className="market-workspace-detail">
            <InstrumentDetail item={selectedItem} />
          </div>
        </div>
      </div>
      {menuState && (
        <RowContextMenu
          x={menuState.x}
          y={menuState.y}
          isStarred={starred.has(menuState.tsCode)}
          onToggleStar={() => handleToggleStar(menuState.tsCode)}
          onClose={() => setMenuState(null)}
        />
      )}
    </section>
  );
}

// 把 fetchData 的 FetchDataItem 转成 ListMarketItem 形状（detail 面板需要 ListMarketItem）。
// quote 字段是嵌套 record；价格相关用 number（detail 组件兼容 string/number）。
function buildExternalItem(fdItem: any, fallbackName: string): ListMarketItem {
  const tsCode = fdItem.tsCode as TsCode;
  return {
    tsCode,
    name: fdItem.name ?? fallbackName,
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
  } as ListMarketItem;
}
