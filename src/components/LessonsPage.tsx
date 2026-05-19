import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useState } from "react";

type Lesson = {
  id: string;
  expectationId: string;
  code: string;
  observation: string;
  takeaway: string;
  outcome: "hit" | "miss" | "expired";
  regimeAtClose: string | null;
  signalsInPlay: Array<{ kind: string }>;
  pnlPct: number | null;
  createdAt: number;
};

const OUTCOME_COLORS: Record<Lesson["outcome"], string> = {
  hit: "#10b981",
  miss: "#ef4444",
  expired: "#f59e0b",
};

export function LessonsPage({
  onAskAgent,
}: {
  onAskAgent?: (prefill: string) => void;
}) {
  const [list, setList] = useState<Lesson[]>([]);

  useEffect(() => {
    const load = () => void invoke<Lesson[]>("list_lessons", { limit: 200 }).then(setList);
    load();
    const unsub = listen("lessons-changed", load);
    return () => {
      void unsub.then((u) => u());
    };
  }, []);

  return (
    <div style={{ padding: 20 }}>
      <h1>Lessons</h1>
      <p style={{ color: "#64748b" }}>
        每个 expectation 终态时自动生成的原子观察——学习闭环的底层原料。
        Heuristics 从这些 lessons 中 emerge。
      </p>

      {list.map((l) => (
        <div
          key={l.id}
          style={{
            border: `1px solid ${OUTCOME_COLORS[l.outcome]}`,
            borderRadius: 6,
            padding: 10,
            marginBottom: 6,
            background: "#fff",
          }}
        >
          <div style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 4 }}>
            <span
              style={{
                background: OUTCOME_COLORS[l.outcome],
                color: "white",
                padding: "1px 6px",
                borderRadius: 3,
                fontSize: 11,
              }}
            >
              {l.outcome}
            </span>
            <span style={{ fontSize: 12, color: "#64748b" }}>{l.code}</span>
            {l.regimeAtClose && (
              <span style={{ fontSize: 11, color: "#94a3b8" }}>regime={l.regimeAtClose}</span>
            )}
            <span style={{ marginLeft: "auto", fontSize: 11, color: "#94a3b8" }}>
              {new Date(l.createdAt).toLocaleDateString("zh-CN")}
            </span>
          </div>
          <div style={{ fontSize: 13, color: "#1e293b" }}>{l.observation}</div>
          {l.takeaway && (
            <div style={{ fontSize: 13, color: "#475569", marginTop: 4, fontStyle: "italic" }}>
              💡 {l.takeaway}
            </div>
          )}
          {onAskAgent && (
            <button
              onClick={() =>
                onAskAgent(
                  `[关于 lesson #${l.id.slice(0, 8)} (${l.code} ${l.outcome})]: `,
                )
              }
              style={{ marginTop: 6, padding: "3px 8px", fontSize: 11 }}
            >
              💬 问 agent
            </button>
          )}
        </div>
      ))}
      {list.length === 0 && (
        <p style={{ color: "#94a3b8" }}>暂无 lesson（每次 reflection tick 后自动累积）</p>
      )}
    </div>
  );
}
