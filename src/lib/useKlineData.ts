// useKlineData — 拉取单只标的的 K 线数据 hook。
//
// Spec: docs/design/quotes-module.md §4 fetch_data + frontend-design.md §5 K 线图
//
// 不抽整 chart wrapper（spec 明确禁止）；只抽数据拉取 + 周期切换 + loading/error 三态，
// 供市场页 / 模拟账户页持仓详情等多处复用。

import { useEffect, useMemo, useRef, useState } from "react";
import {
  commands,
  type FetchInclude,
  type KlinePeriod,
  type MinuteKlinePeriod,
  type TsCode,
} from "../bindings";

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
  if (!raw || typeof raw !== "object") return [];
  const root = raw as Record<string, unknown>;
  // 后端返回 { items: FetchDataItem[] }；找第一个匹配 tsCode 的 item。
  const itemsArr = (root.items ?? []) as unknown;
  if (!Array.isArray(itemsArr)) return [];
  const target = itemsArr.find(
    (it) =>
      it &&
      typeof it === "object" &&
      (it as Record<string, unknown>).tsCode === tsCode,
  ) as Record<string, unknown> | undefined;
  if (!target) return [];
  const isMinute = isMinutePeriod(period);
  const klineGroup = (
    isMinute ? target.minuteKlines ?? target.minute_klines : target.klines
  ) as Record<string, unknown> | undefined;
  if (!klineGroup) return [];
  const series = klineGroup[period];
  if (!series || typeof series !== "object") return [];
  const points = (series as Record<string, unknown>).points;
  if (!Array.isArray(points)) return [];
  return points
    .map((row) => normalizeKlineRow(row))
    .filter((r): r is KlineDataPoint => r !== null);
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
  const [state, setState] = useState<UseKlineDataState>({
    data: [],
    loading: false,
    error: null,
    lastUpdatedMs: null,
  });
  // 用于忽略竞态：每次请求带 token，只有 token 与最新一致才更新 state
  const reqIdRef = useRef(0);

  useEffect(() => {
    if (!enabled || !tsCode) {
      setState({ data: [], loading: false, error: null, lastUpdatedMs: null });
      return;
    }
    const id = ++reqIdRef.current;
    setState((s) => ({ ...s, loading: true, error: null }));
    const isIntraday = period === "intraday";
    let include: FetchInclude;
    if (isIntraday) {
      include = { intraday: true };
    } else if (isMinutePeriod(period)) {
      include = { minuteKlines: [period] };
    } else {
      include = { klines: [period as KlinePeriod] };
    }
    void commands
      .fetchData({ tsCodes: [tsCode], include })
      .then((res) => {
        if (id !== reqIdRef.current) return; // stale request
        // DEBUG
        // eslint-disable-next-line no-console
        console.log("[useKlineData] fetchData result:", { tsCode, period, status: res.status, data: res.status === "ok" ? res.data : null });
        if (res.status === "error") {
          setState({
            data: [],
            loading: false,
            error: `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
            lastUpdatedMs: null,
          });
          return;
        }
        const points = isIntraday
          ? extractIntraday(res.data, tsCode)
          : extractKlines(res.data, tsCode, period as AnyKlinePeriod);
        // eslint-disable-next-line no-console
        console.log("[useKlineData] parsed points:", points.length, points.slice(0, 2));
        setState({
          data: points,
          loading: false,
          error: null,
          lastUpdatedMs: Date.now(),
        });
      });
  }, [tsCode, period, enabled]);

  return useMemo(() => state, [state]);
}
