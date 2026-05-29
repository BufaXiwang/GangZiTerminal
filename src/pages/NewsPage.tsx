// NewsPage — 资讯页主结构。
//
// Spec: docs/design/frontend-design.md §4 资讯页 + docs/design/news-module.md §4 §5
//
// 结构：
//   PageShell
//     control strip：source 多选 chip + FTS 搜索 + 刷新
//   ─ 顶部 横向 日期 nav（最近 14 天）
//   ─ workspace：纵向时间线（按日分组，倒序，滚动加载更多）+ 右侧 ArticleDrawer
//
// 数据流：
//   - mount: listNewsSources() + fetchNews({ limit, offset: 0 })
//   - query / sources / refresh 变化 → 清空 items + 重拉
//   - 滚动到底 → fetchNews({ offset: prev + limit }) 拼接
//   - 点条目 → 打开 ArticleDrawer，drawer 内决定是否 warm + 重读 includeArticle

import { RefreshCcw, Search, X } from "lucide-react";
import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { PageShell } from "../components/PageShell";
import {
  commands,
  type FetchNewsItem,
  type FetchNewsPage,
  type NewsSource,
} from "../bindings";
import { NewsDateNav, formatDateKey } from "./news/NewsDateNav";
import { NewsTimeline } from "./news/NewsTimeline";
import { ArticleDrawer } from "./news/ArticleDrawer";

const PAGE_SIZE = 50;
const SEARCH_DEBOUNCE_MS = 300;

function formatTime(date: Date): string {
  return date.toLocaleTimeString("zh-CN", { hour12: false });
}

export default function NewsPage() {
  // === filter state ===
  const [queryInput, setQueryInput] = useState("");
  const [query, setQuery] = useState("");
  const [sources, setSources] = useState<NewsSource[]>([]);
  /** 选中的 sourceId（subset）；undefined / empty 表示"全选"（不传 sources）。 */
  const [selectedSources, setSelectedSources] = useState<Set<string>>(new Set());
  const [refreshTick, setRefreshTick] = useState(0);

  // === list state ===
  const [items, setItems] = useState<FetchNewsItem[]>([]);
  const [page, setPage] = useState<FetchNewsPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [warnings, setWarnings] = useState<string[]>([]);
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null);

  // === drawer state ===
  const [selectedId, setSelectedId] = useState<string | null>(null);

  // === date nav 当前激活日期（由 timeline 上报当前 viewport 顶部那一天） ===
  const [activeDate, setActiveDate] = useState<string | null>(null);
  const sectionRefs = useRef<Map<string, HTMLElement>>(new Map());

  // === debounced query input → query ===
  useEffect(() => {
    const handle = setTimeout(() => {
      setQuery(queryInput.trim());
    }, SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(handle);
  }, [queryInput]);

  // === load sources once ===
  useEffect(() => {
    void commands.listNewsSources().then((res) => {
      if (res.status === "ok") {
        setSources(res.data.items);
      } else {
        // eslint-disable-next-line no-console
        console.warn("listNewsSources failed:", res.error.code, res.error.message);
      }
    });
  }, []);

  // === reset list when filters / refresh change, then fetch first page ===
  const fetchPage = useCallback(
    async (offset: number) => {
      const sourceArr = selectedSources.size > 0 ? Array.from(selectedSources) : undefined;
      return commands.fetchNews({
        query: query.length > 0 ? query : undefined,
        sources: sourceArr,
        includeArticle: false,
        limit: PAGE_SIZE,
        offset,
      });
    },
    [query, selectedSources],
  );

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    setWarnings([]);
    void fetchPage(0).then((res) => {
      if (cancelled) return;
      if (res.status === "error") {
        setError(
          `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
        );
        setItems([]);
        setPage(null);
        setLoading(false);
        return;
      }
      setItems(res.data.items);
      setPage(res.data.page);
      if (res.data.errors && res.data.errors.length > 0) {
        setWarnings(
          res.data.errors.map((e) =>
            `${e.code}${e.field ? ` (${e.field})` : ""}${e.message ? `: ${e.message}` : ""}`,
          ),
        );
      }
      setLastUpdated(new Date());
      setLoading(false);
    });
    return () => {
      cancelled = true;
    };
  }, [fetchPage, refreshTick]);

  // === load more (append) ===
  const handleLoadMore = useCallback(async () => {
    if (!page || !page.hasMore || loadingMore || loading) return;
    setLoadingMore(true);
    const nextOffset = page.offset + page.limit;
    const res = await fetchPage(nextOffset);
    if (res.status === "ok") {
      setItems((prev) => {
        const seen = new Set(prev.map((x) => x.id));
        const newOnes = res.data.items.filter((x) => !seen.has(x.id));
        return [...prev, ...newOnes];
      });
      setPage(res.data.page);
    } else {
      setError(
        `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
      );
    }
    setLoadingMore(false);
  }, [fetchPage, loading, loadingMore, page]);

  const handleRefresh = useCallback(() => {
    setRefreshTick((t) => t + 1);
  }, []);

  const handleToggleSource = useCallback((sourceId: string) => {
    setSelectedSources((prev) => {
      const next = new Set(prev);
      if (next.has(sourceId)) next.delete(sourceId);
      else next.add(sourceId);
      return next;
    });
  }, []);

  const handleSelectDate = useCallback((dateKey: string) => {
    const el = sectionRefs.current.get(dateKey);
    if (el) {
      el.scrollIntoView({ behavior: "smooth", block: "start" });
      setActiveDate(dateKey);
    }
  }, []);

  const handleRegisterSectionRef = useCallback(
    (dateKey: string, el: HTMLElement | null) => {
      if (el) sectionRefs.current.set(dateKey, el);
      else sectionRefs.current.delete(dateKey);
    },
    [],
  );

  // === sourceId → displayName map，timeline row 友好显示来源名 ===
  const sourceNames = useMemo(() => {
    const m: Record<string, string> = {};
    for (const s of sources) {
      if (s.displayName) m[s.sourceId] = s.displayName;
    }
    return m;
  }, [sources]);

  // === counts by date 给 nav 用 ===
  const countsByDate = useMemo(() => {
    const m: Record<string, number> = {};
    for (const it of items) {
      if (!it.publishedAt) continue;
      const d = new Date(it.publishedAt);
      if (Number.isNaN(d.getTime())) continue;
      const key = formatDateKey(d);
      m[key] = (m[key] ?? 0) + 1;
    }
    return m;
  }, [items]);

  // === status line ===
  const enabledCount = sources.filter((s) => s.enabled).length;
  const totalCount = sources.length;
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

  const meta = (
    <span className="news-meta">
      {totalCount > 0 && (
        <span className="muted">
          {enabledCount}/{totalCount} 来源启用
        </span>
      )}
      {query.length > 0 ? (
        <span className="muted">
          · 匹配 {items.length} 条
          {page?.hasMore ? "+" : ""} (query: "{query}")
        </span>
      ) : (
        items.length > 0 && (
          <span className="muted">
            · 当前 {items.length} 条
            {page?.hasMore ? "+（可加载更多）" : ""}
          </span>
        )
      )}
      {selectedSources.size > 0 && (
        <span className="muted">· {selectedSources.size} 来源过滤中</span>
      )}
      {warnings.length > 0 && (
        <span className="news-warning-pill" title={warnings.join("\n")}>
          ! 部分错误 ({warnings.length})
        </span>
      )}
    </span>
  );

  const selectedItem = useMemo(
    () => items.find((it) => it.id === selectedId) ?? null,
    [items, selectedId],
  );

  const controls = (
    <>
      <div className="search-input" style={{ minWidth: 280 }}>
        <Search size={14} className="search-icon" />
        <input
          type="search"
          placeholder="搜索资讯标题 / 摘要 / 正文…"
          value={queryInput}
          onChange={(e) => setQueryInput(e.target.value)}
        />
        {queryInput.length > 0 && (
          <button
            type="button"
            className="btn ghost"
            style={{ height: 22, padding: "0 4px" }}
            onClick={() => setQueryInput("")}
            aria-label="清除搜索"
          >
            <X size={12} />
          </button>
        )}
      </div>

      <div className="news-source-chips">
        {sources.length === 0 ? (
          <span className="muted" style={{ fontSize: 12 }}>
            (加载来源列表…)
          </span>
        ) : (
          sources.map((s) => {
            const id = s.sourceId;
            const active = selectedSources.has(id);
            const inFilter = selectedSources.size === 0 || active;
            return (
              <button
                key={id}
                type="button"
                className={`chip news-source-chip ${active ? "active" : ""} ${inFilter ? "" : "dimmed"}`}
                onClick={() => handleToggleSource(id)}
                disabled={!s.enabled}
                title={
                  s.lastError
                    ? `last_error: ${s.lastError.code}${s.lastError.message ? " — " + s.lastError.message : ""}`
                    : s.enabled
                      ? "点击切换过滤"
                      : "已禁用"
                }
              >
                {s.displayName ?? s.sourceId}
                {s.lastError && <span className="news-source-error-dot" />}
              </button>
            );
          })
        )}
        {selectedSources.size > 0 && (
          <button
            type="button"
            className="btn ghost"
            style={{ fontSize: 11, height: 24 }}
            onClick={() => setSelectedSources(new Set())}
          >
            清除
          </button>
        )}
      </div>

      <div className="control-spacer" />

      <button
        type="button"
        className="btn"
        onClick={handleRefresh}
        disabled={loading}
        title="重新拉取"
      >
        <RefreshCcw size={14} />
      </button>
    </>
  );

  return (
    <PageShell
      title="资讯"
      status={status}
      statusTone={statusTone}
      meta={meta}
      controls={controls}
    >
      <NewsDateNav
        countsByDate={countsByDate}
        activeDate={activeDate}
        onSelect={handleSelectDate}
      />

      <div className="news-workspace">
        <div className="news-workspace-main">
          {error ? (
            <div className="market-error">
              <div>{error}</div>
              <button className="btn" type="button" onClick={handleRefresh}>
                重试
              </button>
            </div>
          ) : (
            <NewsTimeline
              items={items}
              loading={loading}
              loadingMore={loadingMore}
              hasMore={page?.hasMore ?? false}
              onLoadMore={handleLoadMore}
              onSelectItem={setSelectedId}
              selectedId={selectedId}
              registerSectionRef={handleRegisterSectionRef}
              onActiveDateChange={setActiveDate}
              query={query}
              sourceNames={sourceNames}
            />
          )}
        </div>
      </div>

      <ArticleDrawer
        item={selectedItem}
        open={selectedId !== null}
        onClose={() => setSelectedId(null)}
        sourceNames={sourceNames}
        onItemUpdated={(updated) => {
          setItems((prev) =>
            prev.map((it) => (it.id === updated.id ? updated : it)),
          );
        }}
      />
    </PageShell>
  );
}
