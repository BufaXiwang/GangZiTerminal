// WatchlistPanel — 紧凑自选股列表（与 MarketList compact 风格一致）。
//
// Spec:
//   docs/design/account-module.md §2 自选模型 / WatchlistItemView
//   docs/design/account-module.md §4 update_watchlist (action=remove)
//   docs/design/frontend-design.md §4 模拟账户页
//
// 行布局：
//   左：name (大字加粗) + ST chip + tsCode (小灰)
//   右：price (tabular) + change% (红绿)
//   hover：右上角浮出删除按钮
// 点击行 → 父级 onSelect 弹 K 线 modal。

import { Trash2 } from "lucide-react";
import { Plus } from "lucide-react";
import { useState } from "react";
import type { TsCode, WatchlistItemView } from "../../bindings";
import { useWatchlistStore } from "../../lib/watchlistStore";
import { changeClass, fmtNum, fmtPct } from "./format";

interface WatchlistPanelProps {
  items: WatchlistItemView[];
  loading: boolean;
  onOpenAdd: () => void;
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

  const handleRemove = async (tsCode: TsCode, name?: string | null) => {
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
        <div className="panel-section-meta muted tabular">{items.length} 只</div>
        <button
          type="button"
          className="btn primary watchlist-add-btn"
          onClick={onOpenAdd}
        >
          <Plus size={12} />
          添加自选
        </button>
      </div>

      <div className="market-list">
        <div className="market-list-body">
          {items.length === 0 && !loading && (
            <div className="market-list-empty">
              <div>暂无自选标的</div>
              <div className="muted" style={{ fontSize: 12, marginTop: 4 }}>
                点击右上角添加
              </div>
            </div>
          )}
          {items.map((item) => {
            const pct = item.quote?.changePercent;
            const tone = changeClass(pct);
            const fresh = item.quote?.freshness?.status;
            const isStale = fresh === "stale" || fresh === "missing";
            return (
              <div
                key={item.tsCode}
                role="row"
                className={`market-list-row compact watchlist-compact-row ${onSelect ? "clickable" : ""} ${isStale ? "stale" : ""}`}
                onClick={() => onSelect?.(item.tsCode, item.name)}
              >
                <div className="row-left">
                  <div className="row-name">
                    <span className="instrument-name" title={item.name ?? undefined}>
                      {item.name ?? "-"}
                    </span>
                  </div>
                  <div className="row-code tabular">{item.tsCode}</div>
                </div>
                <div className="row-right">
                  <div className={`row-price tabular ${tone}`}>
                    {fmtNum(item.quote?.price)}
                  </div>
                  <div className={`row-pct tabular ${tone}`}>{fmtPct(pct)}</div>
                </div>
                <button
                  type="button"
                  className="watchlist-row-remove"
                  onClick={(e) => {
                    e.stopPropagation();
                    void handleRemove(item.tsCode, item.name);
                  }}
                  disabled={busy === item.tsCode}
                  title="从自选中移除"
                  aria-label="从自选中移除"
                >
                  <Trash2 size={12} />
                </button>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}
