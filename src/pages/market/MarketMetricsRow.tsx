// MarketMetricsRow — 市场页顶部指标行（6 卡片横排）。
//
// Spec: docs/design/frontend-design.md §4 + quotes-module.md §4
//
// 卡片：4 个核心指数 + 市场宽度 + 行业热度。

import { useEffect } from "react";
import { useCoreIndexes, CORE_INDEXES } from "../../lib/useCoreIndexes";
import { setSource as setPullSource } from "../../lib/quotePull";
import { useMarketBreadth } from "../../lib/useMarketBreadth";
import { useIndustryHeatmap } from "../../lib/useIndustryHeatmap";
import { useMarketSession } from "../../lib/tradingSession";
import { IndexCard } from "./IndexCard";
import { BreadthCard } from "./BreadthCard";
import { HeatmapCard } from "./HeatmapCard";

interface MarketMetricsRowProps {
  selected?: string | null;
  onSelectIndex?: (tsCode: string) => void;
  /** 顶部右上角小字（如 "已更新 19:30:00"） */
  statusText?: string;
}

export function MarketMetricsRow({
  selected,
  onSelectIndex,
  statusText,
}: MarketMetricsRowProps) {
  const indexes = useCoreIndexes();
  const breadth = useMarketBreadth();
  const heatmap = useIndustryHeatmap({ topN: 5 });
  const session = useMarketSession();

  // 聚焦 pull：把核心指数声明为 indices 来源，交给 quotePull 协调器统一刷新。
  useEffect(() => {
    setPullSource(
      "indices",
      CORE_INDEXES.map((c) => c.tsCode),
    );
    return () => setPullSource("indices", []);
  }, []);

  return (
    <div className="market-metrics-row">
      {statusText && (
        <div className="market-metrics-status">
          <span
            className={`market-session-chip ${session.active ? "live" : "idle"}`}
            title="A 股交易时段"
          >
            <span className="status-dot" aria-hidden />
            {session.label}
          </span>
          {statusText}
        </div>
      )}
      {CORE_INDEXES.map((info) => {
        const item = indexes.items.find((i) => i.tsCode === info.tsCode);
        return (
          <IndexCard
            key={info.tsCode}
            label={info.label}
            tsCode={info.tsCode}
            item={item}
            active={selected === info.tsCode}
            onClick={() => onSelectIndex?.(info.tsCode)}
          />
        );
      })}
      <BreadthCard
        data={breadth.data}
        loading={breadth.loading}
        error={breadth.error}
      />
      <HeatmapCard
        data={heatmap.data}
        loading={heatmap.loading}
        error={heatmap.error}
      />
    </div>
  );
}
