// useKlineData — 拉取单只标的的 K 线数据 hook。
//
// Spec: docs/design/quotes-module.md §4 fetch_data + frontend-design.md §5 K 线图
//
// 不抽整 chart wrapper（spec 明确禁止）；只抽数据拉取 + 周期切换 + loading/error 三态，
// 供市场页 / 模拟账户页持仓详情等多处复用。

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  commands,
  type FetchInclude,
  type KlinePeriod,
  type MinuteKlinePeriod,
  type TsCode,
} from "../bindings";

const INITIAL_LIMIT = 500;
const LOAD_MORE_STEP = 300;
const MAX_LIMIT = 2000;

// 后端 bindings 没单独导出联合，本地合成。
type AnyKlinePeriod = KlinePeriod | MinuteKlinePeriod;

export interface KlineDataPoint {
  /** UNIX timestamp（秒）。日 K / 分钟 K 都用秒级。 */
  time: number;
  open: number;
  high: number;
  low: number;
  close: number;
  volume?: number;
}

export interface UseKlineDataState {
  data: KlineDataPoint[];
  loading: boolean;
  error: string | null;
  /** 最近一次数据刷新的本地时间戳（ms） */
  lastUpdatedMs: number | null;
  /** 当前累计请求的 limit；UI 可用来显示"已加载 N 根" */
  limit: number;
  /** 用户拖到左边时调用，追加更多历史数据 */
  requestMore: () => void;
  /** 后端已确认无更多历史（limit 已到上限或上次返回 < limit）→ 不再 requestMore */
  noMoreHistory: boolean;
}

/** 拓展周期：标准 K 线周期 + "intraday" 分时。 */
export type ChartPeriod = AnyKlinePeriod | "intraday";

export interface UseKlineDataOptions {
  tsCode: TsCode | null;
  /** 默认 'day'。支持 intraday + day/week/month + 1m/5m/15m/30m/60m。 */
  period?: ChartPeriod;
  /** 是否启用（false 时不发请求，常用于条件渲染） */
  enabled?: boolean;
}

const MINUTE_PERIODS: readonly MinuteKlinePeriod[] = [
  "1m",
  "5m",
  "15m",
  "30m",
  "60m",
] as const;

function isMinutePeriod(p: AnyKlinePeriod): p is MinuteKlinePeriod {
  return (MINUTE_PERIODS as readonly string[]).includes(p);
}

/**
 * 从后端 fetch_data 返回里抽 K 线数组。
 *
 * 后端 DTO 形状以 quotes-module.md §4 为准；这里做防御式解析，
 * 字段缺失 / 类型不符直接 fallback 为空数组（不抛错），让 UI 显示空态。
 */
function extractIntraday(raw: unknown, tsCode: TsCode): KlineDataPoint[] {
  if (!raw || typeof raw !== "object") return [];
  const root = raw as Record<string, unknown>;
  const itemsArr = (root.items ?? []) as unknown;
  if (!Array.isArray(itemsArr)) return [];
  const target = itemsArr.find(
    (it) =>
      it &&
      typeof it === "object" &&
      (it as Record<string, unknown>).tsCode === tsCode,
  ) as Record<string, unknown> | undefined;
  if (!target) return [];
  const intraday = target.intraday as Record<string, unknown> | undefined;
  if (!intraday) return [];
  const points = intraday.points;
  if (!Array.isArray(points)) return [];
  const tradeDate =
    typeof intraday.tradeDate === "string" ? intraday.tradeDate : undefined;
  return points
    .map((row) => normalizeMinutePoint(row, tradeDate))
    .filter((r): r is KlineDataPoint => r !== null);
}

function normalizeMinutePoint(
  row: unknown,
  tradeDate: string | undefined,
): KlineDataPoint | null {
  if (!row || typeof row !== "object") return null;
  const r = row as Record<string, unknown>;
  // MinutePoint = { tradeDate, time: "HHMM" or "HH:MM", price, ... }
  const timeStr = typeof r.time === "string" ? r.time : null;
  const price = toNumber(r.price);
  if (price == null) return null;
  let unix: number | null = null;
  if (timeStr && tradeDate && /^\d{8}$/.test(tradeDate)) {
    // 时间格式可能是 "HHMM" / "HH:MM" / "HHMMSS"
    const digits = timeStr.replace(/\D/g, "");
    const hh = Number(digits.slice(0, 2));
    const mm = Number(digits.slice(2, 4));
    const y = Number(tradeDate.slice(0, 4));
    const mo = Number(tradeDate.slice(4, 6)) - 1;
    const d = Number(tradeDate.slice(6, 8));
    if (
      Number.isFinite(hh) &&
      Number.isFinite(mm) &&
      Number.isFinite(y) &&
      Number.isFinite(mo) &&
      Number.isFinite(d)
    ) {
      unix = Math.floor(Date.UTC(y, mo, d, hh, mm) / 1000);
    }
  }
  if (unix == null) {
    unix = toUnixSeconds(timeStr ?? r.tradeDate);
  }
  if (unix == null) return null;
  return {
    time: unix,
    open: price,
    high: price,
    low: price,
    close: price,
    volume: toNumber(r.volume) ?? undefined,
  };
}

function extractKlines(
  raw: unknown,
  tsCode: TsCode,
  period: AnyKlinePeriod,
): KlineDataPoint[] {
  if (!raw || typeof raw !== "object") {
    console.warn("[extractKlines] raw not object", raw);
    return [];
  }
  const root = raw as Record<string, unknown>;
  const itemsArr = (root.items ?? []) as unknown;
  if (!Array.isArray(itemsArr)) {
    console.warn("[extractKlines] items not array", root);
    return [];
  }
  const target = itemsArr.find(
    (it) =>
      it &&
      typeof it === "object" &&
      (it as Record<string, unknown>).tsCode === tsCode,
  ) as Record<string, unknown> | undefined;
  if (!target) {
    console.warn("[extractKlines] no target for tsCode", tsCode, itemsArr);
    return [];
  }
  const isMinute = isMinutePeriod(period);
  const klineGroup = (
    isMinute ? target.minuteKlines ?? target.minute_klines : target.klines
  ) as Record<string, unknown> | undefined;
  if (!klineGroup) {
    console.warn("[extractKlines] no klineGroup. target keys:", Object.keys(target), "target:", target);
    return [];
  }
  const series = klineGroup[period];
  if (!series || typeof series !== "object") {
    console.warn("[extractKlines] no series for period", period, "klineGroup keys:", Object.keys(klineGroup), "klineGroup:", klineGroup);
    return [];
  }
  const points = (series as Record<string, unknown>).points;
  if (!Array.isArray(points)) {
    console.warn("[extractKlines] points not array", series);
    return [];
  }
  if (points.length > 0) {
    console.log("[extractKlines] first raw point:", points[0]);
  }
  const out = points
    .map((row) => normalizeKlineRow(row))
    .filter((r): r is KlineDataPoint => r !== null);
  console.log("[extractKlines]", points.length, "raw →", out.length, "normalized");
  return out;
}

function normalizeKlineRow(row: unknown): KlineDataPoint | null {
  if (!row || typeof row !== "object") return null;
  const r = row as Record<string, unknown>;
  // 后端 KlinePoint.date = TradeDate (YYYYMMDD)
  //      MinuteKlinePoint.timestampMs = ms
  const timeRaw =
    r.timestampMs ?? r.timestamp_ms ?? r.date ?? r.tradeDate ?? r.trade_date ?? r.time;
  const time = toUnixSeconds(timeRaw);
  if (time == null) return null;
  const open = toNumber(r.open ?? r.o);
  const high = toNumber(r.high ?? r.h);
  const low = toNumber(r.low ?? r.l);
  const close = toNumber(r.close ?? r.c);
  const volume = toNumber(r.volume ?? r.vol ?? r.v);
  if (open == null || high == null || low == null || close == null) return null;
  return {
    time,
    open,
    high,
    low,
    close,
    volume: volume ?? undefined,
  };
}

function toNumber(v: unknown): number | null {
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string" && v.trim() !== "") {
    const n = Number(v);
    if (Number.isFinite(n)) return n;
  }
  return null;
}

/** 把后端时间字段统一为 UNIX 秒。支持：
 *  - 数字（秒级 / 毫秒级自动判断）
 *  - "YYYYMMDD" 日期串
 *  - "YYYY-MM-DD" / ISO 时间串
 */
function toUnixSeconds(v: unknown): number | null {
  if (typeof v === "number" && Number.isFinite(v)) {
    return v > 1e12 ? Math.floor(v / 1000) : Math.floor(v);
  }
  if (typeof v !== "string" || v.trim() === "") return null;
  // YYYYMMDD
  if (/^\d{8}$/.test(v)) {
    const y = Number(v.slice(0, 4));
    const m = Number(v.slice(4, 6)) - 1;
    const d = Number(v.slice(6, 8));
    const t = Date.UTC(y, m, d);
    return Math.floor(t / 1000);
  }
  // YYYYMMDDHHMM
  if (/^\d{12}$/.test(v)) {
    const y = Number(v.slice(0, 4));
    const m = Number(v.slice(4, 6)) - 1;
    const d = Number(v.slice(6, 8));
    const hh = Number(v.slice(8, 10));
    const mm = Number(v.slice(10, 12));
    const t = Date.UTC(y, m, d, hh, mm);
    return Math.floor(t / 1000);
  }
  const parsed = Date.parse(v);
  if (Number.isFinite(parsed)) return Math.floor(parsed / 1000);
  return null;
}

export function useKlineData(opts: UseKlineDataOptions): UseKlineDataState {
  const { tsCode, period = "day", enabled = true } = opts;
  const [limit, setLimit] = useState(INITIAL_LIMIT);
  const [noMoreHistory, setNoMoreHistory] = useState(false);
  const [innerState, setInnerState] = useState<
    Omit<UseKlineDataState, "limit" | "requestMore" | "noMoreHistory">
  >({
    data: [],
    loading: false,
    error: null,
    lastUpdatedMs: null,
  });
  const reqIdRef = useRef(0);

  // 换标的 / 周期时重置 limit
  useEffect(() => {
    setLimit(INITIAL_LIMIT);
    setNoMoreHistory(false);
  }, [tsCode, period]);

  const requestMore = useCallback(() => {
    setLimit((l) => Math.min(l + LOAD_MORE_STEP, MAX_LIMIT));
  }, []);

  useEffect(() => {
    if (!enabled || !tsCode) {
      setInnerState({ data: [], loading: false, error: null, lastUpdatedMs: null });
      return;
    }
    const id = ++reqIdRef.current;
    setInnerState((s) => ({ ...s, loading: true, error: null }));
    const isIntraday = period === "intraday";
    let include: FetchInclude;
    if (isIntraday) {
      include = { intraday: true };
    } else if (isMinutePeriod(period)) {
      include = { minuteKlines: [period] };
    } else {
      include = { klines: [period as KlinePeriod] };
    }

    let cancelled = false;

    const doFetch = async (): Promise<{ ok: boolean; points: KlineDataPoint[] }> => {
      const res = await commands.fetchData({
        tsCodes: [tsCode],
        include,
        // limit.kline / limit.minute_kline 在后端 FetchLimits 内复用同一字段名映射
        limit: { kline: limit, minuteKline: limit },
      });
      if (cancelled || id !== reqIdRef.current) return { ok: false, points: [] };
      if (res.status === "error") {
        setInnerState({
          data: [],
          loading: false,
          error: `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
          lastUpdatedMs: null,
        });
        return { ok: false, points: [] };
      }
      const points = isIntraday
        ? extractIntraday(res.data, tsCode)
        : extractKlines(res.data, tsCode, period as AnyKlinePeriod);
      return { ok: true, points };
    };

    const run = async (): Promise<void> => {
      try {
        // 1. 先读 DB
        let result = await doFetch();
        if (!result.ok) return;
        if (result.points.length > 0) {
          // 若拿到的点数 < limit，说明 DB 里没有更多历史，标记 noMoreHistory
          if (result.points.length < limit || limit >= MAX_LIMIT) {
            setNoMoreHistory(true);
          }
          setInnerState({
            data: result.points,
            loading: false,
            error: null,
            lastUpdatedMs: Date.now(),
          });
          return;
        }
        // 2. DB 空 → 触发后端 refresh，再读
        const refreshRes = await commands.ensureChartData(tsCode, period);
        if (cancelled || id !== reqIdRef.current) return;
        if (refreshRes.status === "error") {
          setInnerState({
            data: [],
            loading: false,
            error: `${refreshRes.error.code}${refreshRes.error.message ? `: ${refreshRes.error.message}` : ""}`,
            lastUpdatedMs: null,
          });
          return;
        }
        // 3. refresh 完成后重新读 DB
        result = await doFetch();
        if (!result.ok) return;
        if (result.points.length > 0 && result.points.length < limit) {
          setNoMoreHistory(true);
        }
        setInnerState({
          data: result.points,
          loading: false,
          error: result.points.length === 0 ? "无可用数据" : null,
          lastUpdatedMs: Date.now(),
        });
      } catch (e) {
        if (cancelled || id !== reqIdRef.current) return;
        setInnerState({
          data: [],
          loading: false,
          error: String(e),
          lastUpdatedMs: null,
        });
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [tsCode, period, enabled, limit]);

  return useMemo(
    () => ({
      ...innerState,
      limit,
      requestMore: noMoreHistory ? () => {} : requestMore,
      noMoreHistory,
    }),
    [innerState, limit, noMoreHistory, requestMore],
  );
}
