// AddWatchlistModal — 添加自选 modal（type-to-search list_market）。
//
// Spec:
//   docs/design/account-module.md §4 update_watchlist (action=add)
//   docs/design/quotes-module.md §4 line 549 list_market (query + limit)
//   docs/design/frontend-design.md §4 模拟账户页 / §5 modal
//
// 行为：
//   - 输入框 debounced 300ms 调 listMarket({ query, limit: 20, includeQuote: false })
//   - 结果列表显示 ts_code / name / category badge / status
//   - 点击行选中；底部「添加自选」(primary) + 「取消」(ghost)
//   - 确认 → watchlistStore.add(tsCode)；成功后 onDone()（关 modal + 触发账户页 refresh）
//   - ESC / overlay 点击关闭
//   - 已在自选集合里的 ts_code 标记为 "已添加"，按钮禁用

import { Check, Search, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  commands,
  type InstrumentCategory,
  type ListMarketItem,
  type TsCode,
} from "../../bindings";
import { useWatchlistStore } from "../../lib/watchlistStore";

interface AddWatchlistModalProps {
  open: boolean;
  onClose: () => void;
  /** add 成功后回调（关 modal + 父刷新 fetch_account） */
  onDone: () => void;
}

const CATEGORY_LABEL: Record<InstrumentCategory, string> = {
  stock: "股票",
  index: "指数",
  fund: "基金",
};

export function AddWatchlistModal({ open, onClose, onDone }: AddWatchlistModalProps) {
  const [query, setQuery] = useState("");
  const [items, setItems] = useState<ListMarketItem[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<TsCode | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const watchedCodes = useWatchlistStore((s) => s.codes);
  const addToStore = useWatchlistStore((s) => s.add);

  const inputRef = useRef<HTMLInputElement | null>(null);
  const reqIdRef = useRef(0);

  // 打开 / 关闭时重置
  useEffect(() => {
    if (open) {
      setQuery("");
      setItems([]);
      setSelected(null);
      setError(null);
      // focus 输入框
      setTimeout(() => inputRef.current?.focus(), 0);
    }
  }, [open]);

  // ESC 关闭
  useEffect(() => {
    if (!open) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [open, onClose]);

  // debounced 搜索：输入变化 300ms 后才请求
  useEffect(() => {
    if (!open) return;
    const trimmed = query.trim();
    if (trimmed.length === 0) {
      setItems([]);
      setLoading(false);
      setError(null);
      return;
    }
    const id = ++reqIdRef.current;
    setLoading(true);
    setError(null);
    const t = window.setTimeout(() => {
      void commands
        .listMarket({
          query: trimmed,
          includeQuote: false,
          limit: 20,
          offset: 0,
        })
        .then((res) => {
          if (id !== reqIdRef.current) return; // stale
          if (res.status === "error") {
            setError(
              `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
            );
            setItems([]);
            setLoading(false);
            return;
          }
          setItems(res.data.items);
          setLoading(false);
        });
    }, 300);
    return () => {
      window.clearTimeout(t);
    };
  }, [open, query]);

  const selectedItem = useMemo(
    () => items.find((it) => it.tsCode === selected) ?? null,
    [items, selected],
  );

  const canAdd =
    selectedItem != null && !watchedCodes.has(selectedItem.tsCode) && !submitting;

  const handleSubmit = useCallback(async () => {
    if (!selectedItem) return;
    setSubmitting(true);
    const ok = await addToStore(selectedItem.tsCode);
    setSubmitting(false);
    if (ok) {
      onDone();
    } else {
      setError(`添加 ${selectedItem.tsCode} 失败（标的可能尚未加载完成，稍后重试）`);
    }
  }, [selectedItem, addToStore, onDone]);

  if (!open) return null;

  return (
    <div
      className="add-watchlist-overlay"
      onClick={onClose}
      role="presentation"
    >
      <div
        className="add-watchlist-modal"
        onClick={(e) => e.stopPropagation()}
        role="dialog"
        aria-label="添加自选"
      >
        <header className="add-watchlist-head">
          <div className="add-watchlist-title">添加自选</div>
          <button
            type="button"
            className="btn ghost"
            onClick={onClose}
            aria-label="关闭"
          >
            <X size={14} />
          </button>
        </header>

        <div className="add-watchlist-search">
          <Search size={14} className="search-icon" />
          <input
            ref={inputRef}
            type="search"
            placeholder="搜索代码 / 名称 / 拼音首字母"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
        </div>

        <div className="add-watchlist-body">
          {error && (
            <div className="add-watchlist-error">加载失败：{error}</div>
          )}
          {!error && query.trim() === "" && (
            <div className="add-watchlist-hint muted">
              输入代码或名称（如 600519 / 茅台）开始搜索
            </div>
          )}
          {!error && query.trim() !== "" && loading && (
            <div className="add-watchlist-hint muted">搜索中…</div>
          )}
          {!error && query.trim() !== "" && !loading && items.length === 0 && (
            <div className="add-watchlist-hint muted">未匹配到标的</div>
          )}
          {!error && items.length > 0 && (
            <div className="add-watchlist-results">
              {items.map((it) => {
                const isSelected = selected === it.tsCode;
                const isAlready = watchedCodes.has(it.tsCode);
                return (
                  <button
                    key={it.tsCode}
                    type="button"
                    className={`add-watchlist-result ${isSelected ? "selected" : ""} ${
                      isAlready ? "already" : ""
                    }`}
                    onClick={() => {
                      if (!isAlready) setSelected(it.tsCode);
                    }}
                    disabled={isAlready}
                    aria-pressed={isSelected}
                  >
                    <span className="add-watchlist-result-code tabular">
                      {it.tsCode}
                    </span>
                    <span className="add-watchlist-result-name">
                      {it.name}
                      {it.isSt && <span className="chip st-chip">ST</span>}
                    </span>
                    <span className="chip add-watchlist-cat-chip">
                      {CATEGORY_LABEL[it.category] ?? it.category}
                    </span>
                    {isAlready && (
                      <span className="add-watchlist-already-chip">
                        <Check size={11} /> 已添加
                      </span>
                    )}
                  </button>
                );
              })}
            </div>
          )}
        </div>

        <footer className="add-watchlist-footer">
          <button
            type="button"
            className="btn ghost"
            onClick={onClose}
            disabled={submitting}
          >
            取消
          </button>
          <button
            type="button"
            className="btn primary"
            onClick={() => void handleSubmit()}
            disabled={!canAdd}
          >
            {submitting ? "添加中…" : "添加自选"}
          </button>
        </footer>
      </div>
    </div>
  );
}
