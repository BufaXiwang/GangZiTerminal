// KlinePeriodTabs — K 线周期切换器（分钟 K / 日周月）。
//
// Spec: docs/design/frontend-design.md §5 K 线图
//
// 两组分隔：
//   [1m 5m 15m 30m 60m]  |  [日K 周K 月K]
//
// 「分时」是 Quotes 规划能力，但第一阶段隐藏 / 暂缓产品化：
// 当前 TDX minute_time 响应非标准，尚不能作为可靠产品能力。详见 quotes-module.md §5。

import type { ChartPeriod } from "../../lib/useKlineData";

interface KlinePeriodTabsProps {
  value: ChartPeriod;
  onChange: (period: ChartPeriod) => void;
}

interface PeriodOption {
  value: ChartPeriod;
  label: string;
}

const GROUP_MINUTE: PeriodOption[] = [
  { value: "1m", label: "1m" },
  { value: "5m", label: "5m" },
  { value: "15m", label: "15m" },
  { value: "30m", label: "30m" },
  { value: "60m", label: "60m" },
];

const GROUP_DAY: PeriodOption[] = [
  { value: "day", label: "日K" },
  { value: "week", label: "周K" },
  { value: "month", label: "月K" },
];

export function KlinePeriodTabs({ value, onChange }: KlinePeriodTabsProps) {
  const renderGroup = (group: PeriodOption[]) => (
    <div className="kline-tab-group" role="tablist">
      {group.map((p) => (
        <button
          key={p.value}
          type="button"
          role="tab"
          aria-selected={value === p.value}
          className={`kline-tab ${value === p.value ? "active" : ""}`}
          onClick={() => onChange(p.value)}
        >
          {p.label}
        </button>
      ))}
    </div>
  );

  return (
    <div className="kline-period-tabs">
      {renderGroup(GROUP_MINUTE)}
      <span className="kline-tab-sep" aria-hidden />
      {renderGroup(GROUP_DAY)}
    </div>
  );
}
