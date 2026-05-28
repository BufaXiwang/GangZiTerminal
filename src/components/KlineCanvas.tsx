// KlineCanvas — 纯渲染 K 线 + 成交量 容器。
//
// Spec: docs/design/frontend-design.md §5 K 线图
//
// 设计契约：
// - 不是 wrapper：只接收外部喂来的 candle 数据，自己只负责创建 / 销毁 chart
//   实例 + 调 setData() + ResizeObserver。
// - 周期切换、数据拉取、loading / error 状态由调用方页面控制（参考 useKlineData）。
// - A 股语义：上涨 --chart-up（红），下跌 --chart-down（绿）。

import { useEffect, useRef } from "react";
import {
  CandlestickSeries,
  HistogramSeries,
  LineSeries,
  ColorType,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type CandlestickData,
  type HistogramData,
  type LineData,
  type LogicalRange,
  type UTCTimestamp,
} from "lightweight-charts";
import type { KlineDataPoint } from "../lib/useKlineData";

interface KlineCanvasProps {
  data: KlineDataPoint[];
  /** "candle"（默认）= K 线 + 成交量；"line" = 单线（用于分时 close 价）。 */
  mode?: "candle" | "line";
  /** 容器高度（px）。默认 480。spec §5 要求稳定容器尺寸，不允许内部跳动。 */
  height?: number;
  /**
   * 自适应高度：忽略 height，按父容器实际高度渲染。
   * 注意：父容器必须有 min-height: 0 + flex 约束，否则会塌成 0。
   */
  autoHeight?: boolean;
  /** A 股语义上涨色（默认从 CSS var --chart-up 读取） */
  upColor?: string;
  /** A 股语义下跌色（默认从 CSS var --chart-down 读取） */
  downColor?: string;
  /**
   * 序列标识。当 seriesKey 变化时视为换标的 / 换周期 —— 触发 fitContent 重新对齐
   * 视图；不变时（仅 data 变长）视为 load-more —— 保持用户当前滚动位置。
   */
  seriesKey?: string;
  /**
   * 用户拖到左边附近时触发，调用方可借此追加历史数据。
   * 内部 throttle 1.5s，避免反复 fire。
   */
  onRequestMore?: () => void;
}

function readCssVar(name: string, fallback: string): string {
  if (typeof window === "undefined") return fallback;
  const v = getComputedStyle(document.documentElement)
    .getPropertyValue(name)
    .trim();
  return v || fallback;
}

export function KlineCanvas({
  data,
  mode = "candle",
  height = 480,
  autoHeight = false,
  upColor,
  downColor,
  seriesKey,
  onRequestMore,
}: KlineCanvasProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const candleSeriesRef = useRef<ISeriesApi<"Candlestick"> | null>(null);
  const volumeSeriesRef = useRef<ISeriesApi<"Histogram"> | null>(null);
  const lineSeriesRef = useRef<ISeriesApi<"Line"> | null>(null);
  const roRef = useRef<ResizeObserver | null>(null);
  const prevSeriesKeyRef = useRef<string | undefined>(undefined);
  // onRequestMore stable ref，避免每次 props 重生绑订阅
  const onRequestMoreRef = useRef(onRequestMore);
  useEffect(() => {
    onRequestMoreRef.current = onRequestMore;
  }, [onRequestMore]);

  // 创建 chart（仅在首次 mount 或 height/color 变化时重建）
  useEffect(() => {
    if (!containerRef.current) return;
    const up = upColor ?? readCssVar("--chart-up", "#c0392b");
    const down = downColor ?? readCssVar("--chart-down", "#1f8a47");
    const fgDefault = readCssVar("--fg-default", "#5c5042");
    const fgMuted = readCssVar("--fg-muted", "#897866");
    const borderSoft = readCssVar("--border-soft", "#efe7d7");
    const bgCard = readCssVar("--bg-card", "#ffffff");

    const initialHeight = autoHeight
      ? containerRef.current.clientHeight || height
      : height;
    const chart = createChart(containerRef.current, {
      width: containerRef.current.clientWidth,
      height: initialHeight,
      layout: {
        background: { type: ColorType.Solid, color: bgCard },
        textColor: fgDefault,
        fontFamily:
          "'IBM Plex Mono', 'SF Mono', Menlo, 'Inter', -apple-system, sans-serif",
      },
      grid: {
        vertLines: { color: borderSoft },
        horzLines: { color: borderSoft },
      },
      rightPriceScale: { borderColor: borderSoft },
      timeScale: {
        borderColor: borderSoft,
        timeVisible: true,
        secondsVisible: false,
      },
      crosshair: {
        vertLine: { color: fgMuted, width: 1, style: 2 },
        horzLine: { color: fgMuted, width: 1, style: 2 },
      },
    });

    chartRef.current = chart;

    if (mode === "line") {
      const lineSeries = chart.addSeries(LineSeries, {
        color: up,
        lineWidth: 2,
      });
      lineSeriesRef.current = lineSeries;
    } else {
      const candleSeries = chart.addSeries(CandlestickSeries, {
        upColor: up,
        downColor: down,
        borderUpColor: up,
        borderDownColor: down,
        wickUpColor: up,
        wickDownColor: down,
      });
      // 成交量放在独立 pane（lightweight-charts v5 panes API）
      const volumeSeries = chart.addSeries(
        HistogramSeries,
        {
          priceFormat: { type: "volume" },
          priceScaleId: "",
        },
        1,
      );
      volumeSeries.priceScale().applyOptions({
        scaleMargins: { top: 0.1, bottom: 0 },
      });
      candleSeriesRef.current = candleSeries;
      volumeSeriesRef.current = volumeSeries;
    }

    // ResizeObserver：容器宽度变化自适应；autoHeight 时同时跟踪高度。
    const ro = new ResizeObserver(() => {
      if (!containerRef.current || !chartRef.current) return;
      const opts: { width: number; height?: number } = {
        width: containerRef.current.clientWidth,
      };
      if (autoHeight) {
        opts.height = containerRef.current.clientHeight;
      }
      chartRef.current.applyOptions(opts);
    });
    ro.observe(containerRef.current);
    roRef.current = ro;

    // 监听用户拖到左边 —— 触发 onRequestMore（throttle 1.5s）。
    let lastFiredAt = 0;
    const onRangeChange = (range: LogicalRange | null) => {
      if (!range || !onRequestMoreRef.current) return;
      // range.from 可能为负数（滚出范围）；< 5 视为到达左边沿。
      if (range.from <= 5) {
        const now = Date.now();
        if (now - lastFiredAt > 1500) {
          lastFiredAt = now;
          onRequestMoreRef.current();
        }
      }
    };
    chart.timeScale().subscribeVisibleLogicalRangeChange(onRangeChange);

    return () => {
      chart.timeScale().unsubscribeVisibleLogicalRangeChange(onRangeChange);
      ro.disconnect();
      roRef.current = null;
      chart.remove();
      chartRef.current = null;
      candleSeriesRef.current = null;
      volumeSeriesRef.current = null;
      lineSeriesRef.current = null;
    };
  }, [height, autoHeight, upColor, downColor, mode]);

  // 数据更新
  useEffect(() => {
    const up = upColor ?? readCssVar("--chart-up", "#c0392b");
    const down = downColor ?? readCssVar("--chart-down", "#1f8a47");

    // seriesKey 变了 → 换标的/周期，需要 fitContent 重新对齐；
    // 没变（仅 data 变长）→ load-more，保持用户滚动位置。
    const isNewSeries = prevSeriesKeyRef.current !== seriesKey;
    prevSeriesKeyRef.current = seriesKey;

    if (mode === "line") {
      const line = lineSeriesRef.current;
      if (!line) return;
      const lineData: LineData<UTCTimestamp>[] = data.map((d) => ({
        time: d.time as UTCTimestamp,
        value: d.close,
      }));
      line.setData(lineData);
      if (data.length > 0 && isNewSeries) chartRef.current?.timeScale().fitContent();
      return;
    }

    const candle = candleSeriesRef.current;
    const volume = volumeSeriesRef.current;
    if (!candle || !volume) return;

    const candleData: CandlestickData<UTCTimestamp>[] = data.map((d) => ({
      time: d.time as UTCTimestamp,
      open: d.open,
      high: d.high,
      low: d.low,
      close: d.close,
    }));
    const volumeData: HistogramData<UTCTimestamp>[] = data.map((d) => ({
      time: d.time as UTCTimestamp,
      value: d.volume ?? 0,
      color: d.close >= d.open ? up : down,
    }));
    candle.setData(candleData);
    volume.setData(volumeData);
    if (data.length > 0 && isNewSeries) chartRef.current?.timeScale().fitContent();
  }, [data, mode, upColor, downColor, seriesKey]);

  return (
    <div
      ref={containerRef}
      style={{
        width: "100%",
        height: autoHeight ? "100%" : height,
        flex: autoHeight ? "1 1 auto" : undefined,
        minHeight: 0,
        background: "var(--bg-card)",
        border: "1px solid var(--border-default)",
        borderRadius: "var(--radius-sm)",
      }}
    />
  );
}
