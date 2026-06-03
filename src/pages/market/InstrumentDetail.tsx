// InstrumentDetail — 选中标的右侧详情面板（精简版）。
//
// Spec: docs/design/frontend-design.md §4 + §5 K 线图 + quotes-module.md §2/§4
//
// 结构：
//   header（单行）：name + tsCode + category tag + 最新价 + 涨跌% + 成交 + 日内 high/low
//   KlinePeriodTabs：9 周期分 3 组
//   K 线区：KlineCanvas（autoHeight 占满剩余空间）
//   footer：freshness 时间 + source（小灰字）

import { useState } from "react";
import { KlineCanvas, type ChartPeriod } from "../../components/KlineCanvas";
import { KlinePeriodTabs } from "./KlinePeriodTabs";
import type { ListMarketItem } from "../../bindings";

interface InstrumentDetailProps {
  item: ListMarketItem | null;
}

const CATEGORY_LABEL: Record<string, string> = {
  stock: "股票",
  index: "指数",
  fund: "基金",
};

function fmtNum(v: number | undefined | null, digits = 2): string {
  if (v == null || !Number.isFinite(v)) return "-";
  return v.toLocaleString("en-US", {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });
}

function fmtAmount(v: number | undefined | null): string {
  if (v == null || !Number.isFinite(v)) return "-";
  if (Math.abs(v) >= 1e8) return `${(v / 1e8).toFixed(2)}亿`;
  if (Math.abs(v) >= 1e4) return `${(v / 1e4).toFixed(2)}万`;
  return v.toFixed(0);
}

function changeClass(v: number | undefined | null): string {
  if (v == null) return "flat";
  if (v > 0) return "up";
  if (v < 0) return "down";
  return "flat";
}

function fmtPct(v: number | undefined | null): string {
  if (v == null || !Number.isFinite(v)) return "-";
  const sign = v > 0 ? "+" : "";
  return `${sign}${v.toFixed(2)}%`;
}

export function InstrumentDetail({ item }: InstrumentDetailProps) {
  const [period, setPeriod] = useState<ChartPeriod>("day");

  if (!item) {
    return (
      <div className="detail-empty">
        <div className="muted">从列表选择一只标的以查看详情</div>
      </div>
    );
  }

  const q = item.quote;
  const pct = q?.changePercent;
  const tone = changeClass(pct);
  const pricePrecision = item.category === "fund" ? 3 : 2;

  return (
    <div className="instrument-detail">
      {/* 精简一行 Header */}
      <div className="detail-header detail-header-compact">
        <div className="detail-header-left">
          <span className="detail-header-name">{item.name}</span>
          <span className="detail-header-code tabular">{item.tsCode}</span>
          <span className="chip detail-header-category">
            {CATEGORY_LABEL[item.category] ?? item.category}
          </span>
          {item.isSt && <span className="chip st-chip">ST</span>}
        </div>
        <div className="detail-header-right">
          <span className="detail-stat">
            <span className="label">最新</span>
            <span className={`value tabular ${tone}`}>{fmtNum(q?.price)}</span>
          </span>
          <span className="detail-stat">
            <span className={`value tabular ${tone}`}>{fmtPct(pct)}</span>
          </span>
          <span className="detail-stat">
            <span className="label">成交</span>
            <span className="value tabular">{fmtAmount(q?.amount)}</span>
          </span>
          <span className="detail-stat">
            <span className="label">日内</span>
            <span className="value tabular up">{fmtNum(q?.high)}</span>
            <span className="muted">/</span>
            <span className="value tabular down">{fmtNum(q?.low)}</span>
          </span>
        </div>
      </div>

      {/* K 线周期切换器 */}
      <div className="detail-period-strip">
        <KlinePeriodTabs value={period} onChange={setPeriod} />
      </div>

      {/* K 线区域（KlineCanvas 内部自己管 loading/empty/error + load-more） */}
      <div className="detail-chart">
        <KlineCanvas
          tsCode={item.tsCode}
          period={period}
          pricePrecision={pricePrecision}
          liveQuote={q ?? null}
        />
      </div>

      {/* 极简 footer：交易日 */}
      <div className="detail-footer">
        {q?.tradeDate && (
          <span className="muted">交易日 {q.tradeDate}</span>
        )}
      </div>
    </div>
  );
}
