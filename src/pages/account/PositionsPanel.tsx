// PositionsPanel — 持仓表 + 选中持仓日 K 线。
//
// Spec:
//   docs/design/account-module.md §2 仓位模型（Position + PositionProtection）
//   docs/design/frontend-design.md §4 模拟账户页 / §5 列表与表格
//
// 列：代码 | 名称 | 数量 | 可卖 | 成本 | 现价 | 市值 | 浮盈% | 保护 | actor
// - 数字右对齐 + tabular mono；空值 `-`
// - 浮动盈亏百分比红涨绿跌（用 unrealizedPnl / 成本计算；缺数据降级为 marketPrice - avgCost 比例）
// - 可卖 < 持仓数 时加 T+1 锁仓 tooltip
// - 选中后下方 KlineCanvas 显示该 ts_code 的日 K（complex 不附加指标 — 留给市场页）

import { Clock, Shield, ShieldOff, Target, TrendingDown } from "lucide-react";
import { useEffect } from "react";
import { KlineCanvas } from "../../components/KlineCanvas";
import type {
  Position,
  PositionProtection,
  TsCode,
  WarningCode,
} from "../../bindings";
import {
  changeClass,
  fmtMoneyShort,
  fmtNum,
  fmtPct,
  fmtShares,
  fmtSignedMoney,
  pnlClass,
  toNumber,
} from "./format";

interface PositionsPanelProps {
  positions: Position[];
  loading: boolean;
  selected: TsCode | null;
  onSelect: (tsCode: TsCode) => void;
  selectedItem: Position | null;
}

interface ProtectionChip {
  kind: "stop" | "take" | "time" | "signal";
  label: string;
  Icon: typeof Shield;
  title: string;
}

function buildProtectionChips(p: PositionProtection | undefined): ProtectionChip[] {
  if (!p) return [];
  if (!p.enabled) {
    return [
      {
        kind: "signal",
        label: "off",
        Icon: ShieldOff,
        title: "保护条件已禁用",
      },
    ];
  }
  const chips: ProtectionChip[] = [];
  if (p.stopLoss != null) {
    chips.push({
      kind: "stop",
      label: `止损 ${fmtNum(p.stopLoss)}`,
      Icon: TrendingDown,
      title: `止损价 ${fmtNum(p.stopLoss)}`,
    });
  }
  if (p.takeProfit != null) {
    chips.push({
      kind: "take",
      label: `止盈 ${fmtNum(p.takeProfit)}`,
      Icon: Target,
      title: `止盈价 ${fmtNum(p.takeProfit)}`,
    });
  }
  if (p.timeStopAt) {
    chips.push({
      kind: "time",
      label: "时止",
      Icon: Clock,
      title: `时间止损：${p.timeStopAt}`,
    });
  }
  if (p.invalidationSignals && p.invalidationSignals.length > 0) {
    chips.push({
      kind: "signal",
      label: `${p.invalidationSignals.length} 信号`,
      Icon: Shield,
      title: `失效信号：${p.invalidationSignals.join("、")}`,
    });
  }
  return chips;
}

/** unrealizedPnl 占成本比例（用 avgCost * quantity 当成本基）。回退到 marketPrice - avgCost 比例。 */
function pnlPct(p: Position): number | null {
  const cost = toNumber(p.avgCost);
  const qty = p.quantity;
  const unreal = toNumber(p.unrealizedPnl);
  if (cost != null && cost !== 0 && qty > 0 && unreal != null) {
    return (unreal / (cost * qty)) * 100;
  }
  const mp = toNumber(p.marketPrice);
  if (cost != null && cost !== 0 && mp != null) {
    return ((mp - cost) / cost) * 100;
  }
  return null;
}

function PositionWarnings({ warnings }: { warnings: WarningCode[] | undefined }) {
  if (!warnings || warnings.length === 0) return null;
  return (
    <span
      className="position-warning-chip"
      title={`提示：${warnings.join(", ")}`}
    >
      ⚠
    </span>
  );
}

export function PositionsPanel({
  positions,
  loading,
  selected,
  onSelect,
  selectedItem,
}: PositionsPanelProps) {
  // 自动选中第一行 fallback（页面 effect 已处理，但 panel 内部再保险一层）
  useEffect(() => {
    if (!selected && positions.length > 0) {
      onSelect(positions[0].tsCode);
    }
  }, [selected, positions, onSelect]);

  return (
    <div className="positions-panel">
      <div className="panel-section-head">
        <div className="panel-section-title">当前持仓</div>
        <div className="panel-section-meta muted tabular">
          {positions.length} 只
        </div>
      </div>

      <div className="positions-table">
        <div className="positions-table-header" role="row">
          <div className="positions-th left">代码</div>
          <div className="positions-th left">名称</div>
          <div className="positions-th right">持仓</div>
          <div className="positions-th right">可卖</div>
          <div className="positions-th right">成本</div>
          <div className="positions-th right">现价</div>
          <div className="positions-th right">市值</div>
          <div className="positions-th right">浮动盈亏</div>
          <div className="positions-th right">浮动%</div>
          <div className="positions-th left">保护</div>
        </div>
        <div className="positions-table-body">
          {positions.length === 0 && !loading && (
            <div className="positions-empty">
              <div className="muted">暂无持仓</div>
              <div className="faint" style={{ fontSize: 11 }}>
                Agent 开仓后会出现在此
              </div>
            </div>
          )}
          {positions.map((p) => {
            const isSel = p.tsCode === selected;
            const pct = pnlPct(p);
            const chips = buildProtectionChips(p.protection ?? undefined);
            const isTOnePlus = p.sellableQuantity < p.quantity;
            return (
              <div
                key={p.positionId}
                role="row"
                className={`positions-row ${isSel ? "selected" : ""}`}
                onClick={() => onSelect(p.tsCode)}
              >
                <div className="positions-cell left tabular">
                  {p.tsCode}
                  <PositionWarnings warnings={p.warnings} />
                </div>
                <div className="positions-cell left">
                  <span className="instrument-name" title={p.name}>
                    {p.name}
                  </span>
                </div>
                <div className="positions-cell right tabular">
                  {fmtShares(p.quantity)}
                </div>
                <div
                  className={`positions-cell right tabular ${
                    isTOnePlus ? "warn" : ""
                  }`}
                  title={
                    isTOnePlus
                      ? `T+1 锁仓：${p.quantity - p.sellableQuantity} 股不可卖`
                      : undefined
                  }
                >
                  {fmtShares(p.sellableQuantity)}
                  {isTOnePlus && <span className="lock-mark">·</span>}
                </div>
                <div className="positions-cell right tabular">
                  {fmtNum(p.avgCost)}
                </div>
                <div
                  className={`positions-cell right tabular ${changeClass(
                    pnlPct(p),
                  )}`}
                >
                  {fmtNum(p.marketPrice)}
                </div>
                <div className="positions-cell right tabular">
                  {fmtMoneyShort(p.marketValue)}
                </div>
                <div
                  className={`positions-cell right tabular ${pnlClass(
                    p.unrealizedPnl,
                  )}`}
                >
                  {fmtSignedMoney(p.unrealizedPnl)}
                </div>
                <div
                  className={`positions-cell right tabular ${changeClass(pct)}`}
                >
                  {fmtPct(pct)}
                </div>
                <div className="positions-cell left protection-cell">
                  {chips.length === 0 ? (
                    <span className="muted" style={{ fontSize: 11 }}>
                      无保护
                    </span>
                  ) : (
                    chips.map((c, i) => (
                      <span
                        key={`${c.kind}-${i}`}
                        className={`protection-chip protection-${c.kind}`}
                        title={c.title}
                      >
                        <c.Icon size={10} strokeWidth={2} />
                        {c.label}
                      </span>
                    ))
                  )}
                </div>
              </div>
            );
          })}
        </div>
      </div>

      <div className="positions-chart-block">
        <div className="positions-chart-head">
          {selectedItem ? (
            <>
              <span className="positions-chart-name">{selectedItem.name}</span>
              <span className="positions-chart-code tabular muted">
                {selectedItem.tsCode}
              </span>
              <span className="positions-chart-meta muted">日 K · 复权由后端决定</span>
            </>
          ) : (
            <span className="muted">选择持仓查看 K 线</span>
          )}
        </div>
        <div className="positions-chart">
          {/* KlineCanvas 内部自管 loading/empty/error + load-more */}
          {selectedItem && (
            <KlineCanvas tsCode={selectedItem.tsCode} period="day" />
          )}
        </div>
      </div>
    </div>
  );
}
