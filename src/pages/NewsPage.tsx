// NewsPage — 资讯页 placeholder。
//
// Spec: docs/design/frontend-design.md §4 资讯页
//
// F3 sub-agent 实现：刷新状态 + 筛选 + 时间线 + 展开正文。

import { PageShell } from "../components/PageShell";

export default function NewsPage() {
  return (
    <PageShell title="资讯" status="待实现" statusTone="stale">
      <div className="placeholder-block">
        <div className="placeholder-title">资讯页（F3 sub-agent 负责）</div>
        <div>来源 / 筛选 / 时间线 / 展开正文</div>
      </div>
    </PageShell>
  );
}
