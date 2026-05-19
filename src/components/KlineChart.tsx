import { invoke } from "@tauri-apps/api/core";
import {
  CandlestickSeries,
  createChart,
  HistogramSeries,
  LineSeries,
  type IChartApi,
  type Time,
} from "lightweight-charts";
import {
  dispose as disposeKChart,
  init as initKChart,
  type Chart as KChart,
  type KLineData,
} from "klinecharts";
import { Camera, ChevronDown, Eraser, Maximize2, Loader2, PenLine } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import type {
  InstrumentCategory,
  KlinePeriod,
  KlinePoint,
  MinuteKlinePoint,
  MinutePoint,
} from "../types";

type Props = {
  /** 6 位 code（用户显示） */
  code: string;
  /** 带后缀的 ts_code（"000001.SZ" / "510300.SH" / "920469.BJ"）—— 后端调用 key */
  tsCode: string;
  name: string;
  /** "stock" / "index" / "fund"——后端按这个分流到不同 TuShare 接口 */
  category?: InstrumentCategory;
  /** 可选：当前 quote meta */
  meta?: {
    price: number | null | undefined;
    changePercent: number | null | undefined;
    amount: number | null | undefined;
    low: number | null | undefined;
    high: number | null | undefined;
  };
};

type SeriesHandle = { setData: (data: unknown[]) => void };

// ===== Period 定义 ========================================================
// 8 个：分时（EM trends2）/ 1m-60m 分钟级（EM klines）/ 日周月（TuShare 历史）
const periods: Array<{ label: string; value: KlinePeriod }> = [
  { label: "分时", value: "minute" },
  { label: "1m", value: "1m" },
  { label: "5m", value: "5m" },
  { label: "15m", value: "15m" },
  { label: "60m", value: "60m" },
  { label: "日", value: "day" },
  { label: "周", value: "week" },
  { label: "月", value: "month" },
];

const MINUTE_K_PERIODS = new Set<KlinePeriod>(["1m", "5m", "15m", "60m"]);

// ===== Indicator 菜单定义 =================================================
type IndicatorSpec = { id: string; label: string; calcParams?: number[] };

const MAIN_OVERLAYS: IndicatorSpec[] = [
  { id: "MA", label: "MA(5,10,20,60)", calcParams: [5, 10, 20, 60] },
  { id: "EMA", label: "EMA(5,10,20)", calcParams: [5, 10, 20] },
  { id: "BOLL", label: "BOLL(20,2)" },
  { id: "BBI", label: "BBI" },
  { id: "SAR", label: "SAR" },
];

const SUB_INDICATORS: IndicatorSpec[] = [
  { id: "VOL", label: "VOL" },
  { id: "MACD", label: "MACD(12,26,9)" },
  { id: "KDJ", label: "KDJ(9,3,3)" },
  { id: "RSI", label: "RSI(6,12,24)" },
  { id: "CCI", label: "CCI(14)" },
  { id: "BIAS", label: "BIAS(6,12,24)" },
  { id: "WR", label: "WR(6,10,14)" },
  { id: "DMI", label: "DMI(14)" },
  { id: "OBV", label: "OBV" },
];

const DEFAULT_MAIN = new Set(["MA"]);
const DEFAULT_SUB = new Set(["VOL", "MACD"]);

// ===== 画线工具 ============================================================
// klinecharts v10 内置 overlay 类型。
// 用户从下拉菜单选一个 → chart.createOverlay({ name }) → 鼠标在图上点击放置。
// **不持久化**：切标的 / 切周期时 chart 重建，画的线丢失。
type DrawingTool = { id: string; label: string };

const DRAWING_TOOLS: DrawingTool[] = [
  { id: "priceLine", label: "水平价格线" },
  { id: "straightLine", label: "趋势线（两点）" },
  { id: "segment", label: "线段（两点）" },
  { id: "parallelStraightLine", label: "平行通道" },
  { id: "priceChannelLine", label: "价格通道" },
  { id: "fibonacciLine", label: "斐波那契回撤" },
  { id: "rect", label: "矩形框" },
  { id: "simpleAnnotation", label: "文字标注" },
];

// ===== Cache（period 切换间快速展示） =====================================
type ChartBar = {
  timestamp: number; // ms
  open: number;
  high: number;
  low: number;
  close: number;
  volume: number;
  amount: number;
};

const barCache = new Map<string, { data: ChartBar[]; fetchedAt: number }>();
const minuteCache = new Map<string, { data: MinutePoint[]; fetchedAt: number }>();
const cacheTtlMs: Record<KlinePeriod, number> = {
  minute: 30_000,
  "1m": 30_000,
  "5m": 30_000,
  "15m": 30_000,
  "60m": 60_000,
  day: 5 * 60_000,
  week: 30 * 60_000,
  month: 60 * 60_000,
};

// **必须用 tsCode**——平安银行 000001.SZ 和 上证指数 000001.SH 同 6 位 code 不同标的。
function cacheKey(tsCode: string, period: KlinePeriod, limit: number): string {
  return `${tsCode}:${period}:${limit}`;
}

// ===== 主组件 ==============================================================

export function KlineChart({ code, tsCode, name, category = "stock", meta }: Props) {
  const [period, setPeriod] = useState<KlinePeriod>("minute");
  const [bars, setBars] = useState<ChartBar[]>([]);
  const [minutePoints, setMinutePoints] = useState<MinutePoint[]>([]);
  const klineLimit = 500;
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [error, setError] = useState("");
  const [mainOverlays, setMainOverlays] = useState<Set<string>>(DEFAULT_MAIN);
  const [subIndicators, setSubIndicators] = useState<Set<string>>(DEFAULT_SUB);
  const [indicatorMenuOpen, setIndicatorMenuOpen] = useState(false);
  const [drawingMenuOpen, setDrawingMenuOpen] = useState(false);
  const [screenshotHint, setScreenshotHint] = useState<string | null>(null);
  // chart 重建次数——onChartReady 触发时递增；让"画线 load/save"effect 在每次 chart
  // 重建后重新跑。chart 实例本身在 ref 里，不能放 effect deps。
  const [chartReadyTick, setChartReadyTick] = useState(0);
  const panelRef = useRef<HTMLElement | null>(null);
  const chartHandleRef = useRef<KChart | null>(null);

  // 画线持久化——按 (tsCode, period) 一份。切标的 / 切周期时：
  // 1. cleanup：dump 当前用户画的 overlay 写 app_state
  // 2. setup：load 上次存的 overlay 重新 createOverlay
  // 分时（minute）不支持画线，跳过。
  useEffect(() => {
    if (period === "minute") return;
    const key = `gangzi-terminal.kline-drawings:${tsCode}:${period}`;
    const allowedNames = new Set(DRAWING_TOOLS.map((t) => t.id));

    void invoke<unknown>("load_app_state", { key }).then((stored) => {
      const chart = chartHandleRef.current;
      if (!chart || !Array.isArray(stored)) return;
      for (const overlay of stored as Array<{ name?: string; points?: unknown }>) {
        if (!overlay?.name || !allowedNames.has(overlay.name)) continue;
        try {
          (chart as unknown as { createOverlay: (o: unknown) => void }).createOverlay(overlay);
        } catch {
          // 单个 overlay 反序列化失败不阻塞其它
        }
      }
    });

    return () => {
      const chart = chartHandleRef.current;
      if (!chart) return;
      try {
        const getOverlays = (chart as unknown as { getOverlays?: () => unknown[] }).getOverlays;
        if (typeof getOverlays !== "function") return;
        const overlays = getOverlays.call(chart) ?? [];
        const userOverlays = (overlays as Array<{ name?: string; points?: unknown }>)
          .filter((o) => o?.name && allowedNames.has(o.name))
          .map((o) => ({ name: o.name, points: o.points }));
        void invoke("save_app_state", { key, value: userOverlays });
      } catch {
        // chart 已 dispose 等异常 —— 静默
      }
    };
  }, [tsCode, period, chartReadyTick]);

  // 切换标的时重置数据
  useEffect(() => {
    setBars([]);
    setMinutePoints([]);
    setStatus("loading");
    setError("");
  }, [tsCode]);

  // 拉数据 —— 三条分支：分时 / 分钟 K / 日周月 K
  useEffect(() => {
    let cancelled = false;
    setError("");

    if (period === "minute") {
      const key = `${tsCode}:minute`;
      const cached = minuteCache.get(key);
      if (cached && Date.now() - cached.fetchedAt < cacheTtlMs.minute) {
        setMinutePoints(cached.data);
        setStatus("ready");
        return;
      }
      if (minutePoints.length === 0) setStatus("loading");
      void invoke<MinutePoint[]>("fetch_a_share_minutes", { tsCode, days: 1 })
        .then((data) => {
          if (cancelled) return;
          minuteCache.set(key, { data, fetchedAt: Date.now() });
          setMinutePoints(data);
          setStatus("ready");
        })
        .catch((err) => {
          if (cancelled) return;
          setError(err instanceof Error ? err.message : String(err));
          setStatus("error");
        });
      return () => {
        cancelled = true;
      };
    }

    const key = cacheKey(tsCode, period, klineLimit);
    const cached = barCache.get(key);
    if (cached && Date.now() - cached.fetchedAt < cacheTtlMs[period]) {
      setBars(cached.data);
      setStatus("ready");
      return;
    }
    if (bars.length === 0) setStatus("loading");

    const promise = MINUTE_K_PERIODS.has(period)
      ? invoke<MinuteKlinePoint[]>("fetch_minute_klines", {
          tsCode,
          period,
          limit: klineLimit,
        }).then((rows) => rows.map(minuteKlineToBar))
      : invoke<KlinePoint[]>("fetch_a_share_klines", {
          tsCode,
          period,
          limit: klineLimit,
          category,
        }).then((rows) => rows.map(klinePointToBar));

    void promise
      .then((data) => {
        if (cancelled) return;
        barCache.set(key, { data, fetchedAt: Date.now() });
        setBars(data);
        setStatus("ready");
      })
      .catch((err) => {
        if (cancelled) return;
        setError(err instanceof Error ? err.message : String(err));
        setStatus("error");
      });

    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [code, tsCode, period, category]);

  // 菜单 outside click 关闭
  useEffect(() => {
    if (!indicatorMenuOpen && !drawingMenuOpen) return;
    const handler = (e: MouseEvent) => {
      if (!(e.target instanceof Element)) return;
      if (
        !e.target.closest(
          ".kline-indicator-menu, .kline-drawing-menu, .kline-toolbar-btn.indicator, .kline-toolbar-btn.drawing",
        )
      ) {
        setIndicatorMenuOpen(false);
        setDrawingMenuOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [indicatorMenuOpen, drawingMenuOpen]);

  const hasData = period === "minute" ? minutePoints.length > 0 : bars.length > 0;

  function toggleMain(id: string) {
    setMainOverlays((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }
  function toggleSub(id: string) {
    setSubIndicators((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  async function handleFullscreen() {
    const el = panelRef.current;
    if (!el) return;
    // Tauri 2 的 macOS WKWebView 不开启 requestFullscreen API——直接调会 reject
    // 一个 "Fullscreen request denied" 错。改用 CSS-based 全屏（panelRef 切 class，
    // position:fixed 占满整个 app 窗口）。原生 API 能用就先用，不能用走 CSS 兜底。
    try {
      if (document.fullscreenElement) {
        await document.exitFullscreen();
        return;
      }
      // 优先试原生
      if (typeof el.requestFullscreen === "function") {
        await el.requestFullscreen();
        return;
      }
      throw new Error("no native fullscreen");
    } catch {
      // CSS 兜底——toggle 一个 class，由 styles.css 用 position:fixed 占满窗口
      el.classList.toggle("kline-panel-fullscreen");
    }
  }

  async function handleScreenshot() {
    const chart = chartHandleRef.current;
    if (!chart) return;
    const dataUrl = chart.getConvertPictureUrl(true, "png", "#ffffff");
    // 之前用 <a download> 触发下载——Tauri 2 的 macOS WKWebView 对这种伪下载
    // 链接 click() 有时静默失败（用户报告"截图不行了"）。改成剪贴板优先：
    // 1) 截了直接 Cmd+V 能粘到 chat 输入框 / 别的 app，体验更顺
    // 2) 剪贴板失败再回退到 download path 兜底
    try {
      const blob = await (await fetch(dataUrl)).blob();
      // Clipboard API 写图片需要 secure context 和支持的浏览器——WKWebView 16+ OK
      await navigator.clipboard.write([new ClipboardItem({ "image/png": blob })]);
      setScreenshotHint("已复制到剪贴板");
      window.setTimeout(() => setScreenshotHint(null), 1800);
    } catch (e) {
      console.warn("剪贴板写入失败，回退到下载", e);
      const a = document.createElement("a");
      a.download = `${code}-${period}-${Date.now()}.png`;
      a.href = dataUrl;
      document.body.appendChild(a);
      a.click();
      document.body.removeChild(a);
      setScreenshotHint("已下载到 Downloads");
      window.setTimeout(() => setScreenshotHint(null), 1800);
    }
  }

  function handleStartDrawing(tool: string) {
    const chart = chartHandleRef.current;
    if (!chart) return;
    chart.createOverlay({ name: tool });
    setDrawingMenuOpen(false);
  }

  function handleClearOverlays() {
    const chart = chartHandleRef.current;
    if (!chart) return;
    // klinecharts v10：不带参数 = 移除全部用户画的 overlay（不动 indicator）
    chart.removeOverlay();
    setDrawingMenuOpen(false);
    // 同步清空 KV，避免下次切回时 restore 出来
    if (period !== "minute") {
      const key = `gangzi-terminal.kline-drawings:${tsCode}:${period}`;
      void invoke("save_app_state", { key, value: [] });
    }
  }

  const isMinuteView = period === "minute";

  return (
    <section className="kline-panel" ref={panelRef}>
      <div className="kline-head">
        <div className="kline-head-title">
          <strong>{name}</strong>
          <span>{code}</span>
        </div>
        {meta && (
          <div className="kline-meta">
            <span>
              最新 <strong>{formatMetaNumber(meta.price)}</strong>
            </span>
            <em
              className={
                typeof meta.changePercent === "number" && meta.changePercent < 0 ? "down" : "up"
              }
            >
              {formatMetaSigned(meta.changePercent)}%
            </em>
            <span>成交 {formatMetaAmount(meta.amount)}</span>
            <span>
              日内 {formatMetaNumber(meta.low)} / {formatMetaNumber(meta.high)}
            </span>
          </div>
        )}
        <div className="kline-head-actions">
          <div className="kline-tabs">
            {periods.map((item) => (
              <button
                className={period === item.value ? "active" : ""}
                key={item.value}
                type="button"
                onClick={() => setPeriod(item.value)}
              >
                {item.label}
              </button>
            ))}
          </div>
          <div className="kline-toolbar">
            <button
              type="button"
              className="kline-toolbar-btn indicator"
              onClick={() => {
                setIndicatorMenuOpen((x) => !x);
                setDrawingMenuOpen(false);
              }}
              title="指标"
            >
              指标 <ChevronDown size={12} />
            </button>
            {!isMinuteView && (
              <button
                type="button"
                className="kline-toolbar-btn drawing"
                onClick={() => {
                  setDrawingMenuOpen((x) => !x);
                  setIndicatorMenuOpen(false);
                }}
                title="画线"
              >
                <PenLine size={14} /> <ChevronDown size={12} />
              </button>
            )}
            <button
              type="button"
              className="kline-toolbar-btn"
              onClick={() => void handleScreenshot()}
              title="截图（保存到剪贴板，Cmd+V 直接粘）"
            >
              <Camera size={14} />
            </button>
            {screenshotHint && <span className="kline-toolbar-hint">{screenshotHint}</span>}
            <button type="button" className="kline-toolbar-btn" onClick={handleFullscreen} title="全屏">
              <Maximize2 size={14} />
            </button>
            {indicatorMenuOpen && (
              <div className="kline-indicator-menu">
                <div className="kline-indicator-group">
                  <div className="kline-indicator-group-title">主图叠加</div>
                  {MAIN_OVERLAYS.map((spec) => (
                    <label key={spec.id} className="kline-indicator-item">
                      <input
                        type="checkbox"
                        checked={mainOverlays.has(spec.id)}
                        onChange={() => toggleMain(spec.id)}
                      />
                      <span>{spec.label}</span>
                    </label>
                  ))}
                </div>
                <div className="kline-indicator-group">
                  <div className="kline-indicator-group-title">副图指标</div>
                  {SUB_INDICATORS.map((spec) => (
                    <label key={spec.id} className="kline-indicator-item">
                      <input
                        type="checkbox"
                        checked={subIndicators.has(spec.id)}
                        onChange={() => toggleSub(spec.id)}
                      />
                      <span>{spec.label}</span>
                    </label>
                  ))}
                </div>
              </div>
            )}
            {drawingMenuOpen && (
              <div className="kline-drawing-menu">
                {DRAWING_TOOLS.map((tool) => (
                  <button
                    key={tool.id}
                    type="button"
                    className="kline-drawing-item"
                    onClick={() => handleStartDrawing(tool.id)}
                  >
                    {tool.label}
                  </button>
                ))}
                <div className="kline-drawing-divider" />
                <button
                  type="button"
                  className="kline-drawing-item danger"
                  onClick={handleClearOverlays}
                >
                  <Eraser size={12} /> 清除所有画线
                </button>
              </div>
            )}
          </div>
        </div>
      </div>

      {status === "loading" && !hasData ? (
        <div className="kline-state">
          <Loader2 className="spin" size={20} />
          <span>正在加载行情图。</span>
        </div>
      ) : status === "error" ? (
        <div className="kline-state">
          <span>{error || "行情图加载失败。"}</span>
        </div>
      ) : isMinuteView ? (
        <LightweightMinuteChart key={`${tsCode}:minute`} points={minutePoints} />
      ) : (
        <KLineCandlestickChart
          key={`${tsCode}:kline`}
          bars={bars}
          period={period}
          mainOverlays={mainOverlays}
          subIndicators={subIndicators}
          onChartReady={(chart) => {
            chartHandleRef.current = chart;
            // 触发画线 load/save effect 重跑
            setChartReadyTick((t) => t + 1);
          }}
        />
      )}
    </section>
  );
}

// ===== K 线 candle 子组件 ===================================================

function KLineCandlestickChart({
  bars,
  period,
  mainOverlays,
  subIndicators,
  onChartReady,
}: {
  bars: ChartBar[];
  period: KlinePeriod;
  mainOverlays: Set<string>;
  subIndicators: Set<string>;
  onChartReady: (chart: KChart) => void;
}) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<KChart | null>(null);
  const barsRef = useRef<KLineData[]>([]);
  barsRef.current = bars.map(barToKLineData);
  const mainIdRef = useRef<Map<string, string>>(new Map());
  const subIdRef = useRef<Map<string, string>>(new Map());

  useEffect(() => {
    if (!containerRef.current) return;
    disposeKChart(containerRef.current);
    containerRef.current.innerHTML = "";
    const chart = initKChart(containerRef.current, {
      locale: "zh-CN",
      styles: chartStyles(),
    });
    if (!chart) return;
    chart.setSymbol({ ticker: "" });
    chart.setPeriod(periodToKChartPeriod(period));
    chart.setDataLoader({
      getBars: ({ callback }) => {
        callback(barsRef.current, false);
      },
    });
    chartRef.current = chart;
    onChartReady(chart);

    syncMainOverlays(chart, mainIdRef.current, mainOverlays);
    syncSubIndicators(chart, subIdRef.current, subIndicators);

    return () => {
      if (containerRef.current) disposeKChart(containerRef.current);
      chartRef.current = null;
      mainIdRef.current.clear();
      subIdRef.current.clear();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const chart = chartRef.current;
    if (!chart) return;
    chart.setPeriod(periodToKChartPeriod(period));
  }, [period]);

  useEffect(() => {
    const chart = chartRef.current;
    if (!chart) return;
    chart.resetData();
  }, [bars]);

  useEffect(() => {
    const chart = chartRef.current;
    if (!chart) return;
    syncMainOverlays(chart, mainIdRef.current, mainOverlays);
  }, [mainOverlays]);

  useEffect(() => {
    const chart = chartRef.current;
    if (!chart) return;
    syncSubIndicators(chart, subIdRef.current, subIndicators);
  }, [subIndicators]);

  return (
    <div
      className="klinecharts-container"
      ref={containerRef}
      style={{ width: "100%", flex: "1 1 0", minHeight: 0 }}
    />
  );
}

function syncMainOverlays(chart: KChart, ids: Map<string, string>, want: Set<string>) {
  for (const [id, paneId] of ids) {
    if (!want.has(id)) {
      chart.removeIndicator({ name: id, paneId });
      ids.delete(id);
    }
  }
  for (const id of want) {
    if (ids.has(id)) continue;
    const spec = MAIN_OVERLAYS.find((s) => s.id === id);
    if (!spec) continue;
    chart.createIndicator(
      spec.calcParams ? { name: id, calcParams: spec.calcParams } : id,
      false,
      { id: "candle_pane" },
    );
    ids.set(id, "candle_pane");
  }
}

function syncSubIndicators(chart: KChart, ids: Map<string, string>, want: Set<string>) {
  for (const [id, paneId] of ids) {
    if (!want.has(id)) {
      chart.removeIndicator({ name: id, paneId });
      ids.delete(id);
    }
  }
  for (const id of want) {
    if (ids.has(id)) continue;
    const newPaneId = chart.createIndicator(id, false, { height: 70 });
    if (newPaneId) ids.set(id, newPaneId);
  }
}

function periodToKChartPeriod(
  period: KlinePeriod,
): { type: "minute" | "hour" | "day" | "week" | "month"; span: number } {
  switch (period) {
    case "1m":
      return { type: "minute", span: 1 };
    case "5m":
      return { type: "minute", span: 5 };
    case "15m":
      return { type: "minute", span: 15 };
    case "60m":
      return { type: "hour", span: 1 };
    case "day":
      return { type: "day", span: 1 };
    case "week":
      return { type: "week", span: 1 };
    case "month":
      return { type: "month", span: 1 };
    case "minute":
      // 分时不走 klinecharts；fallback 给个值避免 TS exhaustiveness 报错
      return { type: "minute", span: 1 };
  }
}

function barToKLineData(b: ChartBar): KLineData {
  return {
    timestamp: b.timestamp,
    open: b.open,
    high: b.high,
    low: b.low,
    close: b.close,
    volume: b.volume,
    turnover: b.amount,
  };
}

function klinePointToBar(p: KlinePoint): ChartBar {
  const match = p.date.match(/^(\d{4})-(\d{2})-(\d{2})/);
  const ts = match
    ? Date.UTC(Number(match[1]), Number(match[2]) - 1, Number(match[3]), 1, 30, 0)
    : Date.parse(p.date);
  return {
    timestamp: ts,
    open: p.open,
    high: p.high,
    low: p.low,
    close: p.close,
    volume: p.volume ?? 0,
    amount: p.amount ?? 0,
  };
}

function minuteKlineToBar(p: MinuteKlinePoint): ChartBar {
  return {
    timestamp: p.timestamp,
    open: p.open,
    high: p.high,
    low: p.low,
    close: p.close,
    volume: p.volume,
    amount: p.amount,
  };
}

function chartStyles() {
  // 中式红涨绿跌——主图 candle + 子图 indicator 共用一套映射，避免量柱颜色与
  // K 线方向相反（klinecharts v10 默认 indicator.ohlc/bars 是绿涨红跌的西方
  // 习惯，不 override 就会出现"红 K 配绿量柱"的错位）。
  const upAlpha = "rgba(185, 52, 45, 0.7)"; // #b9342d
  const downAlpha = "rgba(22, 130, 90, 0.7)"; // #16825a
  return {
    candle: {
      bar: {
        upColor: "#b9342d",
        downColor: "#16825a",
        upBorderColor: "#b9342d",
        downBorderColor: "#16825a",
        upWickColor: "#b9342d",
        downWickColor: "#16825a",
      },
    },
    indicator: {
      ohlc: { upColor: upAlpha, downColor: downAlpha, noChangeColor: "#9aa3aa" },
      bars: [
        {
          style: "fill" as const,
          borderStyle: "solid" as const,
          borderSize: 1,
          borderDashedValue: [2, 2],
          upColor: upAlpha,
          downColor: downAlpha,
          noChangeColor: "#9aa3aa",
        },
      ],
    },
    grid: {
      horizontal: { color: "#eadfd1", style: "dashed" as const, dashedValue: [2, 2] },
      vertical: { color: "#f0ece4" },
    },
    crosshair: {
      horizontal: { line: { color: "#9aa3aa" } },
      vertical: { line: { color: "#9aa3aa" } },
    },
    xAxis: { axisLine: { color: "#ded9cf" }, tickText: { color: "#65747b" } },
    yAxis: { axisLine: { color: "#ded9cf" }, tickText: { color: "#65747b" } },
  };
}

// ===== 分时（lightweight-charts 折线 + 均价 + 量） ===========================

function LightweightMinuteChart({ points }: { points: MinutePoint[] }) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const seriesRef = useRef<{
    price: SeriesHandle;
    average: SeriesHandle;
    volume: SeriesHandle;
  } | null>(null);
  const data = useMemo(() => points.filter((item) => Number.isFinite(item.price)), [points]);

  useEffect(() => {
    if (!containerRef.current) return;
    const chart = createBaseChart(containerRef.current);
    const priceSeries = chart.addSeries(LineSeries, {
      color: "#1976d2",
      lineWidth: 2,
      priceLineVisible: false,
      lastValueVisible: true,
    });
    const avgSeries = chart.addSeries(LineSeries, {
      color: "#d39d1f",
      lineWidth: 1,
      priceLineVisible: false,
      lastValueVisible: false,
    });
    const volumeSeries = chart.addSeries(
      HistogramSeries,
      {
        color: "#9aa3aa",
        priceFormat: { type: "volume" },
        priceLineVisible: false,
        lastValueVisible: false,
      },
      1,
    );
    chart.panes()[0]?.setStretchFactor(0.78);
    chart.panes()[1]?.setStretchFactor(0.22);
    const cleanup = bindResize(chart, containerRef.current);
    chartRef.current = chart;
    seriesRef.current = {
      price: priceSeries as SeriesHandle,
      average: avgSeries as SeriesHandle,
      volume: volumeSeries as SeriesHandle,
    };
    return () => {
      cleanup();
      chart.remove();
      chartRef.current = null;
      seriesRef.current = null;
    };
  }, []);

  useEffect(() => {
    const chart = chartRef.current;
    const series = seriesRef.current;
    if (!chart || !series || data.length === 0) return;
    series.price.setData(data.map((item) => ({ time: minuteTime(item.time), value: item.price })));
    series.average.setData(
      data
        .filter((item) => item.average)
        .map((item) => ({ time: minuteTime(item.time), value: item.average! })),
    );
    series.volume.setData(
      data.map((item, index) => ({
        time: minuteTime(item.time),
        value: item.volume ?? 0,
        color:
          index > 0 && item.price < data[index - 1].price
            ? "rgba(22, 130, 90, 0.72)"
            : "rgba(185, 52, 45, 0.72)",
      })),
    );
    chart.timeScale().setVisibleLogicalRange({ from: 0, to: Math.max(data.length - 1, 0) });
  }, [data]);

  return <div className="lightweight-chart minute-lightweight-chart" ref={containerRef} />;
}

function createBaseChart(container: HTMLElement) {
  return createChart(container, {
    autoSize: true,
    height: 330,
    layout: {
      background: { color: "#ffffff" },
      textColor: "#65747b",
      attributionLogo: false,
    },
    grid: {
      vertLines: { color: "#f0ece4" },
      horzLines: { color: "#eadfd1" },
    },
    crosshair: {
      mode: 0,
      vertLine: { color: "#9aa3aa", style: 3 },
      horzLine: { color: "#9aa3aa", style: 3 },
    },
    rightPriceScale: {
      borderColor: "#ded9cf",
      scaleMargins: { top: 0.12, bottom: 0.08 },
    },
    timeScale: {
      borderColor: "#ded9cf",
      timeVisible: true,
      secondsVisible: false,
      rightOffset: 0,
      barSpacing: 4,
    },
    localization: {
      locale: "zh-CN",
      timeFormatter: (time: Time) =>
        typeof time === "number" ? formatChartTime(time) : String(time),
    },
    handleScale: false,
    handleScroll: false,
  });
}

function bindResize(chart: IChartApi, container: HTMLElement) {
  const observer = new ResizeObserver((entries) => {
    const width = entries[0]?.contentRect.width;
    if (width) chart.resize(Math.floor(width), chart.paneSize().height + 40);
  });
  observer.observe(container);
  return () => observer.disconnect();
}

function minuteTime(value: string): Time {
  const match = value.match(/^(\d{4})-(\d{2})-(\d{2})\s+(\d{2}):(\d{2})/);
  if (!match) return Math.floor(Date.now() / 1000) as Time;
  const [, year, month, day, hour, minute] = match;
  return Math.floor(
    Date.UTC(Number(year), Number(month) - 1, Number(day), Number(hour), Number(minute)) / 1000,
  ) as Time;
}

function formatChartTime(time: number) {
  const date = new Date(time * 1000);
  const hour = String(date.getUTCHours()).padStart(2, "0");
  const minute = String(date.getUTCMinutes()).padStart(2, "0");
  return `${hour}:${minute}`;
}

// ===== meta 渲染 helpers =================================================

function formatMetaNumber(v: number | null | undefined): string {
  return typeof v === "number" && Number.isFinite(v) ? v.toFixed(2) : "--";
}

function formatMetaSigned(v: number | null | undefined): string {
  if (typeof v !== "number" || !Number.isFinite(v)) return "--";
  return v >= 0 ? `+${v.toFixed(2)}` : v.toFixed(2);
}

function formatMetaAmount(v: number | null | undefined): string {
  if (typeof v !== "number" || !Number.isFinite(v)) return "--";
  if (Math.abs(v) >= 1e8) return `${(v / 1e8).toFixed(2)}亿`;
  if (Math.abs(v) >= 1e4) return `${(v / 1e4).toFixed(2)}万`;
  return v.toFixed(0);
}

// CandlestickSeries 在 lightweight-charts 5 没用到——保留 import 以防 v6+ 切换
void CandlestickSeries;
