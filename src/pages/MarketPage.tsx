// MarketPage — 市场页 placeholder。
//
// Spec: docs/design/frontend-design.md §4 市场页
//
// F2 sub-agent 实现：市场状态 / 核心指数 / breadth → 筛选排序 → 列表 → 详情 + K 线。

import { PageShell } from "../components/PageShell";

export default function MarketPage() {
  return (
    <PageShell title="市场" status="待实现" statusTone="stale">
      <div className="placeholder-block">
        <div className="placeholder-title">市场页（F2 sub-agent 负责）</div>
        <div>核心指数 + breadth + 列表 + 详情 + K 线</div>
      </div>
    </PageShell>
  );
}
