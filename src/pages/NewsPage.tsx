// NewsPage — 资讯页主结构。
//
// Spec: docs/design/frontend-design.md §4 资讯页（日期导航 = 双向窗口流）
//       docs/design/news-module.md §4 fetch_news（order 语义 + 双向 keyset 读取）
//
// 结构：
//   PageShell
//     control strip：source 多选 chip + FTS 搜索 + 刷新
//   ─ 顶部 横向 日期 nav（最近 14 天）
//   ─ workspace：纵向时间线（按日分组，倒序，双向窗口流）；正文行内展示、>3 行可展开
//
// 数据流（双向 keyset 窗口流，spec §4）：
//   - 列表是「全局倒序时间线」的一段连续窗口；游标取窗口首/尾的 publishedAt。
//   - mount / filter 变化：fetchNews({ order:"desc", limit }) → 最新一页（最新端）。
//   - 向更早（下滑底部 sentinel）：publishedTo = oldest.publishedAt, order:"desc"。
//   - 向更新（上滑顶部 sentinel）：publishedFrom = newest.publishedAt, order:"asc"
//     → reverse → prepend，并做滚动锚定补偿避免视口跳动。
//   - 点日期锚定：publishedTo = 该北京日 23:59:59.999, order:"desc" → 替换窗口。
//   - 任何跳转/滑动都只取一页，绝不全量拉取中间天。
//   - 去重一律按 id Set（闭区间游标会带回边界条）。

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
  type FetchNewsRequest,
  type NewsSource,
} from "../bindings";
import { NewsDateNav, formatDateKey } from "./news/NewsDateNav";
import { NewsTimeline, NEWS_ROW_SELECTOR } from "./news/NewsTimeline";
import { beijingDayEndIso } from "../lib/beijingTime";
import { captureTopAnchor, type TopAnchor } from "../lib/scrollAnchor";

const PAGE_SIZE = 50;
const SEARCH_DEBOUNCE_MS = 300;
// 滑动窗口 DOM 上限：保留的 items 封顶（4 页）。向一端扩展超限即裁掉远端，
// 被裁端 hasMore 置回 true 可用 keyset 游标回滚重拉。根治长滚动后数千常驻节点
// 导致的切 tab / 滚动布局重算卡顿（content-visibility 只省绘制不省布局）。
// Spec: news-module.md §fetch_news 双向 keyset「窗口有界（~200）」；frontend-design.md §4。
const MAX_WINDOW = 200;

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

  // === 双向窗口 state ===
  // items 是全局倒序时间线的一段连续窗口（按 publishedAt desc）。
  const [items, setItems] = useState<FetchNewsItem[]>([]);
  // 两端是否还能继续扩展（各自方向是否满页判定）。
  const [hasMoreOlder, setHasMoreOlder] = useState(false);
  const [hasMoreNewer, setHasMoreNewer] = useState(false);
  // 跳日期 / sentinel 回调里读最新窗口，避免闭包拿旧值。
  const itemsRef = useRef<FetchNewsItem[]>([]);
  itemsRef.current = items;
  const hasMoreNewerRef = useRef(false);
  hasMoreNewerRef.current = hasMoreNewer;

  // 每日真实总数（后端 GROUP BY，不受分页限制）—— 给日期导航显示真实条数。
  const [dateCounts, setDateCounts] = useState<Record<string, number>>({});
  const [loading, setLoading] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [loadingNewer, setLoadingNewer] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [warnings, setWarnings] = useState<string[]>([]);
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null);

  // 滚动锚定（元素锚定）：头部内容增减（prepend 向更新 / 裁头部）前记下视口顶部第一条
  // 可见行的 TopAnchor，DOM 更新后在 NewsTimeline 的 layout effect 里按同一行真实位移补偿。
  const timelineElRef = useRef<HTMLElement | null>(null);
  const pendingScrollAnchor = useRef<TopAnchor | null>(null);

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

  // === 统一 fetch 入口：注入当前 filter（query / sources），调用方补 order / 游标。 ===
  const fetchWindow = useCallback(
    async (extra: Omit<FetchNewsRequest, "query" | "sources" | "includeArticle" | "limit">) => {
      const sourceArr = selectedSources.size > 0 ? Array.from(selectedSources) : undefined;
      return commands.fetchNews({
        query: query.length > 0 ? query : undefined,
        sources: sourceArr,
        // 带全文：行内展开要显示完整正文，不能只给 500 字的 articleExcerpt。
        includeArticle: true,
        limit: PAGE_SIZE,
        ...extra,
      });
    },
    [query, selectedSources],
  );

  const applyDateCounts = useCallback((dc?: { date: string; count: number }[] | null) => {
    const m: Record<string, number> = {};
    for (const d of dc ?? []) m[d.date] = d.count;
    setDateCounts(m);
  }, []);

  // === 默认初始加载（mount / filter 变化）：最新一页 ===
  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    setWarnings([]);
    void fetchWindow({ order: "desc" }).then((res) => {
      if (cancelled) return;
      if (res.status === "error") {
        setError(`${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`);
        setItems([]);
        setHasMoreOlder(false);
        setHasMoreNewer(false);
        setLoading(false);
        return;
      }
      const batch = res.data.items;
      setItems(batch);
      // 处于最新端：上方没有更新的。下方是否还有取决于本批是否满页。
      setHasMoreNewer(false);
      setHasMoreOlder(batch.length >= PAGE_SIZE);
      applyDateCounts(res.data.dateCounts);
      if (res.data.errors && res.data.errors.length > 0) {
        setWarnings(
          res.data.errors.map(
            (e) =>
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
  }, [fetchWindow, applyDateCounts, refreshTick]);

  // === 后端 scheduler 刷新出新资讯时同步前端（仅最新端） ===
  // 后端每 ~60s run_refresh，savedCount>0 / articleUpdated>0 时 emit `news-refreshed`。
  // 只在「处于最新端」（hasMoreNewer===false，即没在看历史）时 merge-prepend 新条；
  // anchored 看历史时忽略，避免历史视图被今天的新闻插入跳动（spec：live gating）。
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    void listen("news-refreshed", () => {
      if (hasMoreNewerRef.current) return; // 在看历史，忽略
      void fetchWindow({ order: "desc" }).then((res) => {
        if (cancelled || res.status !== "ok") return;
        if (hasMoreNewerRef.current) return; // 二次确认（async 间隙可能已跳历史）
        let trimmedTail = false;
        setItems((prev) => {
          const seen = new Set(prev.map((x) => x.id));
          const fresh = res.data.items.filter((x) => !seen.has(x.id));
          if (fresh.length === 0) return prev;
          // 新条目时间最新 → 放在倒序时间线最前。
          const merged = [...fresh, ...prev];
          // 窗口有界：live prepend 也封顶，从尾部（最早端）裁掉多出的条。
          // 用户在最新端（hasMoreNewer===false），裁尾在视口下方、不需要锚定补偿。
          if (merged.length > MAX_WINDOW) {
            trimmedTail = true;
            return merged.slice(0, MAX_WINDOW);
          }
          return merged;
        });
        // 被裁的最早端可回滚：底部 sentinel 会用 keyset 游标向更早重拉。
        if (trimmedTail) setHasMoreOlder(true);
        applyDateCounts(res.data.dateCounts);
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
  }, [fetchWindow, applyDateCounts]);

  // === 向更早（下滑底部 sentinel）：publishedTo = oldest.publishedAt, order:"desc" ===
  const handleLoadOlder = useCallback(async () => {
    if (!hasMoreOlder || loadingMore || loading) return;
    const window = itemsRef.current;
    const oldest = window[window.length - 1];
    if (!oldest?.publishedAt) {
      // 末尾条无 publishedAt（落到"未知日期"组）无法做游标，停止下扩。
      setHasMoreOlder(false);
      return;
    }
    setLoadingMore(true);
    const res = await fetchWindow({ publishedTo: oldest.publishedAt, order: "desc" });
    if (res.status === "ok") {
      const batch = res.data.items;
      // 滑动窗口：append 到尾部（视口下方，不影响视口）；若超限从头部（最新端）裁掉多出的条。
      // 裁头部 = 视口上方内容减少 → 必须做元素锚定补偿（否则视口往上跳）。
      let trimmedHead = false;
      // 裁头部前记录锚（视口顶部第一条可见行）；裁完在 layout effect 补偿。
      pendingScrollAnchor.current = captureTopAnchor(timelineElRef.current, NEWS_ROW_SELECTOR);
      setItems((prev) => {
        const seen = new Set(prev.map((x) => x.id));
        const merged = [...prev, ...batch.filter((x) => !seen.has(x.id))];
        if (merged.length > MAX_WINDOW) {
          trimmedHead = true;
          // 从头部（最新端）裁掉多出的条，保持连续倒序窗口。游标随尾部 publishedAt 自然延续。
          return merged.slice(merged.length - MAX_WINDOW);
        }
        // 没裁头部就不需要锚定补偿（append 在视口下方）。
        pendingScrollAnchor.current = null;
        return merged;
      });
      setHasMoreOlder(batch.length >= PAGE_SIZE);
      // 被裁掉的新端可回滚：顶部 sentinel 会用 keyset 游标向更新重拉一页一页补回。
      if (trimmedHead) setHasMoreNewer(true);
      // 不更新 dateCounts：本请求带 publishedTo 游标，返回的 dateCounts 被截断（只含 ≤游标 的天）。
      // dateCounts 是「全量每日真实总数」，只由无游标请求（初始/筛选/刷新）维护。
    } else {
      setError(`${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`);
    }
    setLoadingMore(false);
  }, [fetchWindow, hasMoreOlder, loading, loadingMore]);

  // === 向更新（上滑顶部 sentinel）：publishedFrom = newest.publishedAt, order:"asc" ===
  // reverse 后 prepend，并在 layout effect 里做滚动锚定补偿。
  const handleLoadNewer = useCallback(async () => {
    if (!hasMoreNewer || loadingNewer || loading) return;
    const window = itemsRef.current;
    const newest = window[0];
    if (!newest?.publishedAt) {
      setHasMoreNewer(false);
      return;
    }
    setLoadingNewer(true);
    const res = await fetchWindow({ publishedFrom: newest.publishedAt, order: "asc" });
    if (res.status === "ok") {
      const batch = res.data.items; // 升序
      const rawLen = batch.length;
      const ascDedupReversed = [...batch].reverse(); // → desc，准备 prepend
      let trimmedTail = false;
      // prepend 头部 = 视口上方内容增加 → 元素锚定：记下视口顶部第一条可见行，layout effect 补偿。
      pendingScrollAnchor.current = captureTopAnchor(timelineElRef.current, NEWS_ROW_SELECTOR);
      setItems((prev) => {
        const seen = new Set(prev.map((x) => x.id));
        const fresh = ascDedupReversed.filter((x) => !seen.has(x.id));
        if (fresh.length === 0) {
          pendingScrollAnchor.current = null;
          return prev;
        }
        const merged = [...fresh, ...prev];
        if (merged.length > MAX_WINDOW) {
          trimmedTail = true;
          // 从尾部（最早端）裁掉多出的条。裁尾在视口下方，不影响视口（无需补偿，锚仍只管 prepend）。
          return merged.slice(0, MAX_WINDOW);
        }
        return merged;
      });
      // 原始批满页 → 上方可能还有更新的；空/不满 → 已到最新端。
      setHasMoreNewer(rawLen >= PAGE_SIZE);
      // 被裁掉的最早端可回滚：底部 sentinel 会用 keyset 游标向更早重拉一页一页补回。
      if (trimmedTail) setHasMoreOlder(true);
      // 不更新 dateCounts（同上：本请求带 publishedFrom 游标，返回值被截断）。
    } else {
      setError(`${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`);
    }
    setLoadingNewer(false);
  }, [fetchWindow, hasMoreNewer, loading, loadingNewer]);

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

  // === 点日期锚定（spec §4 锚定到某天）===
  // publishedTo = 该北京日 23:59:59.999, order:"desc" → 该日最新一页打头，替换窗口。
  // hasMoreOlder = 批==50；hasMoreNewer = true（除非空批，首次上滑自然置 false）。
  const handleSelectDate = useCallback(
    async (dateKey: string) => {
      setLoadingMore(true);
      setError(null);
      const res = await fetchWindow({
        publishedTo: beijingDayEndIso(dateKey),
        order: "desc",
      });
      if (res.status === "ok") {
        const batch = res.data.items;
        setItems(batch); // 替换窗口
        setHasMoreOlder(batch.length >= PAGE_SIZE);
        // anchored 看历史：上方可能有更新的，置 true；空批时首次上滑会把它置 false。
        setHasMoreNewer(batch.length > 0);
        // 不更新 dateCounts（本请求带 publishedTo 锚定游标，返回值被截断；保留全量计数）。
        setLastUpdated(new Date());
      } else {
        setError(`${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`);
      }
      setLoadingMore(false);
      // 等新 section 渲染 + ref 注册后再滚动；跨帧重试直到命中。
      let tries = 30;
      const tick = () => {
        if (scrollToDate(dateKey)) return;
        if (--tries > 0) requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
    },
    [fetchWindow, scrollToDate],
  );

  const handleRegisterSectionRef = useCallback(
    (dateKey: string, el: HTMLElement | null) => {
      if (el) sectionRefs.current.set(dateKey, el);
      else sectionRefs.current.delete(dateKey);
    },
    [],
  );

  const handleRegisterTimelineRef = useCallback((el: HTMLElement | null) => {
    timelineElRef.current = el;
  }, []);

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
      {query.length > 0 && (
        <span className="muted">
          · 匹配 {items.length} 条
          {hasMoreOlder ? "+" : ""} (query: "{query}")
        </span>
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
              loadingNewer={loadingNewer}
              hasMore={hasMoreOlder}
              hasMoreNewer={hasMoreNewer}
              onLoadMore={handleLoadOlder}
              onLoadNewer={handleLoadNewer}
              registerSectionRef={handleRegisterSectionRef}
              registerTimelineRef={handleRegisterTimelineRef}
              scrollAnchorRef={pendingScrollAnchor}
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
