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
  CandleType,
  PolygonType,
  LineType,
  type Chart,
  type KLineData,
} from "klinecharts";
import {
  commands,
  type FetchInclude,
  type KlinePeriod,
  type MinuteKlinePeriod,
} from "../bindings";
import { perf } from "../lib/perfLog";
import { isContinuousAuction } from "../lib/tradingSession";

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

// 渐进式加载：先用 INITIAL_LIMIT 喂首屏（覆盖后端 ensure_chart_data 拉的首 800 根），
// 然后后台 loop fetchKlinePage(800, 1600, ...) 不断 applyMoreData 把更早历史补上。
const INITIAL_LIMIT = 2000;
/** 单页大小 — 与后端 fetch_kline_page 一致，TDX 协议固定 800 根 */
const PAGE_SIZE = 800;
/** 安全上限：A 股最老股 ~8500 日 = 11 页；20 页绰绰有余 */
const MAX_PAGES = 20;

// 进程内 cache：记录已经 ensureChartData 过的 (tsCode, period)，避免重复触发后端。
const ensuredKeys = new Set<string>();
// 已完成全量分页加载的 (tsCode, period) — 不再触发 background pagination loop
const fullyLoadedKeys = new Set<string>();
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
  // 当前已显示数据的最大时间戳；盘中轮询只用 updateData 喂 >= 它的尾部 bar，
  // 避免覆盖错位（progressive 历史 loop 只 prepend 更早 bar，不动这个最大值）。
  const lastTsRef = useRef<number>(0);
  const [status, setStatus] = useState<"loading" | "empty" | "error" | "ok">(
    "loading",
  );
  const [errorMsg, setErrorMsg] = useState<string | null>(null);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const mountedAt = performance.now();
    perf(`KlineCanvas mount tsCode=${tsCode} period=${period}`);

    const upColor = readCssVar("--chart-up", "#c0392b");
    const downColor = readCssVar("--chart-down", "#1f8a47");
    const borderSoft = readCssVar("--border-soft", "#efe7d7");
    const fgMuted = readCssVar("--fg-muted", "#897866");

    const chart = init(container, {
      locale: "zh-CN",
      styles: {
        candle: {
          type: CandleType.CandleSolid,
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
        indicator: {
          // VOL 副图 + 其他 bar 类指标颜色按 A 股语义红涨绿跌（默认 klinecharts 是反的）
          bars: [
            {
              style: PolygonType.Fill,
              borderStyle: LineType.Solid,
              borderSize: 1,
              upColor,
              downColor,
              noChangeColor: fgMuted,
            },
          ],
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

    // 容器尺寸变化时让图表重算布局（窗口 / 面板 resize；以及万一在 0 尺寸下 init 后获得尺寸）。
    // 注：页面懒挂载（App.tsx）已保证图表首次在可见状态下 init，这里是兜底 + 处理后续 resize。
    const resizeObserver = new ResizeObserver(() => {
      chartRef.current?.resize();
    });
    resizeObserver.observe(container);

    let cancelled = false;

    void (async () => {
      try {
        setStatus("loading");
        setErrorMsg(null);
        const cacheKey = `${tsCode}|${period}`;
        // 1. 已 ensure 过 → 跳过 backend；否则触发后端拉首页（仅 ~800ms）
        if (!ensuredKeys.has(cacheKey)) {
          const ensureRes = await commands.ensureChartData(tsCode, period);
          if (cancelled) return;
          if (ensureRes.status === "error") {
            setStatus("error");
            setErrorMsg(
              `${ensureRes.error.code}${ensureRes.error.message ? `: ${ensureRes.error.message}` : ""}`,
            );
            return;
          }
          ensuredKeys.add(cacheKey);
        }
        // 2. 读 DB → 首屏渲染（~800 根 day/week/month；分钟 K 一次拉到）
        const initialData = await fetchKlineData(tsCode, period, INITIAL_LIMIT);
        if (cancelled) return;
        if (initialData.length === 0) {
          setStatus("empty");
          return;
        }
        const shownTs = new Set<number>();
        initialData.forEach((b) => shownTs.add(b.timestamp));
        lastTsRef.current = initialData.reduce(
          (mx, b) => Math.max(mx, b.timestamp),
          0,
        );
        chart.applyNewData(initialData, true); // more=true: 表示可能还有更早数据
        setStatus("ok");

        // 3. 渐进式 background loop：仅 day/week/month 走分页；
        //    每页 fetch_kline_page → DB upsert → 前端 re-read diff → applyMoreData。
        //    fullyLoadedKeys 防重复 loop 同一标的同周期。
        const isPaginatableK =
          period === "day" || period === "week" || period === "month";
        if (isPaginatableK && !fullyLoadedKeys.has(cacheKey)) {
          let pageIndex = 1; // 0 已被 ensure 拉过
          while (!cancelled && pageIndex < MAX_PAGES) {
            const offset = pageIndex * PAGE_SIZE;
            const pageRes = await commands.fetchKlinePage(
              tsCode,
              period,
              offset,
            );
            if (cancelled) return;
            if (pageRes.status === "error") break;
            const { added, hasMore } = pageRes.data;
            if (added === 0) break;
            // 重新读 DB 拿全量，挑出新 bars（之前没显示过）
            const allData = await fetchKlineData(
              tsCode,
              period,
              (pageIndex + 1) * PAGE_SIZE + INITIAL_LIMIT,
            );
            if (cancelled) return;
            const newBars = allData.filter((b) => !shownTs.has(b.timestamp));
            if (newBars.length > 0) {
              newBars.forEach((b) => shownTs.add(b.timestamp));
              // applyMoreData(olderBars) 把更早 bars prepend，保留用户滚动位置
              chart.applyMoreData(newBars, hasMore);
            } else {
              // 后端有新行但前端没拿到 → 异常，停
              break;
            }
            if (!hasMore) break;
            pageIndex++;
          }
          if (!cancelled) fullyLoadedKeys.add(cacheKey);
        }
      } catch (e) {
        if (cancelled) return;
        setStatus("error");
        setErrorMsg(String(e));
      }
    })();

    return () => {
      cancelled = true;
      resizeObserver.disconnect();
      const tBeforeDispose = performance.now();
      dispose(container);
      perf(
        `KlineCanvas dispose tsCode=${tsCode} period=${period} took ${(performance.now() - tBeforeDispose).toFixed(1)}ms; lived ${(performance.now() - mountedAt).toFixed(1)}ms`,
      );
      chartRef.current = null;
    };
  }, [tsCode, period, pricePrecision]);

  // 盘中近实时轮询：每 15s 触发后端重拉最新 bar（minute → refresh_minute_klines
  // 增量；day → fetch_kline_page(0) 拉今日），再读尾部用 updateData merge
  // （时间戳 == 末根 → 更新当前 bar；> 末根 → append 新 bar）。
  // 只在交易时段轮询；分时(intraday)已下线不轮询。
  useEffect(() => {
    if (period === "intraday") return;
    let cancelled = false;
    const POLL_MS = 15_000;
    const tick = async () => {
      if (cancelled || !chartRef.current || !isContinuousAuction()) return;
      try {
        // 强制后端重拉最新（绕过 ensuredKeys —— 那只防首次重复触发）
        const ensureRes = await commands.ensureChartData(tsCode, period);
        if (cancelled || !chartRef.current) return;
        if (ensureRes.status === "error") return;
        // 读最近 ~16 根，只 merge >= 当前最大时间戳的尾部
        const latest = await fetchKlineData(tsCode, period, 16);
        if (cancelled || !chartRef.current) return;
        const tail = latest
          .filter((b) => b.timestamp >= lastTsRef.current)
          .sort((a, b) => a.timestamp - b.timestamp);
        for (const bar of tail) {
          chartRef.current.updateData(bar);
        }
        if (tail.length > 0) {
          lastTsRef.current = tail[tail.length - 1].timestamp;
        }
      } catch {
        // 轮询失败静默忽略，下一 tick 再试
      }
    };
    const timer = window.setInterval(() => void tick(), POLL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
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
