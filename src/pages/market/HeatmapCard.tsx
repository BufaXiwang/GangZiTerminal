// HeatmapCard — 行业热度卡片：top N 涨幅行业。
//
// Spec: docs/design/quotes-module.md §4 industry_heatmap
//
// 视觉：标题 + 5 行（行业名 + 平均涨幅%）。内容超出时内部滚动。

import type { IndustryHeatmap } from "../../bindings";

interface HeatmapCardProps {
  data: IndustryHeatmap | null;
  loading?: boolean;
  error?: string | null;
}

function fmtPct(v: number | undefined | null): string {
  if (v == null || !Number.isFinite(v)) return "-";
  const sign = v > 0 ? "+" : "";
  return `${sign}${v.toFixed(2)}%`;
}

function pctClass(v: number | undefined | null): string {
  if (v == null) return "flat";
  if (v > 0) return "up";
  if (v < 0) return "down";
  return "flat";
}

export function HeatmapCard({ data, loading, error }: HeatmapCardProps) {
  if (error && !data) {
    return (
      <div className="metric-card heatmap-card">
        <div className="metric-card-head">
          <span className="metric-card-label">行业热度</span>
        </div>
        <div className="metric-card-empty">数据不可用</div>
      </div>
    );
  }
  if (!data || data.topGainers.length === 0) {
    return (
      <div className="metric-card heatmap-card">
        <div className="metric-card-head">
          <span className="metric-card-label">行业热度</span>
        </div>
        <div className="metric-card-empty muted">
          {loading ? "加载中" : "—"}
        </div>
      </div>
    );
  }

  return (
    <div className="metric-card heatmap-card">
      <div className="metric-card-head">
        <span className="metric-card-label">行业热度</span>
        <span className="metric-card-sub">领涨 Top {data.topGainers.length}</span>
      </div>
      <div className="heatmap-list">
        {data.topGainers.map((row) => (
          <div key={row.sector} className="heatmap-row" title={row.sector}>
            <span className="sector-name">{row.sector}</span>
            <span className={`sector-pct tabular ${pctClass(row.avgChangePercent)}`}>
              {fmtPct(row.avgChangePercent)}
            </span>
          </div>
        ))}
      </div>
    </div>
  );
}
