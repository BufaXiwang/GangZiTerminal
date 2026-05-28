// AccountPage — 模拟账户页（F4 实现）。
//
// Spec:
//   docs/design/frontend-design.md §4 模拟账户页
//   docs/design/account-module.md §2 / §4 fetch_account + update_watchlist
//
// 数据流：
//   - mount 时调 fetch_account({ include: { snapshot, positions, watchlist, triggers } })
//   - 自选行情视图同步到 watchlist store（市场页 ⭐ 共享同一份集合）
//   - 用户没有交易写入口（spec §4：人工 UI 不能下单 / 调仓），只能管自选
//
// 结构：
//   PageShell
//     AccountSummary
//     workspace 左主：PositionsPanel + 选中持仓 K 线（KlineCanvas）
//                右：WatchlistPanel
//   AddWatchlistModal（条件渲染）

import { RefreshCcw } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { PageShell } from "../components/PageShell";
import {
  commands,
  type AccountSnapshot,
  type AccountTrigger,
  type Position,
  type TsCode,
  type WatchlistItemView,
} from "../bindings";
import { useWatchlistStore } from "../lib/watchlistStore";
import { AccountSummary } from "./account/AccountSummary";
import { AddWatchlistModal } from "./account/AddWatchlistModal";
import { PositionsPanel } from "./account/PositionsPanel";
import { WatchlistPanel } from "./account/WatchlistPanel";

export default function AccountPage() {
  const [snapshot, setSnapshot] = useState<AccountSnapshot | null>(null);
  const [positions, setPositions] = useState<Position[]>([]);
  const [triggers, setTriggers] = useState<AccountTrigger[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null);
  const [selectedPosition, setSelectedPosition] = useState<TsCode | null>(null);
  const [addOpen, setAddOpen] = useState(false);

  const setStoreItems = useWatchlistStore((s) => s.setItems);
  const watchlistItems = useWatchlistStore((s) => s.items);

  const refresh = useCallback(async () => {
    setLoading(true);
    setError(null);
    const res = await commands.fetchAccount({
      include: {
        snapshot: true,
        positions: true,
        watchlist: true,
        triggers: true,
      },
      positionStatus: "open",
      triggerHandled: false,
    });
    if (res.status === "error") {
      setError(
        `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
      );
      setLoading(false);
      return;
    }
    setSnapshot(res.data.snapshot ?? null);
    const ps = res.data.positions ?? [];
    setPositions(ps);
    setTriggers(res.data.triggers ?? []);
    // 同步自选到 store（一次拉取多用，避免重复 IPC）
    if (res.data.watchlist) {
      setStoreItems(res.data.watchlist as WatchlistItemView[]);
    }
    setLoading(false);
    setLastUpdated(new Date());
    // 默认选中第一只持仓
    setSelectedPosition((prev) => {
      if (prev && ps.some((p) => p.tsCode === prev)) return prev;
      return ps[0]?.tsCode ?? null;
    });
  }, [setStoreItems]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const status = error
    ? `加载失败：${error}`
    : loading
      ? "加载中"
      : lastUpdated
        ? `已更新 ${lastUpdated.toLocaleTimeString("zh-CN", { hour12: false })}`
        : "等待数据";
  const statusTone = error
    ? "error"
    : loading
      ? "loading"
      : lastUpdated
        ? "ok"
        : "stale";

  const selectedPositionItem = useMemo(
    () => positions.find((p) => p.tsCode === selectedPosition) ?? null,
    [positions, selectedPosition],
  );

  const handleAddDone = useCallback(() => {
    setAddOpen(false);
    // 触发刷新以拿到新自选的行情视图（store 已经乐观加 codes，但 items 缺新行）
    void refresh();
  }, [refresh]);

  const triggerCount = triggers.filter((t) => !t.handled).length;

  return (
    <>
      <PageShell
        title="模拟账户"
        status={status}
        statusTone={statusTone}
        meta={
          triggerCount > 0 ? (
            <span className="account-trigger-meta">
              <span className="status-dot warn" aria-hidden="true" />
              待处理触发 {triggerCount}
            </span>
          ) : undefined
        }
        actions={
          <button
            type="button"
            className="btn"
            onClick={() => void refresh()}
            disabled={loading}
            title="刷新"
          >
            <RefreshCcw size={14} />
            <span>刷新</span>
          </button>
        }
      >
        <AccountSummary snapshot={snapshot} loading={loading} error={error} />

        <div className="account-workspace">
          <div className="account-workspace-side">
            <WatchlistPanel
              items={watchlistItems}
              onOpenAdd={() => setAddOpen(true)}
              loading={loading}
            />
          </div>
          <div className="account-workspace-main">
            <PositionsPanel
              positions={positions}
              loading={loading}
              selected={selectedPosition}
              onSelect={setSelectedPosition}
              selectedItem={selectedPositionItem}
            />
          </div>
        </div>
      </PageShell>

      <AddWatchlistModal
        open={addOpen}
        onClose={() => setAddOpen(false)}
        onDone={handleAddDone}
      />
    </>
  );
}
