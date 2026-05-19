import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useState } from "react";

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

type Counts = {
  seed: number;
  userStated: number;
  agentInferred: number;
  retired: number;
};

const STATE_COLORS: Record<Heuristic["effectiveState"], string> = {
  active: "#10b981",
  challenged: "#f59e0b",
  probationary: "#0ea5e9",
  dormant: "#9ca3af",
  retired: "#6b7280",
};

const ORIGIN_ICON: Record<Heuristic["origin"], string> = {
  seed: "📚",
  user_stated: "🧑",
  agent_inferred: "🤖",
};

export function HeuristicsPage({
  onAskAgent,
}: {
  onAskAgent?: (prefill: string) => void;
}) {
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
      `Retire heuristic "${h.body.slice(0, 60)}…"\n请输入退役原因（>=4 字）：`,
      "",
    );
    if (!reason || reason.trim().length < 4) return;
    void invoke("retire_heuristic_cmd", {
      heuristicId: h.id,
      reason: reason.trim(),
    }).catch((err) => window.alert(`retire 失败：${err}`));
  };

  const byState = (s: Heuristic["effectiveState"]) =>
    list.filter((h) => h.effectiveState === s);

  return (
    <div style={{ padding: 20 }}>
      <h1>Heuristics</h1>
      <p style={{ color: "#64748b" }}>
        Agent 学到的（或用户给的）启发式规则——带 confidence track record。
        📚 = seed / 🧑 = 用户说的 / 🤖 = agent reflection 学到的。
      </p>

      {counts && (
        <div
          style={{
            background: "#f0f9ff",
            border: "1px solid #bae6fd",
            borderRadius: 8,
            padding: 12,
            marginBottom: 20,
            display: "grid",
            gridTemplateColumns: "repeat(4, 1fr)",
            gap: 8,
            fontSize: 13,
          }}
        >
          <div>📚 Seed: <strong>{counts.seed}</strong></div>
          <div>🧑 User: <strong>{counts.userStated}</strong></div>
          <div>🤖 Agent: <strong>{counts.agentInferred}</strong></div>
          <div style={{ color: "#9ca3af" }}>退役: {counts.retired}</div>
        </div>
      )}

      {(["active", "challenged", "probationary", "dormant", "retired"] as Heuristic["effectiveState"][]).map(
        (s) => {
          const items = byState(s);
          if (items.length === 0) return null;
          return (
            <section key={s} style={{ marginBottom: 20 }}>
              <h3 style={{ color: STATE_COLORS[s] }}>
                {s} ({items.length})
              </h3>
              {items.map((h) => (
                <div
                  key={h.id}
                  style={{
                    border: "1px solid #e5e7eb",
                    borderRadius: 6,
                    padding: 10,
                    marginBottom: 6,
                    background: h.effectiveState === "active" ? "#fff" : "#f9fafb",
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                    <span title={h.origin}>{ORIGIN_ICON[h.origin]}</span>
                    <span
                      style={{
                        background: h.category === "known_bias" ? "#fef3c7" : "#dbeafe",
                        padding: "1px 6px",
                        borderRadius: 3,
                        fontSize: 11,
                      }}
                    >
                      {h.category}
                    </span>
                    <span style={{ fontSize: 11, color: "#64748b" }}>
                      hit/miss={h.hitCount}/{h.missCount}
                      {h.confidence !== null && ` (${(h.confidence * 100).toFixed(0)}%)`}
                    </span>
                    {h.regimeTags.length > 0 && (
                      <span style={{ fontSize: 11, color: "#94a3b8" }}>
                        regime: {h.regimeTags.join(",")}
                      </span>
                    )}
                  </div>
                  <div style={{ marginTop: 4 }}>{h.body}</div>
                  {h.supportingLessonIds.length > 0 && (
                    <div style={{ fontSize: 11, color: "#94a3b8", marginTop: 4 }}>
                      支持的 lessons: {h.supportingLessonIds.length} 条
                    </div>
                  )}
                  <div style={{ marginTop: 6, display: "flex", gap: 6 }}>
                    {onAskAgent && (
                      <button
                        onClick={() =>
                          onAskAgent(
                            `[关于 heuristic "${h.body.slice(0, 30)}..."]: `,
                          )
                        }
                        style={{ padding: "3px 8px", fontSize: 11 }}
                      >
                        💬 问 agent
                      </button>
                    )}
                    {h.effectiveState !== "retired" && (
                      <button
                        onClick={() => handleRetire(h)}
                        style={{
                          padding: "3px 8px",
                          fontSize: 11,
                          color: "#b91c1c",
                          borderColor: "#fecaca",
                        }}
                      >
                        ⏏ retire
                      </button>
                    )}
                  </div>
                </div>
              ))}
            </section>
          );
        },
      )}

      {list.length === 0 && (
        <p style={{ color: "#94a3b8" }}>暂无 heuristic（启动时应该 seed 10 条）</p>
      )}
    </div>
  );
}
