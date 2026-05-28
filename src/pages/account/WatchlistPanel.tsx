// WatchlistPanel — 自选股表 + 删除 + 添加入口。C4 sub-step 填充。
//
// Spec: docs/design/account-module.md §2 自选模型 + §4 update_watchlist

import type { WatchlistItemView } from "../../bindings";

interface WatchlistPanelProps {
  items: WatchlistItemView[];
  loading: boolean;
  onOpenAdd: () => void;
}

export function WatchlistPanel(_props: WatchlistPanelProps) {
  return (
    <div className="watchlist-panel-placeholder">
      <div className="muted">自选股（C4 填充）</div>
    </div>
  );
}
