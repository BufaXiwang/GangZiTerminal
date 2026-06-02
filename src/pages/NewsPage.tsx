// NewsPage — 资讯页主结构。
//
// Spec: docs/design/frontend-design.md §4 资讯页 + docs/design/news-module.md §4 §5
//
// 结构：
//   PageShell
//     control strip：source 多选 chip + FTS 搜索 + 刷新
//   ─ 顶部 横向 日期 nav（最近 14 天）
//   ─ workspace：纵向时间线（按日分组，倒序，滚动加载更多）；正文行内展示、>3 行可展开
//
// 数据流：
//   - mount: listNewsSources() + fetchNews({ limit, offset: 0 })
//   - query / sources / refresh 变化 → 清空 items + 重拉
//   - 滚动到底 → fetchNews({ offset: prev + limit }) 拼接
//   - 正文在刷新时按 source 策略同步抓取（无侧边 drawer）；行内点击展开/收起

import { Search, X } from "lucide-react";
import { listen } from "@tauri-apps/api/event";
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
  // 跳日期时在 async 循环里读最新 page / items，避免闭包拿到旧值。
  const pageRef = useRef<FetchNewsPage | null>(null);
  pageRef.current = page;
  const itemsRef = useRef<FetchNewsItem[]>([]);
  itemsRef.current = items;
  // 每日真实总数（后端 GROUP BY，不受分页限制）—— 给日期导航显示真实条数。
  const [dateCounts, setDateCounts] = useState<Record<string, number>>({});
  const [loading, setLoading] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [warnings, setWarnings] = useState<string[]>([]);
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null);

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
        // 带全文：行内展开要显示完整正文，不能只给 500 字的 articleExcerpt。
        includeArticle: true,
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
      // 日期导航用后端按日聚合的真实总数（不随分页累积）。
      {
        const m: Record<string, number> = {};
        for (const dc of res.data.dateCounts ?? []) m[dc.date] = dc.count;
        setDateCounts(m);
      }
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

  // === 后端 scheduler 刷新出新资讯时同步前端 ===
  // 后端每 ~60s run_refresh，若 savedCount>0 / articleUpdated>0 会 emit `news-refreshed`
  // (lib.rs → app.emit)。这里监听并**静默**拉第一页，把新条目并入列表顶部——
  // 不清空、不闪 loading、不动用户滚动位置（区别于手动刷新的整页重置）。
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    void listen("news-refreshed", () => {
      void fetchPage(0).then((res) => {
        if (cancelled || res.status !== "ok") return;
        setItems((prev) => {
          const seen = new Set(prev.map((x) => x.id));
          const fresh = res.data.items.filter((x) => !seen.has(x.id));
          if (fresh.length === 0) return prev;
          // 新条目时间最新 → 放在倒序时间线最前；已存在的保持原序与滚动锚点。
          return [...fresh, ...prev];
        });
        const m: Record<string, number> = {};
        for (const dc of res.data.dateCounts ?? []) m[dc.date] = dc.count;
        setDateCounts(m);
        setLastUpdated(new Date());
      });
    }).then((un) => {
      if (cancelled) un();
      else unlisten = un;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [fetchPage]);

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

  const scrollToDate = useCallback((dateKey: string) => {
    const header = sectionRefs.current.get(dateKey);
    if (!header) return false;
    // 不能用 header.scrollIntoView：header 是 sticky(top:0)，scrollIntoView 会按它
    // "已粘住"的位置算，短小节会把唯一一条挤出视口。改用非粘性的 section 手动算偏移。
    const section = (header.closest(".news-day-section") as HTMLElement | null) ?? header;
    const container = header.closest(".news-timeline") as HTMLElement | null;
    if (container) {
      const top =
        section.getBoundingClientRect().top -
        container.getBoundingClientRect().top +
        container.scrollTop;
      container.scrollTo({ top, behavior: "smooth" });
    } else {
      section.scrollIntoView({ behavior: "smooth", block: "start" });
    }
    setActiveDate(dateKey);
    return true;
  }, []);

  // 点日期导航跳转：把目标日「最新一条」顶到列表顶部。
  // 关键：必须一直加载到**比目标日更早**的日期，目标日下方才有内容垫着，
  // 否则目标日成了列表末尾，scrollIntoView 无法把它顶到顶部（只能停在底部，
  // 看起来"只滚到当日最早一条"）。
  const localDateKey = (it: FetchNewsItem): string | null =>
    it.publishedAt ? formatDateKey(new Date(it.publishedAt)) : null;

  const handleSelectDate = useCallback(
    async (dateKey: string) => {
      // 已经加载到比目标日更早的内容？没有就继续 load more。
      const hasLoadedPast = () =>
        itemsRef.current.some((it) => {
          const k = localDateKey(it);
          return k !== null && k < dateKey;
        });
      if (!hasLoadedPast()) {
        setLoadingMore(true);
        let cur = pageRef.current;
        let guard = 0;
        while (cur?.hasMore && guard < 60) {
          guard++;
          const res = await fetchPage(cur.offset + cur.limit);
          if (res.status !== "ok") break;
          const batch = res.data.items;
          setItems((prev) => {
            const seen = new Set(prev.map((x) => x.id));
            return [...prev, ...batch.filter((x) => !seen.has(x.id))];
          });
          cur = res.data.page;
          setPage(cur);
          pageRef.current = cur;
          const oldest = batch[batch.length - 1];
          const oldestKey = oldest ? localDateKey(oldest) : null;
          if (oldestKey !== null && oldestKey < dateKey) break; // 已越过目标日
        }
        setLoadingMore(false);
      }
      // 等新 section 渲染 + ref 注册后再滚动；跨帧重试直到命中。
      let tries = 30;
      const tick = () => {
        if (scrollToDate(dateKey)) return;
        if (--tries > 0) requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
    },
    [fetchPage, scrollToDate],
  );

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

  // 日期导航计数直接用后端 dateCounts（真实每日总数，不随分页累积）。

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

    </>
  );

  return (
    <PageShell
      title="资讯"
      status={status}
      statusTone={statusTone}
      meta={meta}
      controls={controls}
      compact
    >
      <NewsDateNav
        countsByDate={dateCounts}
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
              registerSectionRef={handleRegisterSectionRef}
              onActiveDateChange={setActiveDate}
              query={query}
              sourceNames={sourceNames}
            />
          )}
        </div>
      </div>
    </PageShell>
  );
}
