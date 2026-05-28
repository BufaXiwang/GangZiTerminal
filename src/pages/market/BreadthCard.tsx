// BreadthCard — 市场宽度卡片：上涨 / 下跌家数 + 涨停 / 跌停。
//
// Spec: docs/design/quotes-module.md §4 market_breadth
//
// 视觉：标题 + "上涨 N / 下跌 M" + 水平 stacked bar（绿 / 灰 / 红 比例） + 涨停/跌停小字。

import type { MarketBreadth } from "../../bindings";

interface BreadthCardProps {
  data: MarketBreadth | null;
  loading?: boolean;
  error?: string | null;
}

export function BreadthCard({ data, loading, error }: BreadthCardProps) {
  if (error && !data) {
    return (
      <div className="metric-card breadth-card">
        <div className="metric-card-head">
          <span className="metric-card-label">市场宽度</span>
        </div>
        <div className="metric-card-empty">数据不可用</div>
      </div>
    );
  }
  if (!data) {
    return (
      <div className="metric-card breadth-card">
        <div className="metric-card-head">
          <span className="metric-card-label">市场宽度</span>
        </div>
        <div className="metric-card-empty muted">{loading ? "加载中" : "—"}</div>
      </div>
    );
  }

  const { up, down, flat, limitUp, limitDown, total } = data;
  // 用 total 作分母；如果 total = 0 退化为全灰。
  const denom = total > 0 ? total : 1;
  const upPct = (up / denom) * 100;
  const flatPct = (flat / denom) * 100;
  const downPct = (down / denom) * 100;

  return (
    <div className="metric-card breadth-card">
      <div className="metric-card-head">
        <span className="metric-card-label">市场宽度</span>
        <span className="metric-card-sub tabular">{total} 只</span>
      </div>
      <div className="breadth-counts tabular">
        <span className="up">↑ {up}</span>
        <span className="muted">/</span>
        <span className="down">↓ {down}</span>
      </div>
      <div className="breadth-bar" aria-hidden>
        <span className="bar-up" style={{ width: `${upPct}%` }} />
        <span className="bar-flat" style={{ width: `${flatPct}%` }} />
        <span className="bar-down" style={{ width: `${downPct}%` }} />
      </div>
      <div className="breadth-limits tabular">
        <span className="limit-up" title="涨停">
          涨停 {limitUp}
        </span>
        <span className="limit-down" title="跌停">
          跌停 {limitDown}
        </span>
      </div>
    </div>
  );
}
