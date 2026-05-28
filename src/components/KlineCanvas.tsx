// KlineCanvas — KLineChart v9 实现版。
//
// Spec: docs/design/frontend-design.md §5 K 线图
//
// 数据流（v9 API）：
//   init(container, options) → Chart
//   chart.applyNewData(dataList, hasMoreOlder)  ← 初始数据
//   chart.setLoadDataCallback({type:'forward'}) ← 用户左拉触发
//   chart.setPriceVolumePrecision(price, vol)
//   chart.setStyles({candle.bar.upColor/downColor + grid + crosshair})
//   chart.createIndicator('VOL')  ← 成交量副图
//
// 组件 owns 整个数据流：useEffect on (tsCode, period) → 重建 chart。
// loading/empty/error 自维护 + overlay 显示。

import { useEffect, useRef, useState } from "react";
import {
  init,
  dispose,
  LoadDataType,
  type Chart,
  type KLineData,
} from "klinecharts";
import {
  commands,
  type FetchInclude,
  type KlinePeriod,
  type MinuteKlinePeriod,
} from "../bindings";

export type ChartPeriod =
  | "intraday"
  | "1m"
  | "5m"
  | "15m"
  | "30m"
  | "60m"
  | "day"
  | "week"
  | "month";

interface KlineCanvasProps {
  tsCode: string;
  period: ChartPeriod;
  pricePrecision?: number;
}

const INITIAL_LIMIT = 500;
const FORWARD_STEP = 300;
const MAX_LIMIT = 2000;
const MINUTE_PERIODS: readonly MinuteKlinePeriod[] = [
  "1m",
  "5m",
  "15m",
  "30m",
  "60m",
] as const;

function isMinutePeriod(p: ChartPeriod): p is MinuteKlinePeriod {
  return (MINUTE_PERIODS as readonly string[]).includes(p);
}

function readCssVar(name: string, fallback: string): string {
  if (typeof window === "undefined") return fallback;
  const v = getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
  return v || fallback;
}

async function fetchKlineData(
  tsCode: string,
  period: ChartPeriod,
  limit: number,
): Promise<KLineData[]> {
  const isIntraday = period === "intraday";
  let include: FetchInclude;
  if (isIntraday) {
    include = { intraday: true };
  } else if (isMinutePeriod(period)) {
    include = { minuteKlines: [period] };
  } else {
    include = { klines: [period as KlinePeriod] };
  }
  const res = await commands.fetchData({
    tsCodes: [tsCode],
    include,
    limit: { kline: limit, minuteKline: limit },
  });
  if (res.status === "error") {
    throw new Error(
      `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
    );
  }
  const items = (res.data.items ?? []) as Array<Record<string, unknown>>;
  const target = items.find((it) => it.tsCode === tsCode);
  if (!target) return [];

  if (isIntraday) {
    const intraday = target.intraday as Record<string, unknown> | undefined;
    if (!intraday) return [];
    const points = intraday.points;
    const tradeDate =
      typeof intraday.tradeDate === "string" ? intraday.tradeDate : undefined;
    if (!Array.isArray(points)) return [];
    return points
      .map((row) => normalizeMinutePoint(row, tradeDate))
      .filter((r): r is KLineData => r !== null);
  }

  if (isMinutePeriod(period)) {
    const group = (target.minuteKlines ?? target.minute_klines) as
      | Record<string, unknown>
      | undefined;
    if (!group) return [];
    const series = group[period] as Record<string, unknown> | undefined;
    if (!series) return [];
    const points = series.points;
    if (!Array.isArray(points)) return [];
    return points
      .map((row) => normalizeMinuteKline(row))
      .filter((r): r is KLineData => r !== null);
  }

  const group = target.klines as Record<string, unknown> | undefined;
  if (!group) return [];
  const series = group[period] as Record<string, unknown> | undefined;
  if (!series) return [];
  const points = series.points;
  if (!Array.isArray(points)) return [];
  return points
    .map((row) => normalizeDayKline(row))
    .filter((r): r is KLineData => r !== null);
}

function toNumber(v: unknown): number | null {
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string" && v.trim() !== "") {
    const n = Number(v);
    if (Number.isFinite(n)) return n;
  }
  return null;
}

function tradeDateToMillis(td: unknown): number | null {
  if (typeof td !== "string" || !/^\d{8}$/.test(td)) return null;
  const y = Number(td.slice(0, 4));
  const m = Number(td.slice(4, 6)) - 1;
  const d = Number(td.slice(6, 8));
  return Date.UTC(y, m, d);
}

function normalizeDayKline(row: unknown): KLineData | null {
  if (!row || typeof row !== "object") return null;
  const r = row as Record<string, unknown>;
  const ts = tradeDateToMillis(r.date ?? r.tradeDate ?? r.trade_date);
  const open = toNumber(r.open);
  const high = toNumber(r.high);
  const low = toNumber(r.low);
  const close = toNumber(r.close);
  if (ts == null || open == null || high == null || low == null || close == null)
    return null;
  return {
    timestamp: ts,
    open,
    high,
    low,
    close,
    volume: toNumber(r.volume) ?? undefined,
    turnover: toNumber(r.amount) ?? undefined,
  };
}

function normalizeMinuteKline(row: unknown): KLineData | null {
  if (!row || typeof row !== "object") return null;
  const r = row as Record<string, unknown>;
  const tsRaw = toNumber(r.timestampMs ?? r.timestamp_ms);
  const open = toNumber(r.open);
  const high = toNumber(r.high);
  const low = toNumber(r.low);
  const close = toNumber(r.close);
  if (tsRaw == null || open == null || high == null || low == null || close == null)
    return null;
  return {
    timestamp: tsRaw,
    open,
    high,
    low,
    close,
    volume: toNumber(r.volume) ?? undefined,
    turnover: toNumber(r.amount) ?? undefined,
  };
}

function normalizeMinutePoint(
  row: unknown,
  tradeDate: string | undefined,
): KLineData | null {
  if (!row || typeof row !== "object") return null;
  const r = row as Record<string, unknown>;
  const timeStr = typeof r.time === "string" ? r.time : null;
  const price = toNumber(r.price);
  if (price == null || !timeStr || !tradeDate || !/^\d{8}$/.test(tradeDate))
    return null;
  const digits = timeStr.replace(/\D/g, "");
  const hh = Number(digits.slice(0, 2));
  const mm = Number(digits.slice(2, 4));
  if (!Number.isFinite(hh) || !Number.isFinite(mm)) return null;
  const y = Number(tradeDate.slice(0, 4));
  const mo = Number(tradeDate.slice(4, 6)) - 1;
  const d = Number(tradeDate.slice(6, 8));
  const ts = Date.UTC(y, mo, d, hh, mm);
  return {
    timestamp: ts,
    open: price,
    high: price,
    low: price,
    close: price,
    volume: toNumber(r.volume) ?? undefined,
  };
}

export function KlineCanvas({
  tsCode,
  period,
  pricePrecision = 2,
}: KlineCanvasProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const chartRef = useRef<Chart | null>(null);
  const [status, setStatus] = useState<"loading" | "empty" | "error" | "ok">(
    "loading",
  );
  const [errorMsg, setErrorMsg] = useState<string | null>(null);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const upColor = readCssVar("--chart-up", "#c0392b");
    const downColor = readCssVar("--chart-down", "#1f8a47");
    const borderSoft = readCssVar("--border-soft", "#efe7d7");
    const fgMuted = readCssVar("--fg-muted", "#897866");

    const chart = init(container, {
      locale: "zh-CN",
      styles: {
        candle: {
          type: "candle_solid",
          bar: {
            upColor,
            downColor,
            noChangeColor: fgMuted,
            upBorderColor: upColor,
            downBorderColor: downColor,
            noChangeBorderColor: fgMuted,
            upWickColor: upColor,
            downWickColor: downColor,
            noChangeWickColor: fgMuted,
          },
          priceMark: {
            high: { color: fgMuted },
            low: { color: fgMuted },
            last: {
              upColor,
              downColor,
              noChangeColor: fgMuted,
              line: { dashedValue: [4, 4] },
            },
          },
        },
        grid: {
          horizontal: { color: borderSoft },
          vertical: { color: borderSoft },
        },
        xAxis: {
          axisLine: { color: borderSoft },
          tickText: { color: fgMuted },
          tickLine: { color: borderSoft },
        },
        yAxis: {
          axisLine: { color: borderSoft },
          tickText: { color: fgMuted },
          tickLine: { color: borderSoft },
        },
        crosshair: {
          horizontal: { line: { color: fgMuted } },
          vertical: { line: { color: fgMuted } },
        },
      },
    });
    if (!chart) {
      setStatus("error");
      setErrorMsg("chart init failed");
      return;
    }
    chartRef.current = chart;
    chart.setPriceVolumePrecision(pricePrecision, 0);
    // 副图：成交量
    chart.createIndicator("VOL", false, { id: "vol_pane" });

    let cancelled = false;
    let currentLimit = INITIAL_LIMIT;
    const loadedTs = new Set<number>();

    const setupLoadMore = () => {
      chart.setLoadDataCallback(({ type, callback }) => {
        if (cancelled) return;
        if (type !== LoadDataType.Forward) {
          callback([], false);
          return;
        }
        if (currentLimit >= MAX_LIMIT) {
          callback([], false);
          return;
        }
        currentLimit = Math.min(currentLimit + FORWARD_STEP, MAX_LIMIT);
        void fetchKlineData(tsCode, period, currentLimit).then(
          (all) => {
            if (cancelled) return;
            const newBars = all.filter((b) => !loadedTs.has(b.timestamp));
            newBars.forEach((b) => loadedTs.add(b.timestamp));
            const stillForward =
              all.length >= currentLimit && currentLimit < MAX_LIMIT;
            callback(newBars, stillForward);
          },
          () => {
            if (!cancelled) callback([], false);
          },
        );
      });
    };

    void (async () => {
      try {
        setStatus("loading");
        setErrorMsg(null);
        let data = await fetchKlineData(tsCode, period, INITIAL_LIMIT);
        if (data.length === 0) {
          // DB 空 → 触发 backend refresh
          const refreshRes = await commands.ensureChartData(tsCode, period);
          if (cancelled) return;
          if (refreshRes.status === "error") {
            setStatus("error");
            setErrorMsg(
              `${refreshRes.error.code}${refreshRes.error.message ? `: ${refreshRes.error.message}` : ""}`,
            );
            return;
          }
          data = await fetchKlineData(tsCode, period, INITIAL_LIMIT);
        }
        if (cancelled) return;
        if (data.length === 0) {
          setStatus("empty");
          return;
        }
        data.forEach((b) => loadedTs.add(b.timestamp));
        chart.applyNewData(data, data.length >= INITIAL_LIMIT);
        setupLoadMore();
        setStatus("ok");
      } catch (e) {
        if (cancelled) return;
        setStatus("error");
        setErrorMsg(String(e));
      }
    })();

    return () => {
      cancelled = true;
      dispose(container);
      chartRef.current = null;
    };
  }, [tsCode, period, pricePrecision]);

  return (
    <div
      style={{
        position: "relative",
        flex: "1 1 auto",
        minHeight: 0,
        width: "100%",
      }}
    >
      <div ref={containerRef} style={{ width: "100%", height: "100%" }} />
      {status === "loading" && (
        <div className="detail-chart-status overlay">加载中</div>
      )}
      {status === "empty" && (
        <div className="detail-chart-status overlay">暂无数据</div>
      )}
      {status === "error" && (
        <div className="detail-chart-status overlay">加载失败：{errorMsg}</div>
      )}
    </div>
  );
}
