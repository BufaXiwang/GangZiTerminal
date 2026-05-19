import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Layers } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { EmptyState, Stat } from "./LessonsPage";

type Strategy = {
  id: string;
  name: string;
  description: string;
  triggerWhen: Array<{ signal: { kind: string; [k: string]: unknown } }>;
  target: { direction: string; pctRelativeToCurrent: number; horizonDays: number };
  enabled: boolean;
  appliedCount: number;
  hitCount: number;
  missCount: number;
  createdAt: number;
  updatedAt: number;
};

export function StrategiesPage({ onAskAgent }: { onAskAgent?: (prefill: string) => void }) {
  const [list, setList] = useState<Strategy[]>([]);

  useEffect(() => {
    const load = () => void invoke<Strategy[]>("list_strategies").then(setList);
    load();
    const unsub = listen("strategies-changed", load);
    return () => {
      void unsub.then((u) => u());
    };
  }, []);

  const summary = useMemo(() => {
    let enabled = 0;
    let applied = 0;
    let hit = 0;
    let miss = 0;
    for (const s of list) {
      if (s.enabled) enabled++;
      applied += s.appliedCount;
      hit += s.hitCount;
      miss += s.missCount;
    }
    const judged = hit + miss;
    const hitRate = judged >= 3 ? `${Math.round((hit / judged) * 100)}%` : "样本不足";
    return { enabled, applied, hit, miss, hitRate };
  }, [list]);

  return (
    <section className="page-shell agent-subpage">
      <header className="section-head">
        <div>
          <h2>策略</h2>
          <p>用户 + agent 共建的"什么时候建 expectation"规则集。chat 跟 agent 说话可修改。</p>
        </div>
      </header>

      {list.length === 0 ? (
        <EmptyState
          icon={<Layers size={28} strokeWidth={1.4} />}
          title="还没有策略"
          body="策略是「什么信号组合 → 建什么预期」的可执行规则。启动时应该会 seed 3 条（资金驱动 / 超跌反弹 / 动量突破）；如果是空的可能是 seed 流程出错。"
          hint="可以跟 agent 在对话里描述想观察的形态，让它帮你新增策略。"
        />
      ) : (
        <>
          <div className="agent-stats-strip">
            <Stat label="启用 / 总数" value={`${summary.enabled} / ${list.length}`} />
            <Stat label="总应用次数" value={summary.applied} />
            <Stat label="命中" value={summary.hit} tone={summary.hit > 0 ? "good" : "muted"} />
            <Stat label="未中" value={summary.miss} tone={summary.miss > 0 ? "danger" : "muted"} />
            <Stat label="整体命中率" value={summary.hitRate} />
          </div>
          <div className="agent-card-list">
          {list.map((s) => {
            const total = s.hitCount + s.missCount;
            const conf = total >= 3 ? `${((s.hitCount / total) * 100).toFixed(0)}%` : "样本不足";
            return (
              <article key={s.id} className={`agent-card${s.enabled ? "" : " dim"}`}>
                <div className="agent-card-head">
                  <span className="agent-card-title">{s.name}</span>
                  <button
                    type="button"
                    className={`agent-badge ${s.enabled ? "good" : "neutral"} dot`}
                    style={{ cursor: "pointer", border: 0 }}
                    title={s.enabled ? "点击 disable" : "点击 enable"}
                    onClick={() =>
                      void invoke("set_strategy_enabled", {
                        strategyId: s.id,
                        enabled: !s.enabled,
                      }).catch((err) => window.alert(`toggle 失败：${err}`))
                    }
                  >
                    {s.enabled ? "已启用" : "已停用"}
                  </button>
                  <span className="agent-card-meta">
                    <span>applied · {s.appliedCount}</span>
                    <span>
                      hit/miss · {s.hitCount}/{s.missCount}
                    </span>
                    <span>conf · {conf}</span>
                  </span>
                </div>
                <div className="agent-card-body">{s.description}</div>
                <div className="agent-card-sub">
                  触发信号 · {s.triggerWhen.map((t) => t.signal.kind).join(" + ")}
                  <br />
                  目标 · {s.target.direction} {s.target.pctRelativeToCurrent}% / {s.target.horizonDays}d
                </div>
                {onAskAgent && (
                  <div className="agent-card-actions">
                    <button
                      type="button"
                      className="agent-mini-btn"
                      onClick={() => onAskAgent(`[关于策略 "${s.name}"]: `)}
                    >
                      问 agent
                    </button>
                  </div>
                )}
              </article>
            );
          })}
          </div>
        </>
      )}
    </section>
  );
}
