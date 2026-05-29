// ArticleDrawer — 右侧滑出 drawer，看资讯正文。
//
// Spec: docs/design/frontend-design.md §4 资讯页（"展开正文 / 关联标的 / Agent 分析入口"）
//       docs/design/news-module.md §4 fetch_news.includeArticle + warm_articles
//
// 行为：
//   - 打开时优先显示 item.articleExcerpt（spec §4 line 271：默认 500 字符摘录，已够看）
//   - 如果 item.article 已附带（列表层 includeArticle=true 时才会有）→ 直接渲染 article.content
//   - 否则提供"查看完整正文"按钮：
//       1. 调 commands.warmArticles({ newsIds: [item.id] })
//       2. 成功后调 commands.fetchNews({ query: item.title, includeArticle: true, limit: 50 })
//          —— spec §4 line 263 已删 ids 查询，只能用 title FTS 重新定位同一 id
//       3. 在结果里按 id 命中本条 → setLoaded(article)；若返回 article_missing warning，
//          就显示对应提示
//   - 关闭：点 overlay / ESC / 右上 X
//
// 不修改业务真源，所有错误来自后端 code（spec §5 封闭集合）。

import { ExternalLink, RefreshCcw, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  commands,
  type ArticleSnippet,
  type FetchNewsItem,
  type WarningCode,
} from "../../bindings";

export interface ArticleDrawerProps {
  item: FetchNewsItem | null;
  open: boolean;
  onClose: () => void;
  /** drawer 内部触发 warm + 重读 includeArticle=true 时，把更新后的 item 回写到列表。 */
  onItemUpdated: (item: FetchNewsItem) => void;
  /** sourceId → 友好展示名。 */
  sourceNames?: Record<string, string>;
}

type WarmState =
  | { kind: "idle" }
  | { kind: "warming" }
  | { kind: "fetching" }
  | { kind: "done"; article: ArticleSnippet }
  | { kind: "missing"; warnings: WarningCode[] }
  | { kind: "error"; message: string };

function formatTimeAgo(iso?: string): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  const diffMs = Date.now() - d.getTime();
  const sec = Math.floor(diffMs / 1000);
  if (sec < 60) return `${sec} 秒前抓取`;
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min} 分钟前抓取`;
  const hour = Math.floor(min / 60);
  if (hour < 24) return `${hour} 小时前抓取`;
  const day = Math.floor(hour / 24);
  return `${day} 天前抓取`;
}

function formatDateTime(iso?: string): string {
  if (!iso) return "无发布时间";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString("zh-CN", { hour12: false });
}

export function ArticleDrawer({
  item,
  open,
  onClose,
  onItemUpdated,
  sourceNames = {},
}: ArticleDrawerProps) {
  const [warm, setWarm] = useState<WarmState>({ kind: "idle" });

  // 当 item 切换时重置 warm 状态
  useEffect(() => {
    setWarm({ kind: "idle" });
  }, [item?.id]);

  // 打开即自动拉正文（spec §4 warm_articles 按需触发）：有 URL 且本地还没正文时，
  // 不让用户再点一次按钮。快讯类（无独立正文）会回 article_missing，优雅降级显示摘要。
  const autoWarmedRef = useRef<string | null>(null);

  // ESC 关闭
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [open, onClose]);

  const article: ArticleSnippet | null = useMemo(() => {
    if (warm.kind === "done") return warm.article;
    if (item?.article) return item.article;
    return null;
  }, [item, warm]);

  const handleFetchArticle = useCallback(async () => {
    if (!item) return;
    setWarm({ kind: "warming" });

    // 1. warm
    const warmRes = await commands.warmArticles({ newsIds: [item.id] });
    if (warmRes.status === "error") {
      setWarm({
        kind: "error",
        message: `${warmRes.error.code}${warmRes.error.message ? ": " + warmRes.error.message : ""}`,
      });
      return;
    }
    if (!warmRes.data.ok) {
      // discriminated union: warmRes.data.error
      const err = warmRes.data.error;
      setWarm({
        kind: "error",
        message: `${err.code}${err.field ? ` (${err.field})` : ""}${err.message ? ": " + err.message : ""}`,
      });
      return;
    }

    // 2. fetch with includeArticle=true，用 title 作为 FTS query 重新定位
    setWarm({ kind: "fetching" });
    const fetchRes = await commands.fetchNews({
      query: item.title,
      includeArticle: true,
      limit: 50,
      offset: 0,
    });
    if (fetchRes.status === "error") {
      setWarm({
        kind: "error",
        message: `${fetchRes.error.code}${fetchRes.error.message ? ": " + fetchRes.error.message : ""}`,
      });
      return;
    }
    const hit = fetchRes.data.items.find((x) => x.id === item.id);
    if (hit?.article) {
      setWarm({ kind: "done", article: hit.article });
      onItemUpdated(hit);
      return;
    }
    // 没命中或没 article → article_missing
    const warnings = hit?.warnings ?? [];
    setWarm({ kind: "missing", warnings });
    if (hit) onItemUpdated(hit);
  }, [item, onItemUpdated]);

  // 打开自动 warm：每个 item 只触发一次；已有 article / 无 URL 时跳过。
  useEffect(() => {
    if (!open || !item) return;
    if (item.article || item.articleExcerpt) return; // 本地已有正文/摘录
    if (!item.url) return; // 无 URL 无法抽取
    if (autoWarmedRef.current === item.id) return; // 本 item 已自动拉过
    autoWarmedRef.current = item.id;
    void handleFetchArticle();
  }, [open, item, handleFetchArticle]);

  if (!open || !item) return null;

  const showWarnings = item.warnings.length > 0;
  const articleFetchedAt =
    article?.fetchedAt ?? item.freshness?.articleFetchedAt;

  return (
    <div className="article-drawer-overlay" onClick={onClose}>
      <aside
        className="article-drawer"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-label="资讯正文"
      >
        <header className="article-drawer-head">
          <div className="article-drawer-head-meta">
            <span className="news-row-source">
              {sourceNames[item.source] ?? item.source}
            </span>
            <span>{formatDateTime(item.publishedAt)}</span>
            {item.url && (
              <a href={item.url} target="_blank" rel="noreferrer noopener">
                <ExternalLink size={12} style={{ marginRight: 2 }} />
                原文
              </a>
            )}
          </div>
          <button
            type="button"
            className="btn ghost"
            onClick={onClose}
            aria-label="关闭"
          >
            <X size={14} />
          </button>
        </header>

        <div className="article-drawer-body">
          <h2 className="article-drawer-title">
            {article?.title ?? item.title}
          </h2>

          <div className="article-drawer-byline">
            {articleFetchedAt && <span>{formatTimeAgo(articleFetchedAt)}</span>}
            {item.freshness?.ageMs !== undefined && (
              <span>记录龄期 {Math.round(item.freshness.ageMs / 1000)}s</span>
            )}
          </div>

          {showWarnings && (
            <div className="article-drawer-warning">
              ⚠ 提示：{item.warnings.join(", ")}
            </div>
          )}

          {/* 正文区：article.content > articleExcerpt > summary，按可用度降级。 */}
          {(() => {
            const loading = warm.kind === "warming" || warm.kind === "fetching";
            const body =
              article?.content ?? item.articleExcerpt ?? item.summary ?? null;
            if (body) {
              return (
                <article className="article-drawer-content">{body}</article>
              );
            }
            if (loading) {
              return (
                <div className="article-drawer-skeleton" aria-label="正在加载正文">
                  <span />
                  <span />
                  <span />
                </div>
              );
            }
            // 无正文且非加载中：快讯类无独立正文，引导看原文。
            return (
              <div className="article-drawer-empty">
                <p>这条资讯没有独立正文（多为快讯 / 一句话消息）。</p>
                {item.url && (
                  <a
                    className="btn"
                    href={item.url}
                    target="_blank"
                    rel="noreferrer noopener"
                  >
                    <ExternalLink size={13} style={{ marginRight: 4 }} />
                    打开原文
                  </a>
                )}
              </div>
            );
          })()}

          {/* 加载中提示条 */}
          {(warm.kind === "warming" || warm.kind === "fetching") &&
            (item.articleExcerpt || item.summary) && (
              <div className="article-drawer-loading-hint">
                <RefreshCcw size={11} className="spin" /> 正在抓取完整正文…
              </div>
            )}

          {/* 抓取失败 / 重试（次要操作，仅在有 URL 且失败时显示） */}
          {(warm.kind === "missing" || warm.kind === "error") &&
            !article &&
            item.url && (
              <div className="article-drawer-actions">
                <button
                  type="button"
                  className="btn ghost"
                  onClick={handleFetchArticle}
                >
                  <RefreshCcw size={12} style={{ marginRight: 4 }} />
                  重试抓取正文
                </button>
              </div>
            )}
        </div>
      </aside>
    </div>
  );
}
