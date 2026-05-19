import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Lightbulb } from "lucide-react";
import { useEffect, useState } from "react";
import { EmptyState, Stat } from "./LessonsPage";

type Heuristic = {
  id: string;
  body: string;
  category: "principle" | "known_bias" | "risk_preference";
  origin: "seed" | "user_stated" | "agent_inferred";
  regimeTags: string[];
  supportingLessonIds: string[];
  applicationCount: number;
  hitCount: number;
  missCount: number;
  confidence: number | null;
  effectiveState: "active" | "challenged" | "probationary" | "dormant" | "retired";
  lastAppliedAt: number | null;
  retiredAt: number | null;
  createdAt: number;
};

type Counts = { seed: number; userStated: number; agentInferred: number; retired: number };

const STATE_LABEL: Record<Heuristic["effectiveState"], string> = {
  active: "active",
  challenged: "challenged",
  probationary: "probationary",
  dormant: "dormant",
  retired: "retired",
};

const STATE_BADGE: Record<
  Heuristic["effectiveState"],
  "good" | "warn" | "brand" | "neutral"
> = {
  active: "good",
  challenged: "warn",
  probationary: "brand",
  dormant: "neutral",
  retired: "neutral",
};

const ORIGIN_LABEL: Record<Heuristic["origin"], string> = {
  seed: "种子",
  user_stated: "用户",
  agent_inferred: "agent",
};

const STATE_ORDER: Heuristic["effectiveState"][] = [
  "active",
  "challenged",
  "probationary",
  "dormant",
  "retired",
];

export function HeuristicsPage({ onAskAgent }: { onAskAgent?: (prefill: string) => void }) {
  const [list, setList] = useState<Heuristic[]>([]);
  const [counts, setCounts] = useState<Counts | null>(null);

  useEffect(() => {
    const load = () => {
      void invoke<Heuristic[]>("list_heuristics", { limit: 200 }).then(setList);
      void invoke<Counts>("get_heuristic_counts").then(setCounts);
    };
    load();
    const unsub = listen("heuristics-changed", load);
    return () => {
      void unsub.then((u) => u());
    };
  }, []);

  const handleRetire = (h: Heuristic) => {
    const reason = window.prompt(
      `停用启发式\n"${h.body.slice(0, 60)}…"\n\n停用后不再注入 prompt 影响决策（保留历史记录）。请填停用原因（≥4 字）：`,
      "",
    );
    if (!reason || reason.trim().length < 4) return;
    void invoke("retire_heuristic_cmd", {
      heuristicId: h.id,
      reason: reason.trim(),
    }).catch((err) => window.alert(`停用失败：${err}`));
  };

  // 近 7 天 emerge——origin=agent_inferred 且 createdAt 近 7 日。这是 reflection
  // 复盘链是否真在产出的关键 canary：连续 2 周为 0 说明 emerge 死了。
  const emerged7d = list.filter(
    (h) => h.origin === "agent_inferred" && h.createdAt >= Date.now() - 7 * 24 * 3600 * 1000,
  ).length;

  return (
    <section className="page-shell agent-subpage">
      <header className="section-head">
        <div>
          <h2>启发式</h2>
          <p>Agent 学到的（或用户给的）启发式规则——带 confidence track record。</p>
        </div>
      </header>

      {list.length === 0 ? (
        <EmptyState
          icon={<Lightbulb size={28} strokeWidth={1.4} />}
          title="还没有启发式"
          body="启发式是 agent 跨次决策复用的判断（既包括种子规则，也包括从 lessons 中 emerge 出来的新规则）。启动时应该 seed 10 条；如果是空的，可能是 seed 流程出错。"
          hint="跟 agent 在对话里说「记住这条 …」可以新增 user 类型启发式。"
        />
      ) : (
        <>
          <div className="agent-stats-strip">
            {counts && (
              <>
                <Stat label="种子" value={counts.seed} />
                <Stat label="用户" value={counts.userStated} />
                <Stat label="agent 学到" value={counts.agentInferred} tone={counts.agentInferred > 0 ? "good" : "muted"} />
                <Stat label="已停用" value={counts.retired} tone="muted" />
                <Stat
                  label="近 7 天 emerge"
                  value={emerged7d}
                  tone={emerged7d === 0 && counts.agentInferred > 0 ? "danger" : undefined}
                  hint={emerged7d === 0 && counts.agentInferred > 0 ? "emerge 链路可能死了" : undefined}
                />
              </>
            )}
          </div>
          {STATE_ORDER.map((state) => {
          const items = list.filter((h) => h.effectiveState === state);
          if (items.length === 0) return null;
          return (
            <div key={state} className="agent-group">
              <h3 className="agent-group-head">
                <span className={`agent-badge ${STATE_BADGE[state]} dot`}>{STATE_LABEL[state]}</span>
                <small>{items.length} 条</small>
              </h3>
              <div className="agent-card-list">
                {items.map((h) => (
                  <article
                    key={h.id}
                    className={`agent-card${state === "retired" || state === "dormant" ? " dim" : ""}`}
                  >
                    <div className="agent-card-head">
                      <span className="agent-badge brand">{h.category}</span>
                      <span className="agent-card-meta">
                        <span>来源 · {ORIGIN_LABEL[h.origin]}</span>
                        <span>
                          命中 {h.hitCount} · 未中 {h.missCount}
                          {h.confidence !== null && ` · 置信 ${(h.confidence * 100).toFixed(0)}%`}
                        </span>
                      </span>
                    </div>
                    <div className="agent-card-body">{h.body}</div>
                    {(h.regimeTags.length > 0 || h.supportingLessonIds.length > 0) && (
                      <div className="agent-card-sub">
                        {h.regimeTags.length > 0 && <>regime · {h.regimeTags.join(", ")}　</>}
                        {h.supportingLessonIds.length > 0 && (
                          <>支持的 lessons · {h.supportingLessonIds.length} 条</>
                        )}
                      </div>
                    )}
                    <div className="agent-card-actions">
                      {onAskAgent && (
                        <button
                          type="button"
                          className="agent-mini-btn"
                          onClick={() => onAskAgent(`[关于启发式 "${h.body.slice(0, 30)}…"]: `)}
                        >
                          问 agent
                        </button>
                      )}
                      {h.effectiveState !== "retired" && (
                        <button
                          type="button"
                          className="agent-mini-btn danger"
                          title="停用后不再影响 agent 决策，记录保留"
                          onClick={() => handleRetire(h)}
                        >
                          停用
                        </button>
                      )}
                    </div>
                  </article>
                ))}
              </div>
            </div>
          );
          })}
        </>
      )}
    </section>
  );
}
