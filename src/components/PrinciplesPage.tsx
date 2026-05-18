import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";

type PrincipleDto = {
  id: string;
  body: string;
  category: "principle" | "known_bias" | "risk_preference";
  origin: "user_stated" | "agent_inferred";
  state: "proposed" | "active" | "dormant" | "retired";
  regimeTags: string[];
  hitCount: number;
  lastAppliedAt: number | null;
  createdAt: number;
};

type HealthMetrics = {
  thesisCompletenessRate: number | null;
  totalTheses: number;
  totalClosedTheses: number;
  reflectionEpisodeCount7d: number;
  principleStateCounts: {
    proposed: number;
    active: number;
    dormant: number;
    retired: number;
  };
  principleOriginShare: {
    userStated: number;
    agentInferred: number;
    agentInferredShare: number | null;
  };
};

const STATE_LABELS: Record<PrincipleDto["state"], string> = {
  proposed: "Proposed（待验证）",
  active: "Active（生效中）",
  dormant: "Dormant（沉睡）",
  retired: "Retired（已淘汰）",
};

const ORIGIN_ICON: Record<PrincipleDto["origin"], string> = {
  user_stated: "🧑",
  agent_inferred: "🤖",
};

function formatPct(v: number | null): string {
  return v === null ? "—" : `${(v * 100).toFixed(0)}%`;
}

function PrincipleCard({ p }: { p: PrincipleDto }) {
  return (
    <div
      style={{
        border: "1px solid #e5e7eb",
        borderRadius: 6,
        padding: 10,
        marginBottom: 6,
        background: p.state === "active" ? "#fff" : "#f9fafb",
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 4 }}>
        <span title={p.origin}>{ORIGIN_ICON[p.origin]}</span>
        <span
          style={{
            background: p.category === "known_bias" ? "#fef3c7" : "#dbeafe",
            color: "#1e293b",
            padding: "1px 6px",
            borderRadius: 3,
            fontSize: 11,
          }}
        >
          {p.category}
        </span>
        <span style={{ fontSize: 11, color: "#64748b" }}>
          hit_count: {p.hitCount}
        </span>
        {p.regimeTags.length > 0 && (
          <span style={{ fontSize: 11, color: "#64748b" }}>
            regime: {p.regimeTags.join(", ")}
          </span>
        )}
      </div>
      <div>{p.body}</div>
    </div>
  );
}

export function PrinciplesPage() {
  const [principles, setPrinciples] = useState<PrincipleDto[]>([]);
  const [health, setHealth] = useState<HealthMetrics | null>(null);

  useEffect(() => {
    void (async () => {
      const list = await invoke<PrincipleDto[]>("list_principles", { limit: 500 });
      setPrinciples(list);
      const m = await invoke<HealthMetrics>("get_health_metrics");
      setHealth(m);
    })();
  }, []);

  const byState = (s: PrincipleDto["state"]) =>
    principles.filter((p) => p.state === s);

  return (
    <div style={{ padding: 20 }}>
      <h1>Principles（投资原则）</h1>
      <p style={{ color: "#64748b" }}>
        Agent 学到的（或用户给的）投资原则 / 已知偏差 / 风险偏好。
        🧑 = 用户口头说的；🤖 = agent reflection 学到的。
      </p>

      {health && (
        <div
          style={{
            background: "#f0f9ff",
            border: "1px solid #bae6fd",
            borderRadius: 8,
            padding: 12,
            marginBottom: 20,
          }}
        >
          <h3 style={{ marginTop: 0 }}>Agent 机制健康度</h3>
          <div style={{ display: "grid", gridTemplateColumns: "repeat(3, 1fr)", gap: 8, fontSize: 13 }}>
            <div>
              Thesis 完整度: <strong>{formatPct(health.thesisCompletenessRate)}</strong>
              <div style={{ fontSize: 11, color: "#64748b" }}>
                共 {health.totalTheses} 条（{health.totalClosedTheses} 已闭环）
              </div>
            </div>
            <div>
              近 7 天 reflection: <strong>{health.reflectionEpisodeCount7d}</strong>
            </div>
            <div>
              agent_inferred 占比:{" "}
              <strong>{formatPct(health.principleOriginShare.agentInferredShare)}</strong>
              <div style={{ fontSize: 11, color: "#64748b" }}>
                🧑 {health.principleOriginShare.userStated} / 🤖 {health.principleOriginShare.agentInferred}
              </div>
            </div>
          </div>
        </div>
      )}

      {(["proposed", "active", "dormant", "retired"] as PrincipleDto["state"][]).map(
        (state) => {
          const items = byState(state);
          if (items.length === 0) return null;
          return (
            <section key={state} style={{ marginBottom: 20 }}>
              <h3>
                {STATE_LABELS[state]} ({items.length})
              </h3>
              {items
                .sort((a, b) => b.hitCount - a.hitCount)
                .map((p) => (
                  <PrincipleCard key={p.id} p={p} />
                ))}
            </section>
          );
        },
      )}

      {principles.length === 0 && (
        <p style={{ color: "#94a3b8" }}>（暂无 principle；启动时应该 seed 10 条）</p>
      )}
    </div>
  );
}
