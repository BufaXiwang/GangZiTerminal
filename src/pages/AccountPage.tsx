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

import { RefreshCcw, RotateCcw } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
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
import { setSource as setPullSource } from "../lib/quotePull";
import { AccountSummary } from "./account/AccountSummary";
import { AddWatchlistModal } from "./account/AddWatchlistModal";
import { PositionsPanel } from "./account/PositionsPanel";
import { WatchlistPanel } from "./account/WatchlistPanel";
import { KlineModal } from "../components/KlineModal";

export default function AccountPage() {
  const [snapshot, setSnapshot] = useState<AccountSnapshot | null>(null);
  const [positions, setPositions] = useState<Position[]>([]);
  const [triggers, setTriggers] = useState<AccountTrigger[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null);
  const [selectedPosition, setSelectedPosition] = useState<TsCode | null>(null);
  const [addOpen, setAddOpen] = useState(false);
  // K 线 modal（点击自选行弹出）
  const [klineModal, setKlineModal] = useState<{
    tsCode: TsCode;
    name?: string | null;
  } | null>(null);

  const setStoreItems = useWatchlistStore((s) => s.setItems);
  const watchlistItems = useWatchlistStore((s) => s.items);

  const refresh = useCallback(async (silent = false) => {
    if (!silent) {
      setLoading(true);
      setError(null);
    }
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
      if (!silent) {
        setError(
          `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
        );
        setLoading(false);
      }
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
    if (!silent) setLoading(false);
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

  // 订阅 universe 行情进度事件，节流后静默重取账户 —— 让自选 / 持仓估值
  // 随行情实时更新（否则只有 mount + 手动刷新两次取数，cache 后填的报价看不到）。
  const lastBgRefetchRef = useRef(0);
  const pendingBgTimerRef = useRef<number | null>(null);
  useEffect(() => {
    const THROTTLE_MS = 3000;
    let unlisten: (() => void) | null = null;
    const doBg = () => {
      lastBgRefetchRef.current = Date.now();
      void refresh(true);
    };
    void listen("market-quotes-refresh-progress", () => {
      const elapsed = Date.now() - lastBgRefetchRef.current;
      if (elapsed >= THROTTLE_MS) {
        doBg();
      } else if (pendingBgTimerRef.current == null) {
        pendingBgTimerRef.current = window.setTimeout(() => {
          pendingBgTimerRef.current = null;
          doBg();
        }, THROTTLE_MS - elapsed);
      }
    }).then((un) => {
      unlisten = un;
    });
    return () => {
      if (pendingBgTimerRef.current != null) {
        window.clearTimeout(pendingBgTimerRef.current);
      }
      unlisten?.();
    };
  }, [refresh]);

  // 聚焦 pull：把自选 + 持仓声明为 account 来源，交给 quotePull 协调器统一刷新
  //（account 优先级最高，cap 截断时不会被列表头挤掉）。卸载时清空贡献。
  useEffect(() => {
    const codes = [
      ...watchlistItems.map((it) => it.tsCode),
      ...positions.map((p) => p.tsCode),
    ];
    setPullSource("account", codes);
  }, [watchlistItems, positions]);
  useEffect(() => () => setPullSource("account", []), []);

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

  // 重置账户 = 重开一局模拟盘（spec account-module.md §4 account_reset）。
  // 带确认弹窗避免误触：清账户财务侧、回初始本金，保留自选；旧局归档可取回。
  const handleReset = useCallback(async () => {
    const ok = window.confirm(
      "重置账户将重开一局模拟盘：清空持仓 / 挂单 / 成交 / 账户事件，现金回初始本金。\n" +
        "自选股保留，旧局数据会归档（可取回）。Agent 决策记录不受影响。\n\n" +
        "确定要重置吗？",
    );
    if (!ok) return;
    setLoading(true);
    setError(null);
    const res = await commands.accountReset();
    if (res.status === "error") {
      setError(
        `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
      );
      setLoading(false);
      return;
    }
    await refresh();
  }, [refresh]);

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
          <>
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
            <button
              type="button"
              className="btn"
              onClick={() => void handleReset()}
              disabled={loading}
              title="重置账户（重开一局，保留自选）"
            >
              <RotateCcw size={14} />
              <span>重置账户</span>
            </button>
          </>
        }
      >
        <AccountSummary snapshot={snapshot} loading={loading} error={error} />

        <div className="account-workspace">
          <div className="account-workspace-side">
            <WatchlistPanel
              items={watchlistItems}
              onOpenAdd={() => setAddOpen(true)}
              loading={loading}
              onSelect={(tsCode, name) => setKlineModal({ tsCode, name })}
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

      <KlineModal
        open={klineModal !== null}
        tsCode={klineModal?.tsCode ?? null}
        name={klineModal?.name}
        category="stock"
        onClose={() => setKlineModal(null)}
      />
    </>
  );
}
