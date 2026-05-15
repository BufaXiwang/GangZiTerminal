import { useEffect, useMemo, useState } from "react";
import { formatDate, formatNumber, formatSigned } from "../lib/format";
import { getMarketSession, sectorRows } from "../lib/market";
import type { MarketOverview as MarketOverviewData } from "../types";

type Props = {
  autoRefresh: boolean;
  lastUpdated: string | null;
  marketOverview: MarketOverviewData;
  refreshInterval: number;
  status: string;
};

export function MarketOverview({
  autoRefresh,
  lastUpdated,
  marketOverview,
  refreshInterval,
  status,
}: Props) {
  const sectors = marketOverview.sectors.length
    ? marketOverview.sectors.slice(0, 5).map((sector) => ({
        name: sector.name,
        value: `${formatSigned(sector.changePercent)}%`,
      }))
    : sectorRows.map(([name, value]) => ({ name, value }));

  // 市场会话状态——盘外/休市时数据不是"现在"的，必须明确告诉用户。
  // 每分钟自走一次，不依赖外部刷新——用户即使不点按钮，标签也会随时间推进。
  const [now, setNow] = useState(() => new Date());
  useEffect(() => {
    const timer = window.setInterval(() => setNow(new Date()), 60_000);
    return () => window.clearInterval(timer);
  }, []);
  const session = useMemo(() => getMarketSession(now), [now]);
  const isMarketLive = session.isLive;

  return (
    <section className="market-strip">
      <div className="market-head">
        <div>
          <h1>今日市场</h1>
          <p>
            <span className={`market-session market-session-${session.status}`}>
              {isMarketLive ? "● " : "○ "}
              {session.label}
            </span>
            {" · "}
            {autoRefresh ? `自动刷新 ${Math.round(refreshInterval / 1000)}s` : "手动刷新"} ·{" "}
            {lastUpdated ? formatDate(lastUpdated) : "等待更新"}
            {status ? ` · ${status}` : ""}
          </p>
          {!isMarketLive && <p className="market-session-note">{session.note}</p>}
        </div>
      </div>

      <div className="market-overview">
        <div className="index-grid">
          {marketOverview.indices.map((quote) => (
            <div className="market-tile" key={quote.code}>
              <span>{quote.name}</span>
              <strong>{formatNumber(quote.price)}</strong>
              <em className={quote.changePercent && quote.changePercent < 0 ? "down" : "up"}>
                {formatSigned(quote.changePercent)}%
              </em>
            </div>
          ))}
        </div>
        <div className="market-side">
          <div className="sector-board">
            <span>行业热度</span>
            {sectors.map((sector) => (
              <div key={sector.name}>
                <b>{sector.name}</b>
                <em className={sector.value.startsWith("-") || sector.value === "--%" ? "down" : "up"}>{sector.value}</em>
              </div>
            ))}
          </div>
        </div>
      </div>
    </section>
  );
}
