// useKlineData — 拉取单只标的的 K 线数据 hook。
//
// Spec: docs/design/quotes-module.md §4 fetch_data + frontend-design.md §5 K 线图
//
// 不抽整 chart wrapper（spec 明确禁止）；只抽数据拉取 + 周期切换 + loading/error 三态，
// 供市场页 / 模拟账户页持仓详情等多处复用。

import { useEffect, useMemo, useRef, useState } from "react";
import { commands, type KlinePeriod, type TsCode } from "../bindings";

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

export interface UseKlineDataOptions {
  tsCode: TsCode | null;
  /** 默认 'day' */
  period?: KlinePeriod;
  /** 是否启用（false 时不发请求，常用于条件渲染） */
  enabled?: boolean;
}

const MINUTE_PERIODS: KlinePeriod[] = ["1m", "5m", "15m", "30m", "60m"];

/**
 * 从后端 fetch_data 返回里抽 K 线数组。
 *
 * 后端 DTO 形状以 quotes-module.md §4 为准；这里做防御式解析，
 * 字段缺失 / 类型不符直接 fallback 为空数组（不抛错），让 UI 显示空态。
 */
function extractKlines(
  raw: unknown,
  tsCode: TsCode,
  period: KlinePeriod,
): KlineDataPoint[] {
  if (!raw || typeof raw !== "object") return [];
  const root = raw as Record<string, unknown>;
  const items = (root.items ?? root.data ?? {}) as Record<string, unknown>;
  const entry = items[tsCode];
  if (!entry || typeof entry !== "object") return [];
  const isMinute = MINUTE_PERIODS.includes(period);
  const bag = entry as Record<string, unknown>;
  const klineGroup = (
    isMinute ? bag.minute_klines ?? bag.minuteKlines : bag.klines
  ) as Record<string, unknown> | undefined;
  if (!klineGroup) return [];
  const series = klineGroup[period];
  if (!Array.isArray(series)) return [];
  return series
    .map((row) => normalizeKlineRow(row))
    .filter((r): r is KlineDataPoint => r !== null);
}

function normalizeKlineRow(row: unknown): KlineDataPoint | null {
  if (!row || typeof row !== "object") return null;
  const r = row as Record<string, unknown>;
  // 兼容多种字段命名：trade_date / date / time / timestamp / ts
  const timeRaw =
    r.time ?? r.timestamp ?? r.ts ?? r.trade_date ?? r.date ?? r.dt;
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
    const isMinute = MINUTE_PERIODS.includes(period);
    const include = isMinute
      ? { minute_klines: [period] }
      : { klines: [period] };
    void commands
      .fetchData({ ts_codes: [tsCode], include })
      .then((res) => {
        if (id !== reqIdRef.current) return; // stale request
        if (res.status === "error") {
          setState({
            data: [],
            loading: false,
            error: `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
            lastUpdatedMs: null,
          });
          return;
        }
        const points = extractKlines(res.data, tsCode, period);
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
