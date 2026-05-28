// AccountSummary — 模拟账户页顶部账户摘要 cards。
//
// Spec: docs/design/account-module.md §2 AccountSnapshot + §4 fetch_account
//        docs/design/frontend-design.md §4 模拟账户页 / §5 卡片
//
// 紧凑 summary 行（不堆大卡片）：
//   - 主值「总资产」用大字 mono；旁边一个「总盈亏」chip（红涨绿跌）
//   - 次级指标三列：现金 / 持仓市值 / 已实现+未实现拆分
//   - 风控状态：valuationFreshness pill + 未定价仓位计数 + warnings 标记
//
// 字段全部来自 AccountSnapshot。缺失显示 `-`。

import type { AccountSnapshot, FreshnessStatus, WarningCode } from "../../bindings";
import {
  fmtDateTime,
  fmtMoneyShort,
  fmtSignedMoney,
  pnlClass,
  toNumber,
} from "./format";

interface AccountSummaryProps {
  snapshot: AccountSnapshot | null;
  loading: boolean;
  error: string | null;
}

const FRESHNESS_LABEL: Record<FreshnessStatus, string> = {
  fresh: "估值实时",
  stale: "估值过期",
  missing: "估值缺失",
};

const FRESHNESS_TONE: Record<FreshnessStatus, "ok" | "warn" | "error"> = {
  fresh: "ok",
  stale: "warn",
  missing: "error",
};

function fmtWarning(code: WarningCode): string {
  return code;
}

export function AccountSummary({ snapshot, loading, error }: AccountSummaryProps) {
  if (error) {
    return (
      <div className="account-summary account-summary-error">
        <span className="muted">账户加载失败：{error}</span>
      </div>
    );
  }
  if (!snapshot && loading) {
    return (
      <div className="account-summary account-summary-loading">
        <span className="muted">账户加载中…</span>
      </div>
    );
  }
  if (!snapshot) {
    return (
      <div className="account-summary account-summary-empty">
        <span className="muted">暂无账户数据</span>
      </div>
    );
  }

  const totalPnlClass = pnlClass(snapshot.totalPnl);
  const fresh = snapshot.valuationFreshness;
  const freshTone = FRESHNESS_TONE[fresh.status];
  const warnings = snapshot.warnings ?? [];
  const hasUnpriced = (snapshot.unpricedPositionCount ?? 0) > 0;

  return (
    <div className="account-summary">
      {/* 主行：总资产 + 总盈亏 + 估值 pill */}
      <div className="account-summary-headline">
        <div className="account-summary-totals">
          <div className="account-summary-label">总资产</div>
          <div className="account-summary-total tabular">
            {fmtMoneyShort(snapshot.totalAssets)}
          </div>
        </div>
        <div className="account-summary-pnl-block">
          <div className="account-summary-label">总盈亏</div>
          <div className={`account-summary-pnl tabular ${totalPnlClass}`}>
            {fmtSignedMoney(snapshot.totalPnl)}
          </div>
          <div className="account-summary-pnl-detail muted tabular">
            已实现 <span className={pnlClass(snapshot.realizedPnl)}>
              {fmtSignedMoney(snapshot.realizedPnl)}
            </span>
            <span className="dot-sep">·</span>
            浮动 <span className={pnlClass(snapshot.unrealizedPnl)}>
              {fmtSignedMoney(snapshot.unrealizedPnl)}
            </span>
          </div>
        </div>
        <div className="account-summary-fresh-block">
          <span className={`account-fresh-pill ${freshTone}`}>
            <span className={`status-dot ${freshTone}`} aria-hidden="true" />
            {FRESHNESS_LABEL[fresh.status]}
          </span>
          {fresh.capturedAt && (
            <div className="account-summary-captured muted tabular">
              {fmtDateTime(fresh.capturedAt)}
            </div>
          )}
        </div>
      </div>

      {/* 次行：现金分项 + 持仓市值 + 开仓数 / 挂单数 */}
      <div className="account-summary-grid">
        <SummaryCell label="现金" value={fmtMoneyShort(snapshot.cash)} />
        <SummaryCell
          label="可用"
          value={fmtMoneyShort(snapshot.availableCash)}
        />
        <SummaryCell
          label="冻结"
          value={fmtMoneyShort(snapshot.frozenCash)}
        />
        <SummaryCell
          label="持仓市值"
          value={fmtMoneyShort(snapshot.marketValue)}
        />
        <SummaryCell
          label="开仓数"
          value={(snapshot.openPositionCount ?? 0).toString()}
          subtle={
            hasUnpriced
              ? `${snapshot.unpricedPositionCount} 未定价`
              : undefined
          }
          subtleTone={hasUnpriced ? "warn" : undefined}
        />
        <SummaryCell
          label="挂单数"
          value={(snapshot.pendingOrderCount ?? 0).toString()}
        />
        <SummaryCell
          label="初始资金"
          value={fmtMoneyShort(snapshot.initialCash)}
          subtle={pnlVsInitial(snapshot)}
          subtleTone={pnlClass(snapshot.totalPnl)}
        />
      </div>

      {warnings.length > 0 && (
        <div className="account-summary-warnings">
          <span className="warn-icon" aria-hidden="true">
            ⚠
          </span>
          {warnings.map((w) => (
            <span key={w} className="account-warning-chip">
              {fmtWarning(w)}
            </span>
          ))}
        </div>
      )}
    </div>
  );
}

function pnlVsInitial(s: AccountSnapshot): string | undefined {
  const initial = toNumber(s.initialCash);
  const total = toNumber(s.totalPnl);
  if (initial == null || initial === 0 || total == null) return undefined;
  const ratio = (total / initial) * 100;
  const sign = ratio > 0 ? "+" : "";
  return `${sign}${ratio.toFixed(2)}%`;
}

interface SummaryCellProps {
  label: string;
  value: string;
  subtle?: string;
  subtleTone?: "up" | "down" | "flat" | "warn";
}

function SummaryCell({ label, value, subtle, subtleTone }: SummaryCellProps) {
  return (
    <div className="account-summary-cell">
      <div className="account-summary-cell-label">{label}</div>
      <div className="account-summary-cell-value tabular">{value}</div>
      {subtle && (
        <div className={`account-summary-cell-subtle tabular ${subtleTone ?? ""}`}>
          {subtle}
        </div>
      )}
    </div>
  );
}
