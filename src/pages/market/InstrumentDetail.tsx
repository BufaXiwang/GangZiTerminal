// InstrumentDetail — 选中标的右侧详情面板。
//
// Spec: docs/design/frontend-design.md §4 + §5 K 线图 + quotes-module.md §2/§4
//
// 结构：
//   header: tsCode + name + category badge + 大字价格 + 涨跌幅
//   summary 网格：previousClose / open / high / low / limitUp / limitDown / volume / amount
//   period strip：分时 / 1m / 5m / 15m / 30m / 60m / 日 / 周 / 月
//   K 线区：KlineCanvas（candle 或 line）

import { useState } from "react";
import { KlineCanvas } from "../../components/KlineCanvas";
import { useKlineData, type ChartPeriod } from "../../lib/useKlineData";
import type { ListMarketItem } from "../../bindings";

interface InstrumentDetailProps {
  item: ListMarketItem | null;
}

interface PeriodOption {
  value: ChartPeriod;
  label: string;
}

const PERIODS: PeriodOption[] = [
  { value: "intraday", label: "分时" },
  { value: "1m", label: "1m" },
  { value: "5m", label: "5m" },
  { value: "15m", label: "15m" },
  { value: "30m", label: "30m" },
  { value: "60m", label: "60m" },
  { value: "day", label: "日" },
  { value: "week", label: "周" },
  { value: "month", label: "月" },
];

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

function fmtVolume(v: number | undefined | null): string {
  if (v == null || !Number.isFinite(v)) return "-";
  if (Math.abs(v) >= 1e8) return `${(v / 1e8).toFixed(2)}亿`;
  if (Math.abs(v) >= 1e4) return `${(v / 1e4).toFixed(2)}万`;
  return v.toLocaleString("en-US");
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

function fmtChange(v: number | undefined | null): string {
  if (v == null || !Number.isFinite(v)) return "-";
  const sign = v > 0 ? "+" : "";
  return `${sign}${v.toFixed(2)}`;
}

export function InstrumentDetail({ item }: InstrumentDetailProps) {
  const [period, setPeriod] = useState<ChartPeriod>("day");
  const kline = useKlineData({
    tsCode: item?.tsCode ?? null,
    period,
    enabled: !!item,
  });

  if (!item) {
    return (
      <div className="detail-empty">
        <div className="muted">从列表选择一只标的以查看详情</div>
      </div>
    );
  }

  const q = item.quote;
  const pct = q?.changePercent;
  const change = q?.change;
  // Note: spec'd full StockQuote.limitUp/limitDown 只在 fetch_data 里返回，
  // list_market 不带；这里 summary 显示 - 表示未取到。
  const limitUp = undefined as number | undefined;
  const limitDown = undefined as number | undefined;

  const isLineMode = period === "intraday";

  return (
    <div className="instrument-detail">
      <div className="detail-header">
        <div className="detail-header-line">
          <span className="detail-header-name">{item.name}</span>
          <span className="detail-header-code tabular">{item.tsCode}</span>
          <span className="chip detail-header-category">
            {CATEGORY_LABEL[item.category] ?? item.category}
          </span>
          {item.isSt && <span className="chip st-chip">ST</span>}
        </div>
        <div className="detail-price-line">
          <span className={`detail-price tabular ${changeClass(pct)}`}>
            {fmtNum(q?.price)}
          </span>
          <span className={`detail-change tabular ${changeClass(pct)}`}>
            {fmtChange(change)} {fmtPct(pct)}
          </span>
        </div>
      </div>

      <div className="detail-summary">
        <div className="summary-row">
          <span className="label">昨收</span>
          <span className="value">{fmtNum(q?.previousClose)}</span>
        </div>
        <div className="summary-row">
          <span className="label">今开</span>
          <span className="value">{fmtNum(q?.open)}</span>
        </div>
        <div className="summary-row">
          <span className="label">最高</span>
          <span className="value up">{fmtNum(q?.high)}</span>
        </div>
        <div className="summary-row">
          <span className="label">最低</span>
          <span className="value down">{fmtNum(q?.low)}</span>
        </div>
        <div className="summary-row">
          <span className="label">涨停</span>
          <span className="value up">{fmtNum(limitUp)}</span>
        </div>
        <div className="summary-row">
          <span className="label">跌停</span>
          <span className="value down">{fmtNum(limitDown)}</span>
        </div>
        <div className="summary-row">
          <span className="label">成交量</span>
          <span className="value">{fmtVolume(q?.volume)}</span>
        </div>
        <div className="summary-row">
          <span className="label">成交额</span>
          <span className="value">{fmtAmount(q?.amount)}</span>
        </div>
      </div>

      <div className="detail-period-strip">
        <div className="segmented" role="tablist">
          {PERIODS.map((p) => (
            <button
              key={p.value}
              type="button"
              role="tab"
              aria-selected={period === p.value}
              className={`segmented-item ${period === p.value ? "active" : ""}`}
              onClick={() => setPeriod(p.value)}
            >
              {p.label}
            </button>
          ))}
        </div>
      </div>

      <div className="detail-chart">
        {kline.loading && kline.data.length === 0 ? (
          <div className="detail-chart-status">加载中</div>
        ) : kline.error ? (
          <div className="detail-chart-status">加载失败：{kline.error}</div>
        ) : kline.data.length === 0 ? (
          <div className="detail-chart-status">暂无数据</div>
        ) : (
          <KlineCanvas
            data={kline.data}
            mode={isLineMode ? "line" : "candle"}
            height={360}
          />
        )}
      </div>
    </div>
  );
}
