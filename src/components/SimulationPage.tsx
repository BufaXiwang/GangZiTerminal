import { Plus, RefreshCw, X } from "lucide-react";
import { invoke } from "@tauri-apps/api/core";
import { useEffect, useMemo, useState } from "react";
import { KlineChart } from "./KlineChart";
import { useAccountSnapshot } from "../hooks/useAccountSnapshot";
import { useMarketQuotesFor } from "../hooks/useMarketQuotesFor";
import { useWatchlist } from "../hooks/useWatchlist";
import { formatAmount, formatDate, formatNumber, formatSigned } from "../lib/format";
import type { DomainPosition, MarketQuote, RiskAlert, WatchlistEntry } from "../types";

type Props = {
  /** 风险预警（前端 lib/simulation 派生，advisory） */
  riskAlerts: RiskAlert[];
};

const watchlistPreviewLimit = 48;
const openPositionRenderLimit = 80;

/**
 * 模拟账户页面。
 *
 * 数据源：
 * - **账户总览 + 持仓**：`useAccountSnapshot`（ACCOUNT_SNAPSHOT in-memory cache，
 *   通过 `account-snapshot-updated` event 自动 refetch）
 * - **自选股**：`useWatchlist`（用户加 / 删；agent 也能加，agent 没 remove tool）
 *
 * 写操作：
 * - 用户：仅 watchlist add/remove + reset 账户
 * - Agent：通过 AccountService 调（屏蔽中）
 */
export function SimulationPage({ riskAlerts }: Props) {
  const { snapshot, refresh: refreshSnapshot } = useAccountSnapshot();
  const watchlistHook = useWatchlist();

  const [newCode, setNewCode] = useState("");
  const [selectedPositionId, setSelectedPositionId] = useState<string | null>(null);
  const [chartEntry, setChartEntry] = useState<WatchlistEntry | null>(null);

  const openPositions = snapshot?.openPositions ?? [];
  const closedPositions = snapshot?.closedPositions ?? [];
  const visibleWatchlist = watchlistHook.entries.slice(0, watchlistPreviewLimit);
  const quoteTsCodes = useMemo(() => {
    const codes = visibleWatchlist.map((entry) => entry.tsCode);
    if (chartEntry) codes.push(chartEntry.tsCode);
    // open positions 也加进来——卡片要显示当前价 / 市值 / 浮动盈亏
    for (const p of openPositions) {
      codes.push(inferStockTsCode(p.code));
    }
    return codes;
  }, [visibleWatchlist, chartEntry, openPositions]);
  const { quoteMap } = useMarketQuotesFor(quoteTsCodes);

  useEffect(() => {
    if (openPositions.length === 0) {
      setSelectedPositionId(null);
      return;
    }
    if (!selectedPositionId || !openPositions.some((p) => p.id === selectedPositionId)) {
      setSelectedPositionId(openPositions[0].id);
    }
  }, [openPositions, selectedPositionId]);

  useEffect(() => {
    if (!chartEntry) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") setChartEntry(null);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [chartEntry]);

  if (!snapshot) {
    return (
      <section className="page-shell">
        <p className="muted">账户快照加载中…</p>
      </section>
    );
  }

  const openCount = openPositions.length;
  const closedCount = closedPositions.length;
  const selectedPosition =
    openPositions.find((p) => p.id === selectedPositionId) ?? openPositions[0] ?? null;
  const hiddenWatchlistCount = Math.max(0, watchlistHook.entries.length - visibleWatchlist.length);
  const visibleOpenPositions = openPositions.slice(0, openPositionRenderLimit);
  const hiddenOpenCount = Math.max(0, openCount - visibleOpenPositions.length);
  const chartQuote = chartEntry ? quoteMap.get(chartEntry.tsCode) ?? null : null;

  function handleAdd() {
    const code = newCode.trim();
    if (!/^\d{6}$/.test(code)) return;
    void watchlistHook.add(code);
    setNewCode("");
  }

  function handleReset() {
    const totalCount = openCount + closedCount;
    if (totalCount === 0) return;
    const confirmed = window.confirm(
      `确定要重置模拟账户吗？\n\n` +
        `这会删除全部 ${totalCount} 条持仓记录` +
        (openCount > 0 ? `（含 ${openCount} 条 open）` : "") +
        `。\n学习记录和自选股保留。\n此操作不可撤销。`,
    );
    if (!confirmed) return;
    void invoke<number>("reset_simulation_account")
      .then(() => refreshSnapshot())
      .catch((err) => {
        console.warn("reset_simulation_account 失败:", err);
      });
  }

  return (
    <section className="page-shell simulation-page">
      <div className="section-head">
        <div>
          <h2>模拟账户</h2>
          <p className="muted">
            自选股 + 持仓 + 账户总览。所有交易由 Agent 自动执行，用户只读 + 管理自选股。
          </p>
        </div>
        <div className="button-row">
          <button className="ghost" onClick={() => void refreshSnapshot()}>
            <RefreshCw size={16} />
            刷新
          </button>
          <button
            className="ghost danger"
            disabled={openCount === 0 && closedCount === 0}
            onClick={handleReset}
          >
            重置账户
          </button>
        </div>
      </div>

      {/* ====== 账户总览 ====== */}
      <div className="simulation-summary">
        <article>
          <span>总资产</span>
          <strong>{formatAmount(snapshot.totalAssets)}</strong>
        </article>
        <article>
          <span>可用现金</span>
          <strong>{formatAmount(snapshot.cash)}</strong>
        </article>
        <article>
          <span>持仓市值</span>
          <strong>{formatAmount(snapshot.marketValue)}</strong>
        </article>
        <article>
          <span>总盈亏</span>
          <strong className={snapshot.totalPnl < 0 ? "down" : "up"}>
            {formatAmount(snapshot.totalPnl)}
          </strong>
        </article>
        <article>
          <span>浮动盈亏</span>
          <strong className={snapshot.unrealizedPnl < 0 ? "down" : "up"}>
            {formatAmount(snapshot.unrealizedPnl)}
          </strong>
        </article>
        <article>
          <span>已实现盈亏</span>
          <strong className={snapshot.realizedPnl < 0 ? "down" : "up"}>
            {formatAmount(snapshot.realizedPnl)}
          </strong>
        </article>
        <article>
          <span>持仓 / 已平仓</span>
          <strong>{openCount}</strong>
          <small>{closedCount} 笔已平仓</small>
        </article>
      </div>

      <div className="simulation-layout">
        {/* ====== 自选股 CRUD ====== */}
        <aside className="watchlist-block watchlist-panel">
          <div className="section-head compact">
            <div>
              <h3>自选股</h3>
              <p className="muted">
                显示前 {visibleWatchlist.length} 只，剩余 {hiddenWatchlistCount} 只继续参与行情订阅。
              </p>
            </div>
            <span>{watchlistHook.entries.length} 只</span>
          </div>
          <div className="watchlist-add-row">
            <input
              type="text"
              placeholder="输入 6 位代码"
              value={newCode}
              onChange={(e) => setNewCode(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") handleAdd();
              }}
              maxLength={6}
              disabled={watchlistHook.busy}
            />
            <button
              type="button"
              className="primary"
              onClick={handleAdd}
              disabled={watchlistHook.busy || !/^\d{6}$/.test(newCode.trim())}
            >
              <Plus size={14} /> 添加
            </button>
          </div>
          {watchlistHook.error && (
            <p className="muted" style={{ color: "#b14444" }}>
              {watchlistHook.error}
            </p>
          )}
          <div className="watchlist-grid">
            {watchlistHook.entries.length === 0 ? (
              <p className="muted">还没有自选股。</p>
            ) : (
              visibleWatchlist.map((entry) => (
                <WatchlistQuoteRow
                  entry={entry}
                  key={entry.tsCode}
                  quote={quoteMap.get(entry.tsCode) ?? null}
                  onOpen={() => setChartEntry(entry)}
                  onRemove={() => void watchlistHook.remove(entry.code)}
                  busy={watchlistHook.busy}
                />
              ))
            )}
          </div>
        </aside>

        <div className="account-workspace">
          {/* ====== 风险预警（前端 advisory） ====== */}
          {riskAlerts.length > 0 && (
            <section className="risk-alert-list">
              <div className="section-head compact">
                <h3>风控检查</h3>
                <span>{riskAlerts.length} 条</span>
              </div>
              {riskAlerts.map((alert) => (
                <article className={alert.severity} key={alert.id}>
                  <strong>{alert.title}</strong>
                  <p>{alert.detail}</p>
                  {alert.action && <small>{alert.action}</small>}
                </article>
              ))}
            </section>
          )}

          {/* ====== 当前持仓 ====== */}
          <section className="sim-position-list">
            <div className="section-head compact">
              <h3>当前持仓</h3>
              <span>{openCount} 笔</span>
            </div>
            {openCount === 0 ? (
              <div className="empty-state account-empty-state">
                <strong>暂无当前持仓</strong>
                <p>Agent 满足开仓条件时会自动建仓；最近平仓记录保留在下方。</p>
              </div>
            ) : (
              <>
                {selectedPosition && (
                  <section className="selected-position-kline">
                    <KlineChart
                      code={selectedPosition.code}
                      tsCode={inferStockTsCode(selectedPosition.code)}
                      name={selectedPosition.name}
                      category="stock"
                    />
                  </section>
                )}
                {visibleOpenPositions.map((p) => (
                  <OpenPositionCard
                    key={p.id}
                    position={p}
                    quote={quoteMap.get(inferStockTsCode(p.code)) ?? null}
                    selected={p.id === selectedPosition?.id}
                    onSelect={() => setSelectedPositionId(p.id)}
                  />
                ))}
                {hiddenOpenCount > 0 && (
                  <p className="watchlist-more">
                    已显示前 {visibleOpenPositions.length} 笔，另有 {hiddenOpenCount} 笔持仓未展开。
                  </p>
                )}
              </>
            )}
          </section>

          {/* ====== 已平仓记录 ====== */}
          <section className="closed-position-list">
            <div className="section-head compact">
              <h3>最近平仓</h3>
              <span>{closedCount} 笔</span>
            </div>
            {closedCount === 0 ? (
              <p className="muted">暂无平仓记录。</p>
            ) : (
              closedPositions.slice(0, 24).map((p) => (
                <ClosedPositionCard key={p.id} position={p} />
              ))
            )}
          </section>
        </div>

      </div>
      {chartEntry && (
        <div className="watchlist-chart-backdrop" onClick={() => setChartEntry(null)}>
          <section className="watchlist-chart-modal" onClick={(event) => event.stopPropagation()}>
            <div className="watchlist-chart-head">
              <div>
                <h3>{chartEntry.name || chartEntry.code}</h3>
                <span>{chartEntry.tsCode}</span>
              </div>
              <button
                className="watchlist-remove"
                type="button"
                title="关闭"
                onClick={() => setChartEntry(null)}
              >
                <X size={16} />
              </button>
            </div>
            <KlineChart
              code={chartEntry.code}
              tsCode={chartEntry.tsCode}
              name={chartEntry.name || chartEntry.code}
              category="stock"
              meta={
                chartQuote
                  ? {
                      price: chartQuote.price,
                      changePercent: chartQuote.changePercent,
                      amount: chartQuote.amount,
                      low: chartQuote.low,
                      high: chartQuote.high,
                    }
                  : undefined
              }
            />
          </section>
        </div>
      )}
    </section>
  );
}

function WatchlistQuoteRow({
  entry,
  quote,
  onOpen,
  onRemove,
  busy,
}: {
  entry: WatchlistEntry;
  quote: MarketQuote | null;
  onOpen: () => void;
  onRemove: () => void;
  busy: boolean;
}) {
  return (
    <article className="watchlist-item quote" onClick={onOpen}>
      <div>
        <strong>{entry.name || entry.code}</strong>
        <small>{entry.code}</small>
      </div>
      <div className="watchlist-quote-price">
        <span>{formatQuotePrice(quote?.price ?? null)}</span>
        <em className={quoteTone(quote?.changePercent ?? null)}>
          {formatQuotePercent(quote?.changePercent ?? null)}
        </em>
      </div>
      <button
        type="button"
        className="watchlist-remove"
        title="移出自选"
        onClick={(event) => {
          event.stopPropagation();
          onRemove();
        }}
        disabled={busy}
      >
        <X size={14} />
      </button>
    </article>
  );
}

function OpenPositionCard({
  position,
  quote,
  selected,
  onSelect,
}: {
  position: DomainPosition;
  quote: MarketQuote | null;
  selected: boolean;
  onSelect: () => void;
}) {
  const cost = position.avgEntryPrice * position.currentShares;
  // 当前价拿不到时（盘外接口异常 / 新股停牌）回退到均价——marketValue 显示成本
  const currentPrice =
    quote?.price != null && Number.isFinite(quote.price) ? quote.price : null;
  const marketValue =
    currentPrice != null ? currentPrice * position.currentShares : cost;
  const pnl = marketValue - cost;
  const pnlPct = cost > 0 ? (pnl / cost) * 100 : 0;
  const pnlTone = pnl > 0.01 ? "up" : pnl < -0.01 ? "down" : "flat";
  return (
    <article
      className={selected ? "sim-position-card active" : "sim-position-card"}
      role="button"
      tabIndex={0}
      onClick={onSelect}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") onSelect();
      }}
    >
      <div className="sim-position-head">
        <div>
          <h3>{position.name}</h3>
          <span>{position.code}</span>
        </div>
        <div className={`sim-position-pnl tone-${pnlTone}`}>
          <strong>{formatSigned(pnl)}</strong>
          <em>
            {pnl > 0 ? "+" : ""}
            {pnlPct.toFixed(2)}%
          </em>
        </div>
      </div>
      <div className="sim-position-grid">
        <div>
          <span>当前价</span>
          <strong className={`tone-${pnlTone}`}>
            {currentPrice != null ? formatNumber(currentPrice) : "—"}
          </strong>
        </div>
        <div>
          <span>均价</span>
          <strong>{formatNumber(position.avgEntryPrice)}</strong>
        </div>
        <div>
          <span>持仓股数</span>
          <strong>{position.currentShares}</strong>
        </div>
        <div>
          <span>市值</span>
          <strong>{formatAmount(marketValue)}</strong>
        </div>
        <div>
          <span>持仓成本</span>
          <strong>{formatAmount(cost)}</strong>
        </div>
        <div>
          <span>止损</span>
          <strong>{position.stopLoss != null ? formatNumber(position.stopLoss) : "—"}</strong>
        </div>
        <div>
          <span>止盈</span>
          <strong>{position.takeProfit != null ? formatNumber(position.takeProfit) : "—"}</strong>
        </div>
      </div>
      {(quote?.bidLevels?.length || quote?.askLevels?.length) ? (
        <div className="sim-position-orderbook">
          {(() => {
            const bid1 = quote?.bidLevels?.[0];
            const ask1 = quote?.askLevels?.[0];
            const fmtLevel = (level: typeof bid1) =>
              level && level.price != null
                ? `${formatNumber(level.price)}${
                    level.volume != null ? ` × ${level.volume}` : ""
                  }`
                : "—";
            return (
              <>
                <span>
                  买一 <strong className="tone-up">{fmtLevel(bid1)}</strong>
                </span>
                <span>
                  卖一 <strong className="tone-down">{fmtLevel(ask1)}</strong>
                </span>
              </>
            );
          })()}
        </div>
      ) : null}
      {position.thesis && <p>{position.thesis}</p>}
      <small>建仓时间：{formatDate(position.enteredAt)}</small>
    </article>
  );
}

function inferStockTsCode(code: string): string {
  if (/^(60|68|90)/.test(code)) return `${code}.SH`;
  if (/^(83|87|88|92)/.test(code)) return `${code}.BJ`;
  return `${code}.SZ`;
}

function formatQuotePrice(value: number | null) {
  if (value == null || !Number.isFinite(value)) return "--";
  return value.toFixed(value >= 1000 ? 2 : 2);
}

function formatQuotePercent(value: number | null) {
  if (value == null || !Number.isFinite(value)) return "--";
  return `${value > 0 ? "+" : ""}${value.toFixed(2)}%`;
}

function quoteTone(value: number | null) {
  if (value == null || !Number.isFinite(value) || value === 0) return "flat";
  return value > 0 ? "up" : "down";
}

function ClosedPositionCard({ position }: { position: DomainPosition }) {
  if (position.status.state !== "closed") return null;
  const { exitPrice, exitAt, reason } = position.status;
  const pnl = (exitPrice - position.avgEntryPrice) * position.currentShares;
  const reasonLabel = ((): string => {
    switch (reason) {
      case "stop_loss":
        return "止损平仓";
      case "take_profit":
        return "止盈平仓";
      case "time_stop":
        return "时间止损";
      case "invalidated":
        return "假设证伪";
      default:
        return "手动平仓";
    }
  })();
  return (
    <article>
      <strong>
        {position.name} <span>{position.code}</span>
      </strong>
      <span>
        {formatNumber(position.avgEntryPrice)} → {formatNumber(exitPrice)}
      </span>
      <em className={pnl < 0 ? "down" : "up"}>{formatSigned(pnl)}</em>
      <small>
        {formatDate(exitAt)} · {reasonLabel}
      </small>
    </article>
  );
}
