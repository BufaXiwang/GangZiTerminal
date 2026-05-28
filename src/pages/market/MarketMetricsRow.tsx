// MarketMetricsRow — 市场页顶部指标行（6 卡片横排）。
//
// Spec: docs/design/frontend-design.md §4 + quotes-module.md §4
//
// 卡片：4 个核心指数 + 市场宽度 + 行业热度。

import { useCoreIndexes, CORE_INDEXES } from "../../lib/useCoreIndexes";
import { useMarketBreadth } from "../../lib/useMarketBreadth";
import { useIndustryHeatmap } from "../../lib/useIndustryHeatmap";
import { IndexCard } from "./IndexCard";
import { BreadthCard } from "./BreadthCard";
import { HeatmapCard } from "./HeatmapCard";

interface MarketMetricsRowProps {
  /** 当前选中的 tsCode，用于高亮对应 IndexCard */
  selected?: string | null;
  onSelectIndex?: (tsCode: string) => void;
}

export function MarketMetricsRow({
  selected,
  onSelectIndex,
}: MarketMetricsRowProps) {
  const indexes = useCoreIndexes();
  const breadth = useMarketBreadth();
  const heatmap = useIndustryHeatmap({ topN: 5 });

  return (
    <div className="market-metrics-row">
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
