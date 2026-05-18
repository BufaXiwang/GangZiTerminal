import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";

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

export function StrategiesPage() {
  const [list, setList] = useState<Strategy[]>([]);

  useEffect(() => {
    void invoke<Strategy[]>("list_strategies").then(setList);
  }, []);

  return (
    <div style={{ padding: 20 }}>
      <h1>Strategies</h1>
      <p style={{ color: "#64748b" }}>
        用户 + agent 共建的"什么时候建 expectation"规则集。chat 跟 agent 说话可修改。
      </p>

      {list.map((s) => {
        const total = s.hitCount + s.missCount;
        const conf = total >= 3 ? `${((s.hitCount / total) * 100).toFixed(0)}%` : "样本不足";
        return (
          <div
            key={s.id}
            style={{
              border: "1px solid #e5e7eb",
              borderRadius: 8,
              padding: 12,
              marginBottom: 8,
              background: s.enabled ? "#fff" : "#f9fafb",
              opacity: s.enabled ? 1 : 0.6,
            }}
          >
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <strong>{s.name}</strong>
              <span
                style={{
                  background: s.enabled ? "#10b981" : "#9ca3af",
                  color: "white",
                  padding: "2px 6px",
                  borderRadius: 3,
                  fontSize: 11,
                }}
              >
                {s.enabled ? "enabled" : "disabled"}
              </span>
              <span style={{ marginLeft: "auto", fontSize: 12, color: "#64748b" }}>
                applied={s.appliedCount} | hit/miss={s.hitCount}/{s.missCount} | conf={conf}
              </span>
            </div>
            <div style={{ marginTop: 4, fontSize: 13, color: "#475569" }}>{s.description}</div>
            <div style={{ marginTop: 6, fontSize: 11, color: "#64748b" }}>
              触发信号：{s.triggerWhen.map((t) => t.signal.kind).join(" + ")}
            </div>
            <div style={{ fontSize: 11, color: "#64748b" }}>
              target: {s.target.direction} {s.target.pctRelativeToCurrent}% / {s.target.horizonDays}d
            </div>
          </div>
        );
      })}
      {list.length === 0 && (
        <p style={{ color: "#94a3b8" }}>暂无 strategy（启动时应该 seed 3 条）</p>
      )}
    </div>
  );
}
