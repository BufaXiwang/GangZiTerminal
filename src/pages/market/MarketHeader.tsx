// MarketHeader — 顶栏核心指数 + breadth 概览。
//
// Spec: docs/design/frontend-design.md §4 + quotes-module.md §2 (core_indexes)
//
// - 4 个核心指数：000001.SH（上证）/ 399001.SZ（深证）/ 399006.SZ（创业板）/ 000300.SH（沪深300）
// - breadth：上涨家数 / 下跌家数 / 总成交额（基于 listMarket(category=stock, limit=500)）
//
// 简化：listMarket 后端 limit cap = 500（spec §4 line 549）。breadth 在 first-phase 中
// 取 top 500（按 amount 排序）作为采样，标注 "based on top 500"。TODO：等后端补独立
// breadth endpoint 后切到全量统计。

import { useEffect, useState } from "react";
import {
  commands,
  type FetchDataItem,
  type ListMarketItem,
} from "../../bindings";

interface CoreIndexInfo {
  tsCode: string;
  label: string;
}

const CORE_INDEXES: CoreIndexInfo[] = [
  { tsCode: "000001.SH", label: "上证" },
  { tsCode: "399001.SZ", label: "深证" },
  { tsCode: "399006.SZ", label: "创业板" },
  { tsCode: "000300.SH", label: "沪深300" },
];

interface BreadthCounts {
  up: number;
  down: number;
  flat: number;
  total: number;
  totalAmount: number;
  sampled: boolean;
}

function fmtPrice(v: number | undefined | null): string {
  if (v == null || !Number.isFinite(v)) return "-";
  return v.toFixed(2);
}

function fmtPct(v: number | undefined | null): string {
  if (v == null || !Number.isFinite(v)) return "-";
  const sign = v > 0 ? "+" : "";
  return `${sign}${v.toFixed(2)}%`;
}

function fmtAmount(v: number): string {
  if (v >= 1e12) return `${(v / 1e12).toFixed(2)}万亿`;
  if (v >= 1e8) return `${(v / 1e8).toFixed(2)}亿`;
  if (v >= 1e4) return `${(v / 1e4).toFixed(2)}万`;
  return v.toFixed(0);
}

function pctClass(v: number | undefined | null): string {
  if (v == null) return "flat";
  if (v > 0) return "up";
  if (v < 0) return "down";
  return "flat";
}

function priceFromQuote(q: FetchDataItem["quote"]): number | undefined {
  if (!q?.price) return undefined;
  const n = Number(q.price);
  return Number.isFinite(n) ? n : undefined;
}

function pctFromQuote(q: FetchDataItem["quote"]): number | undefined {
  return q?.changePercent ?? undefined;
}

export function MarketHeader() {
  const [indexes, setIndexes] = useState<FetchDataItem[]>([]);
  const [breadth, setBreadth] = useState<BreadthCounts | null>(null);

  // 拉核心指数实时报价
  useEffect(() => {
    let cancelled = false;
    void commands
      .fetchData({
        tsCodes: CORE_INDEXES.map((c) => c.tsCode),
        include: { quote: true },
      })
      .then((res) => {
        if (cancelled) return;
        if (res.status === "error") return;
        setIndexes(res.data.items);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // 拉 breadth：top 500 个股票（按 amount 排序）
  useEffect(() => {
    let cancelled = false;
    void commands
      .listMarket({
        category: "stock",
        includeQuote: true,
        limit: 500,
        offset: 0,
      })
      .then((res) => {
        if (cancelled) return;
        if (res.status === "error") return;
        const counts = countBreadth(res.data.items);
        setBreadth(counts);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <div className="market-header">
      {CORE_INDEXES.map((info) => {
        const item = indexes.find((i) => i.tsCode === info.tsCode);
        const price = priceFromQuote(item?.quote);
        const pct = pctFromQuote(item?.quote);
        return (
          <div key={info.tsCode} className="market-header-index">
            <span className="idx-name">{info.label}</span>
            <span className="idx-price tabular">{fmtPrice(price)}</span>
            <span className={`idx-pct tabular ${pctClass(pct)}`}>
              {fmtPct(pct)}
            </span>
          </div>
        );
      })}
      {breadth && (
        <div className="market-header-breadth">
          <span className="breadth-cell up">↑{breadth.up}</span>
          <span className="breadth-cell down">↓{breadth.down}</span>
          <span className="breadth-cell flat">→{breadth.flat}</span>
          <span className="breadth-cell flat" title="总成交额">
            {fmtAmount(breadth.totalAmount)}
          </span>
          {breadth.sampled && (
            <span className="faint" title="基于成交额前 500 名采样">
              · top500
            </span>
          )}
        </div>
      )}
    </div>
  );
}

function countBreadth(items: ListMarketItem[]): BreadthCounts {
  let up = 0;
  let down = 0;
  let flat = 0;
  let totalAmount = 0;
  for (const it of items) {
    const pct = it.quote?.changePercent;
    if (pct == null) {
      flat += 1;
    } else if (pct > 0) {
      up += 1;
    } else if (pct < 0) {
      down += 1;
    } else {
      flat += 1;
    }
    if (it.quote?.amount != null && Number.isFinite(it.quote.amount)) {
      totalAmount += it.quote.amount;
    }
  }
  return {
    up,
    down,
    flat,
    total: items.length,
    totalAmount,
    // 后端 list_market.limit 上限 500（spec §4）；只要满 500 视作 sampled。
    sampled: items.length >= 500,
  };
}
