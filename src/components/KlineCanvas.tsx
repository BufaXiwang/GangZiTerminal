// KlineCanvas — KLineChart 实现版（替代 lightweight-charts）。
//
// Spec: docs/design/frontend-design.md §5 K 线图
//
// 优势 vs lightweight-charts：
// - 内置 setDataLoader（type='init'/'forward' 自动驱动 load-more 左拉加载历史）
// - 内置 30+ 技术指标（MA / MACD / KDJ / BOLL / RSI 等）
// - 默认 A 股红涨绿跌（通过 styles 覆盖）
// - 内置中文 locale
//
// 设计：组件 owns 整个数据流：
//   useEffect on (tsCode, period) → init chart + setDataLoader
//   getBars callback 内部：
//     'init' → 调 fetch_data；空就 ensure_chart_data + 重试
//     'forward' → 增大 limit 重拉，给出 prepended slice
// loading/error 自维护并覆盖显示。

import { useEffect, useRef, useState } from "react";
import {
  init,
  dispose,
  type Chart,
  type KLineData,
  type Period,
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
  /** 标的 ts_code（带市场后缀 e.g. 000001.SH）*/
  tsCode: string;
  /** 周期 */
  period: ChartPeriod;
  /** 价格精度（指数 2，股票 2，基金 3）。可省略，默认 2 */
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

function periodToKLineChart(p: ChartPeriod): Period {
  switch (p) {
    case "intraday":
      return { type: "minute", span: 1 };
    case "1m":
      return { type: "minute", span: 1 };
    case "5m":
      return { type: "minute", span: 5 };
    case "15m":
      return { type: "minute", span: 15 };
    case "30m":
      return { type: "minute", span: 30 };
    case "60m":
      return { type: "hour", span: 1 };
    case "day":
      return { type: "day", span: 1 };
    case "week":
      return { type: "week", span: 1 };
    case "month":
      return { type: "month", span: 1 };
  }
}

function readCssVar(name: string, fallback: string): string {
  if (typeof window === "undefined") return fallback;
  const v = getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
  return v || fallback;
}

// 后端 fetch_data 返回 → KLineData[]
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
    if (!containerRef.current) return;
    const upColor = readCssVar("--chart-up", "#c0392b");
    const downColor = readCssVar("--chart-down", "#1f8a47");
    const bgCard = readCssVar("--bg-card", "#ffffff");
    const borderSoft = readCssVar("--border-soft", "#efe7d7");
    const fgDefault = readCssVar("--fg-default", "#5c5042");
    const fgMuted = readCssVar("--fg-muted", "#897866");

    const chart = init(containerRef.current, {
      locale: "zh-CN",
      styles: {
        candle: {
          bar: {
            upColor,
            downColor,
            upBorderColor: upColor,
            downBorderColor: downColor,
            upWickColor: upColor,
            downWickColor: downColor,
          },
        },
        grid: {
          horizontal: { color: borderSoft },
          vertical: { color: borderSoft },
        },
        xAxis: { axisLine: { color: borderSoft }, tickText: { color: fgMuted } },
        yAxis: { axisLine: { color: borderSoft }, tickText: { color: fgMuted } },
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
    chart.setSymbol({ ticker: tsCode, pricePrecision, volumePrecision: 0 });
    chart.setPeriod(periodToKLineChart(period));

    // 用 closure 跟踪当前 limit + 已加载的 timestamps（避免重复）
    let currentLimit = INITIAL_LIMIT;
    const loadedTs = new Set<number>();
    let cancelled = false;

    chart.setDataLoader({
      getBars: async ({ type, callback }) => {
        if (cancelled) return;
        if (type === "init") {
          setStatus("loading");
          setErrorMsg(null);
          try {
            let data = await fetchKlineData(tsCode, period, currentLimit);
            if (data.length === 0) {
              // DB 空 → 触发后端 refresh，再读
              const refreshRes = await commands.ensureChartData(tsCode, period);
              if (cancelled) return;
              if (refreshRes.status === "error") {
                setStatus("error");
                setErrorMsg(
                  `${refreshRes.error.code}${refreshRes.error.message ? `: ${refreshRes.error.message}` : ""}`,
                );
                callback([], false);
                return;
              }
              data = await fetchKlineData(tsCode, period, currentLimit);
            }
            if (cancelled) return;
            if (data.length === 0) {
              setStatus("empty");
              callback([], false);
              return;
            }
            data.forEach((b) => loadedTs.add(b.timestamp));
            setStatus("ok");
            callback(data, { forward: data.length >= currentLimit });
          } catch (e) {
            if (cancelled) return;
            setStatus("error");
            setErrorMsg(String(e));
            callback([], false);
          }
        } else if (type === "forward") {
          // 用户左拉到尽头 → 加大 limit 再取，给出 prepended slice
          if (currentLimit >= MAX_LIMIT) {
            callback([], false);
            return;
          }
          currentLimit = Math.min(currentLimit + FORWARD_STEP, MAX_LIMIT);
          try {
            const all = await fetchKlineData(tsCode, period, currentLimit);
            if (cancelled) return;
            const newBars = all.filter((b) => !loadedTs.has(b.timestamp));
            newBars.forEach((b) => loadedTs.add(b.timestamp));
            const stillForward =
              all.length >= currentLimit && currentLimit < MAX_LIMIT;
            callback(newBars, { forward: stillForward });
          } catch (e) {
            if (cancelled) return;
            callback([], false);
          }
        } else {
          // backward (newer) / update — 我们不主动推送
          callback([], false);
        }
      },
    });

    return () => {
      cancelled = true;
      if (containerRef.current) dispose(containerRef.current);
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
