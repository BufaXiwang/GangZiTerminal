// WatchlistPanel — 自选股表 + 删除 + 添加入口。
//
// Spec:
//   docs/design/account-module.md §2 自选模型 / WatchlistItemView
//   docs/design/account-module.md §4 update_watchlist (action=remove)
//   docs/design/frontend-design.md §4 模拟账户页
//
// 列：代码 | 名称 | 现价 | 涨跌% | 成交额 | 删除
// - 行点击 → 跳到市场页（后续可加）；当前只展示
// - 删除 → confirm() → store.remove → 后端 update_watchlist + 本地刷新
// - 顶部「+ 添加自选」按钮 → 打开 AddWatchlistModal

import { Plus, Trash2 } from "lucide-react";
import { useState } from "react";
import type { TsCode, WatchlistItemView } from "../../bindings";
import { useWatchlistStore } from "../../lib/watchlistStore";
import {
  changeClass,
  fmtAmountShort,
  fmtNum,
  fmtPct,
} from "./format";

interface WatchlistPanelProps {
  items: WatchlistItemView[];
  loading: boolean;
  onOpenAdd: () => void;
  /** 点击行（非操作按钮）→ 弹 K 线 modal */
  onSelect?: (tsCode: TsCode, name?: string | null) => void;
}

export function WatchlistPanel({
  items,
  loading,
  onOpenAdd,
  onSelect,
}: WatchlistPanelProps) {
  const remove = useWatchlistStore((s) => s.remove);
  const [busy, setBusy] = useState<TsCode | null>(null);

  const handleRemove = async (tsCode: TsCode, name?: string) => {
    const label = name ? `${name} (${tsCode})` : tsCode;
    if (!window.confirm(`从自选中移除 ${label}?`)) return;
    setBusy(tsCode);
    await remove(tsCode);
    setBusy(null);
  };

  return (
    <div className="watchlist-panel">
      <div className="panel-section-head">
        <div className="panel-section-title">自选股</div>
        <div className="panel-section-meta muted tabular">
          {items.length} 只
        </div>
        <button
          type="button"
          className="btn primary watchlist-add-btn"
          onClick={onOpenAdd}
        >
          <Plus size={12} />
          添加自选
        </button>
      </div>

      <div className="watchlist-table">
        <div className="watchlist-table-header" role="row">
          <div className="watchlist-th left">代码</div>
          <div className="watchlist-th left">名称</div>
          <div className="watchlist-th right">现价</div>
          <div className="watchlist-th right">涨跌%</div>
          <div className="watchlist-th right">成交额</div>
          <div className="watchlist-th right">操作</div>
        </div>
        <div className="watchlist-table-body">
          {items.length === 0 && !loading && (
            <div className="watchlist-empty">
              <div className="muted">暂无自选标的</div>
              <div className="muted" style={{ fontSize: 12, marginTop: 4 }}>
                点击右上角添加
              </div>
            </div>
          )}
          {items.map((it) => {
            const pct = it.quote?.changePercent;
            const fresh = it.quote?.freshness?.status;
            const isStale = fresh === "stale" || fresh === "missing";
            return (
              <div
                key={it.tsCode}
                role="row"
                className={`watchlist-row ${isStale ? "stale" : ""} ${onSelect ? "clickable" : ""}`}
                onClick={() => onSelect?.(it.tsCode, it.name)}
              >
                <div className="watchlist-cell left tabular">{it.tsCode}</div>
                <div className="watchlist-cell left">
                  <span className="instrument-name" title={it.name}>
                    {it.name ?? "-"}
                  </span>
                  {it.note && (
                    <span
                      className="watchlist-note-chip"
                      title={it.note}
                    >
                      备
                    </span>
                  )}
                </div>
                <div
                  className={`watchlist-cell right tabular ${changeClass(pct)}`}
                  title={
                    fresh === "missing"
                      ? "行情缺失"
                      : fresh === "stale"
                        ? "行情过期"
                        : undefined
                  }
                >
                  {fmtNum(it.quote?.price)}
                </div>
                <div
                  className={`watchlist-cell right tabular ${changeClass(pct)}`}
                >
                  {fmtPct(pct)}
                </div>
                <div className="watchlist-cell right tabular">
                  {fmtAmountShort(it.quote?.amount)}
                </div>
                <div className="watchlist-cell right">
                  <button
                    type="button"
                    className="btn ghost watchlist-remove-btn"
                    onClick={(e) => {
                      e.stopPropagation();
                      void handleRemove(it.tsCode, it.name);
                    }}
                    disabled={busy === it.tsCode}
                    title="从自选中移除"
                    aria-label="从自选中移除"
                  >
                    <Trash2 size={12} />
                  </button>
                </div>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}
