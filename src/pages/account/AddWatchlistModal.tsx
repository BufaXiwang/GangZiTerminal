// AddWatchlistModal — 添加自选 modal（type-to-search list_market）。C5 sub-step 填充。
//
// Spec: docs/design/account-module.md §4 update_watchlist (action=add)
//        + quotes-module.md §4 list_market

interface AddWatchlistModalProps {
  open: boolean;
  onClose: () => void;
  onDone: () => void;
}

export function AddWatchlistModal({ open, onClose, onDone: _onDone }: AddWatchlistModalProps) {
  if (!open) return null;
  return (
    <div className="article-drawer-overlay" onClick={onClose}>
      <div
        className="add-watchlist-modal-placeholder"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="muted">添加自选 modal（C5 填充）</div>
        <button className="btn" type="button" onClick={onClose}>
          关闭
        </button>
      </div>
    </div>
  );
}
