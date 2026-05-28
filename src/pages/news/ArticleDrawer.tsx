// ArticleDrawer — 右侧滑出 drawer，看资讯正文。
//
// Spec: docs/design/frontend-design.md §4 资讯页 + docs/design/news-module.md §4 (fetch_news.includeArticle)
//
// C2 阶段：stub —— 让主结构能 build。C4 会接管 warm_articles + includeArticle 流程。

import { X } from "lucide-react";
import type { FetchNewsItem } from "../../bindings";

export interface ArticleDrawerProps {
  item: FetchNewsItem | null;
  open: boolean;
  onClose: () => void;
  /** drawer 内部触发 warm + 重读 includeArticle=true 时，把更新后的 item 回写到列表。 */
  onItemUpdated: (item: FetchNewsItem) => void;
}

export function ArticleDrawer({ item, open, onClose }: ArticleDrawerProps) {
  if (!open || !item) return null;
  return (
    <div className="article-drawer-overlay" onClick={onClose}>
      <aside
        className="article-drawer"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-label="资讯正文"
      >
        <header className="article-drawer-head">
          <span className="muted" style={{ fontSize: 12 }}>
            {item.source}
          </span>
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
          <h2 className="article-drawer-title">{item.title}</h2>
          <div className="muted" style={{ fontSize: 12, marginBottom: 12 }}>
            {item.publishedAt ?? "无发布时间"}
          </div>
          <div>{item.articleExcerpt ?? item.summary ?? "(C4 将在此渲染正文)"}</div>
        </div>
      </aside>
    </div>
  );
}
