import { invoke } from "@tauri-apps/api/core";
import { ExternalLink, Newspaper, RefreshCw } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { useNewsRefresh } from "../hooks/useNewsRefresh";
import type { ArticleContent, NewsItem } from "../types";

const ALL_SOURCES = "__all__";
const DATE_TIMELINE_DAYS = 15; // 横向时间线显示最近 N 天（左=今天，右=更早，可横向滚动）

export function NewsPage() {
  const { items, isRefreshing, lastUpdated, refreshFeeds } = useNewsRefresh();
  const [sourceFilter, setSourceFilter] = useState<string>(ALL_SOURCES);
  /** 选中的日期（YYYY-MM-DD）。默认 = 今天。 */
  const [selectedDate, setSelectedDate] = useState<string>(() => todayKey());
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [articles, setArticles] = useState<Map<string, ArticleContent>>(new Map());
  const [loadingArticles, setLoadingArticles] = useState<Set<string>>(new Set());
  const fetchingRef = useRef<Set<string>>(new Set());

  // 聚合所有出现过的来源——按字母排序保证顺序稳定（chip 不会左右跳）
  const sources = useMemo(() => {
    const set = new Set<string>();
    for (const it of items) set.add(it.source);
    return Array.from(set).sort();
  }, [items]);

  /** 最近 N 天的"日期 → 计数"，给横向时间线渲染用 */
  const dateBuckets = useMemo(() => buildDateBuckets(items, DATE_TIMELINE_DAYS), [items]);

  // 按发布时间倒序 + 来源 filter + 选中日期 filter
  const visible = useMemo(() => {
    return [...items]
      .filter((it) => {
        if (sourceFilter !== ALL_SOURCES && it.source !== sourceFilter) return false;
        const t = parsePublished(it.published);
        if (t <= 0) return false;
        const key = localDateKey(new Date(t));
        if (key !== selectedDate) return false;
        return true;
      })
      .sort((a, b) => parsePublished(b.published) - parsePublished(a.published));
  }, [items, sourceFilter, selectedDate]);

  // 聚合到"日期 → 条数组"，让模板能在跨日处插入日期 anchor
  const grouped = useMemo(() => groupByDate(visible), [visible]);

  async function toggleExpand(item: NewsItem) {
    const willExpand = !expanded.has(item.id);
    setExpanded((prev) => {
      const next = new Set(prev);
      if (willExpand) next.add(item.id);
      else next.delete(item.id);
      return next;
    });
    if (!willExpand) return;
    if (articles.has(item.id)) return;
    if (!item.link) return;
    if (fetchingRef.current.has(item.id)) return;
    fetchingRef.current.add(item.id);
    setLoadingArticles((prev) => new Set(prev).add(item.id));
    try {
      const article = await invoke<ArticleContent>("fetch_article_content", {
        url: item.link,
        itemId: item.id,
        source: item.source,
        fallbackTitle: item.title,
        fallbackSummary: item.summary,
        fallbackPublished: item.published,
      });
      setArticles((prev) => new Map(prev).set(item.id, article));
    } catch (err) {
      // 失败时塞一个占位 article 让 UI 知道是错误
      setArticles((prev) =>
        new Map(prev).set(item.id, {
          url: item.link ?? "",
          title: item.title,
          source: item.source ?? undefined,
          published: item.published ?? undefined,
          author: undefined,
          paragraphs: [`抓取失败：${err instanceof Error ? err.message : String(err)}`],
          images: [],
          fetchedAt: new Date().toISOString(),
          extraction: "error",
        }),
      );
    } finally {
      fetchingRef.current.delete(item.id);
      setLoadingArticles((prev) => {
        const next = new Set(prev);
        next.delete(item.id);
        return next;
      });
    }
  }

  // 初始化没数据 → 立刻拉一次
  useEffect(() => {
    if (items.length === 0 && !isRefreshing) {
      void refreshFeeds();
    }
    // 故意只在 mount 时跑一次
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <section className="page-shell news-page">
      <header className="section-head">
        <div>
          <h2>资讯</h2>
          <p>
            财经资讯时间线
            {lastUpdated && (
              <>
                <span className="news-update-sep"> · </span>
                最近更新 {formatUpdateTime(lastUpdated)}
              </>
            )}
          </p>
        </div>
        <button
          type="button"
          className="ghost"
          onClick={() => void refreshFeeds()}
          disabled={isRefreshing}
        >
          <RefreshCw size={14} className={isRefreshing ? "spin" : ""} />
          {isRefreshing ? "刷新中" : "刷新"}
        </button>
      </header>

      <div className="news-filter-bar">
        <div className="news-date-timeline">
          <div className="news-date-axis">
            {dateBuckets.map((bucket) => {
              const active = selectedDate === bucket.dateKey;
              const empty = bucket.count === 0;
              return (
                <button
                  key={bucket.dateKey}
                  type="button"
                  className={`news-date-tick${active ? " active" : ""}${empty ? " empty" : ""}`}
                  onClick={() => setSelectedDate(bucket.dateKey)}
                  title={`${bucket.fullLabel} · ${bucket.count} 条`}
                >
                  <span className="news-date-dot" />
                  <span className="news-date-label">{bucket.shortLabel}</span>
                  <span className="news-date-count">{bucket.count}</span>
                </button>
              );
            })}
          </div>
        </div>
        <div className="news-filter-group">
          <span className="news-filter-label">来源</span>
          <button
            type="button"
            className={sourceFilter === ALL_SOURCES ? "chip slim active" : "chip slim"}
            onClick={() => setSourceFilter(ALL_SOURCES)}
          >
            全部
          </button>
          {sources.map((source) => (
            <button
              key={source}
              type="button"
              className={sourceFilter === source ? "chip slim active" : "chip slim"}
              onClick={() => setSourceFilter(source)}
            >
              {source}
            </button>
          ))}
        </div>
      </div>

      {visible.length === 0 ? (
        <div className="news-empty">
          <Newspaper size={32} strokeWidth={1.2} />
          <p>{items.length === 0 ? "暂无资讯，等待 scheduler 自动拉取或手动刷新。" : "当前筛选下没有匹配。"}</p>
        </div>
      ) : (
        <div className="news-timeline">
          {grouped.map((group) => (
            <div className="news-timeline-group" key={group.dateKey}>
              <div className="news-timeline-date">
                <span className="news-timeline-date-dot" aria-hidden="true" />
                <strong>{group.dateLabel}</strong>
                <span className="news-timeline-date-count">{group.items.length} 条</span>
              </div>
              {group.items.map((item) => {
                const isExpanded = expanded.has(item.id);
                const isLoading = loadingArticles.has(item.id);
                const article = articles.get(item.id);
                return (
                  <article
                    key={item.id}
                    className={`news-timeline-row${isExpanded ? " expanded" : ""}`}
                  >
                    <div className="news-timeline-rail" aria-hidden="true">
                      <span className="news-timeline-dot" />
                    </div>
                    <div className="news-timeline-content">
                      <div className="news-timeline-meta">
                        <time>{formatTimeShort(item.published)}</time>
                        <span className="news-source-chip">{item.source}</span>
                        {item.link && (
                          <a
                            className="news-external"
                            href={item.link}
                            target="_blank"
                            rel="noreferrer"
                            onClick={(e) => e.stopPropagation()}
                            title="新窗口打开原文"
                          >
                            <ExternalLink size={11} />
                          </a>
                        )}
                      </div>
                      <button
                        type="button"
                        className="news-timeline-title-btn"
                        onClick={() => void toggleExpand(item)}
                      >
                        <strong>{item.title}</strong>
                      </button>
                      {item.summary && !isExpanded && (
                        <p className="news-timeline-summary">{item.summary}</p>
                      )}
                      {isExpanded && (
                        <div className="news-timeline-article">
                          {isLoading && !article && (
                            <p className="news-article-status">正在抓取正文…</p>
                          )}
                          {article && article.extraction === "error" ? (
                            <p className="news-article-status error">
                              {article.paragraphs[0]}
                            </p>
                          ) : article ? (
                            <>
                              {article.paragraphs.slice(0, 30).map((para, idx) => (
                                <p key={idx}>{para}</p>
                              ))}
                              {article.paragraphs.length > 30 && (
                                <p className="news-article-status">
                                  …{article.paragraphs.length - 30} 段已省略，
                                  <a href={item.link ?? "#"} target="_blank" rel="noreferrer">
                                    点这里看完整原文
                                  </a>
                                </p>
                              )}
                            </>
                          ) : null}
                        </div>
                      )}
                    </div>
                  </article>
                );
              })}
            </div>
          ))}
        </div>
      )}
    </section>
  );
}

// ===== helpers ============================================================

type DateGroup = {
  dateKey: string;
  dateLabel: string;
  items: NewsItem[];
};

function groupByDate(items: NewsItem[]): DateGroup[] {
  const map = new Map<string, NewsItem[]>();
  for (const it of items) {
    const d = parsePublished(it.published);
    const key = d > 0 ? localDateKey(new Date(d)) : "unknown";
    const arr = map.get(key) ?? [];
    arr.push(it);
    map.set(key, arr);
  }
  return Array.from(map.entries()).map(([dateKey, items]) => ({
    dateKey,
    dateLabel: dateKeyToLabel(dateKey),
    items,
  }));
}

function dateKeyToLabel(key: string): string {
  if (key === "unknown") return "未知日期";
  if (key === todayKey()) return "今天";
  const yesterday = new Date();
  yesterday.setDate(yesterday.getDate() - 1);
  if (key === localDateKey(yesterday)) return "昨天";
  // YYYY-MM-DD → M月D日
  const [, m, d] = key.split("-");
  if (m && d) return `${parseInt(m, 10)}月${parseInt(d, 10)}日`;
  return key;
}

/** 本地时区的 YYYY-MM-DD（不要用 toISOString()——它是 UTC，可能差一天） */
function localDateKey(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const dd = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${dd}`;
}

function todayKey(): string {
  return localDateKey(new Date());
}

type DateBucket = {
  /** YYYY-MM-DD 本地日期 */
  dateKey: string;
  /** 短 label：今/昨/M.D */
  shortLabel: string;
  /** 完整 label：M月D日 周X */
  fullLabel: string;
  /** 当天条数 */
  count: number;
};

/** 构造横向时间线 buckets——最近 days 天（含今天），**左=今天，右=更早**（用户要求）。
 *  当天没有 item 也保留（dot 灰显）。容器允许横向滚动。 */
function buildDateBuckets(items: NewsItem[], days: number): DateBucket[] {
  const counts = new Map<string, number>();
  for (const it of items) {
    const t = parsePublished(it.published);
    if (t <= 0) continue;
    const key = localDateKey(new Date(t));
    counts.set(key, (counts.get(key) ?? 0) + 1);
  }

  const buckets: DateBucket[] = [];
  const now = new Date();
  const startOfToday = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  for (let i = 0; i < days; i++) {
    const d = new Date(startOfToday);
    d.setDate(d.getDate() - i);
    const dateKey = localDateKey(d);
    let shortLabel: string;
    if (i === 0) shortLabel = "今";
    else if (i === 1) shortLabel = "昨";
    else shortLabel = `${d.getMonth() + 1}/${d.getDate()}`;
    const weekday = ["日", "一", "二", "三", "四", "五", "六"][d.getDay()];
    const fullLabel = `${d.getMonth() + 1}月${d.getDate()}日 周${weekday}`;
    buckets.push({
      dateKey,
      shortLabel,
      fullLabel,
      count: counts.get(dateKey) ?? 0,
    });
  }
  return buckets;
}

function parsePublished(raw: string | null | undefined): number {
  if (!raw) return 0;
  // 兼容多种形态：ISO / unix ms / unix s / 'YYYY-MM-DD HH:MM:SS'
  const trimmed = raw.trim();
  if (!trimmed) return 0;
  if (/^\d+$/.test(trimmed)) {
    const n = parseInt(trimmed, 10);
    // > 10^12 视为毫秒，否则秒
    return n > 1_000_000_000_000 ? n : n * 1000;
  }
  // ISO 串
  const t = Date.parse(trimmed);
  if (Number.isFinite(t)) return t;
  // 'YYYY-MM-DD HH:MM:SS' (无 T) 也能被多数浏览器 parse；上面 Date.parse 失败再 fallback
  return 0;
}

function formatTimeShort(raw: string | null | undefined): string {
  const t = parsePublished(raw);
  if (!t) return "—";
  const d = new Date(t);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

function formatUpdateTime(iso: string): string {
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return iso;
  const d = new Date(t);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}:${String(d.getSeconds()).padStart(2, "0")}`;
}
