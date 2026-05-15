import type { RiskAlert, SimulatedPosition, StockQuote } from "../types";

export type SimulationAccountSnapshot = {
  openPositions: SimulatedPosition[];
  closedPositions: SimulatedPosition[];
  marketValue: number;
  invested: number;
  realizedPnl: number;
  floatingPnl: number;
  availableCash: number;
  totalAssets: number;
  totalPnl: number;
};

export function calculateSimulationAccount(
  initialCash: number,
  positions: SimulatedPosition[],
  quotes: StockQuote[],
): SimulationAccountSnapshot {
  const openPositions = positions.filter((position) => position.status === "open");
  const closedPositions = positions.filter((position) => position.status === "closed");
  const marketValue = openPositions.reduce((sum, position) => {
    const quote = quotes.find((item) => item.code === position.code);
    return sum + (quote?.price ?? position.entryPrice) * position.shares;
  }, 0);
  const invested = openPositions.reduce((sum, position) => sum + position.entryPrice * position.shares, 0);
  const realizedProceeds = closedPositions.reduce((sum, position) => sum + (position.exitPrice ?? position.entryPrice) * position.shares, 0);
  const realizedCost = closedPositions.reduce((sum, position) => sum + position.entryPrice * position.shares, 0);
  const realizedPnl = realizedProceeds - realizedCost;
  const floatingPnl = marketValue - invested;
  const availableCash = initialCash - invested - realizedCost + realizedProceeds;
  const totalAssets = availableCash + marketValue;
  const totalPnl = totalAssets - initialCash;

  return {
    openPositions,
    closedPositions,
    marketValue,
    invested,
    realizedPnl,
    floatingPnl,
    availableCash,
    totalAssets,
    totalPnl,
  };
}

export function evaluateSimulationRisk(
  initialCash: number,
  positions: SimulatedPosition[],
  quotes: StockQuote[],
): RiskAlert[] {
  const account = calculateSimulationAccount(initialCash, positions, quotes);
  const alerts: RiskAlert[] = [];
  const openValue = account.marketValue;
  if (initialCash > 0 && openValue / initialCash > 0.45) {
    alerts.push({
      id: "portfolio-exposure",
      severity: "warning",
      title: "模拟仓位偏高",
      detail: `当前持仓市值约占初始资金 ${Math.round((openValue / initialCash) * 100)}%。`,
      action: "优先复盘新增仓位是否都满足验证条件。",
    });
  }

  positions
    .filter((position) => position.status === "open")
    .forEach((position) => {
      const quote = quotes.find((item) => item.code === position.code);
      const latestPrice = quote?.price ?? position.entryPrice;
      const returnPct = position.entryPrice > 0 ? (latestPrice - position.entryPrice) / position.entryPrice : 0;
      const ageDays = Math.floor((Date.now() - Date.parse(position.entryAt)) / (24 * 60 * 60 * 1000));
      if (returnPct <= -0.08) {
        alerts.push({
          id: `loss-${position.id}`,
          severity: "danger",
          code: position.code,
          title: `${position.name} 触及亏损复盘线`,
          detail: `当前模拟收益约 ${(returnPct * 100).toFixed(1)}%，需要检查原假设是否失效。`,
          action: "查看来源分析和止损条件，必要时触发复盘或模拟退出。",
        });
      }
      if (ageDays >= 5) {
        alerts.push({
          id: `time-${position.id}`,
          severity: "warning",
          code: position.code,
          title: `${position.name} 到达时间止损检查点`,
          detail: `已持有 ${ageDays} 天，若事件逻辑未兑现，应复盘时间止损。`,
          action: "检查验证清单、板块强弱和后续公告。",
        });
      }
      const positionValue = latestPrice * position.shares;
      if (initialCash > 0 && positionValue / initialCash > 0.12) {
        alerts.push({
          id: `single-${position.id}`,
          severity: "info",
          code: position.code,
          title: `${position.name} 单一标的暴露偏高`,
          detail: `单一标的约占初始资金 ${Math.round((positionValue / initialCash) * 100)}%。`,
          action: "避免同一主题重复加仓，保持模拟样本可比较。",
        });
      }
    });

  return alerts.slice(0, 12);
}
