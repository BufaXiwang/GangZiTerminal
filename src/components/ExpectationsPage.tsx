import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useMemo, useState } from "react";

type Lesson = {
  id: string;
  expectationId: string;
  code: string;
  observation: string;
  takeaway: string;
  outcome: "hit" | "miss" | "expired";
  createdAt: number;
};

const OUTCOME_COLOR: Record<Lesson["outcome"], string> = {
  hit: "#10b981",
  miss: "#ef4444",
  expired: "#f59e0b",
};

type Expectation = {
  id: string;
  code: string;
  direction: "up" | "down" | "range_bound";
  targetPrice: number | null;
  targetPriceCeiling: number | null;
  horizonDays: number;
  reasoning: string;
  signalsUsed: Array<{ kind: string; [k: string]: unknown }>;
  conviction: "low" | "medium" | "high";
  theme: string | null;
  supersedes: string | null;
  state: "pending" | "hit" | "missed" | "expired" | "cancelled" | "superseded";
  regimeAtCreation: string | null;
  createdAt: number;
  expiresAt: number;
  closedAt: number | null;
};

const STATE_LABELS: Record<Expectation["state"], string> = {
  pending: "Pending（进行中）",
  hit: "Hit（命中）",
  missed: "Missed（未达 target）",
  expired: "Expired（到期未达）",
  cancelled: "Cancelled（主动撤）",
  superseded: "Superseded（被替换）",
};

const STATE_COLORS: Record<Expectation["state"], string> = {
  pending: "#3b82f6",
  hit: "#10b981",
  missed: "#ef4444",
  expired: "#f59e0b",
  cancelled: "#6b7280",
  superseded: "#9ca3af",
};

function formatTime(ms: number): string {
  return new Date(ms).toLocaleString("zh-CN");
}

function ExpectationCard({
  exp,
  onAsk,
  onJumpToSupersedes,
}: {
  exp: Expectation;
  onAsk: (msg: string) => void;
  onJumpToSupersedes: (id: string) => void;
}) {
  const askPrefill = `[关于 expectation ${exp.id} (${exp.code} ${exp.direction})]: `;
  const terminal = exp.state !== "pending";
  const [showLessons, setShowLessons] = useState(false);
  const [lessons, setLessons] = useState<Lesson[] | null>(null);
  const [lessonsLoading, setLessonsLoading] = useState(false);

  const toggleLessons = () => {
    if (showLessons) {
      setShowLessons(false);
      return;
    }
    setShowLessons(true);
    if (lessons === null && !lessonsLoading) {
      setLessonsLoading(true);
      void invoke<Lesson[]>("list_lessons_for_expectation", {
        expectationId: exp.id,
      })
        .then((res) => setLessons(res))
        .catch(() => setLessons([]))
        .finally(() => setLessonsLoading(false));
    }
  };
  return (
    <div
      id={`exp-${exp.id}`}
      style={{
        border: `1px solid ${STATE_COLORS[exp.state]}`,
        borderRadius: 8,
        padding: 12,
        marginBottom: 8,
        background: "#fff",
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 6 }}>
        <span
          style={{
            background: STATE_COLORS[exp.state],
            color: "white",
            padding: "2px 8px",
            borderRadius: 4,
            fontSize: 11,
          }}
        >
          {STATE_LABELS[exp.state]}
        </span>
        <span style={{ fontSize: 12, color: "#64748b" }}>
          {exp.code} · {exp.direction} · {exp.conviction} · {exp.horizonDays}d
        </span>
        {exp.theme && (
          <span style={{ fontSize: 11, color: "#0ea5e9", background: "#dbeafe", padding: "1px 6px", borderRadius: 3 }}>
            #{exp.theme}
          </span>
        )}
        <span style={{ marginLeft: "auto", fontSize: 11, color: "#94a3b8" }}>
          {formatTime(exp.createdAt)} · 到期 {formatTime(exp.expiresAt)}
        </span>
      </div>
      <div style={{ marginBottom: 4 }}>
        Target: <strong>{exp.targetPrice ?? "（观察型）"}</strong>
        {exp.targetPriceCeiling && ` ~ ${exp.targetPriceCeiling}`}
      </div>
      <div style={{ fontSize: 13, color: "#475569" }}>{exp.reasoning}</div>
      <div style={{ fontSize: 11, color: "#64748b", marginTop: 4 }}>
        signals: {exp.signalsUsed.map((s) => s.kind).join(", ") || "（无）"}
      </div>
      {exp.supersedes && (
        <div style={{ fontSize: 11, marginTop: 4 }}>
          replaces:{" "}
          <button
            onClick={() => onJumpToSupersedes(exp.supersedes!)}
            style={{
              padding: "1px 6px",
              fontSize: 11,
              background: "transparent",
              border: "1px dashed #94a3b8",
              color: "#0ea5e9",
              cursor: "pointer",
              fontFamily: "monospace",
            }}
            title="跳转到被替换的 expectation"
          >
            #{exp.supersedes.slice(0, 8)} ↗
          </button>
        </div>
      )}
      <div style={{ marginTop: 6, display: "flex", gap: 6 }}>
        <button
          onClick={() => onAsk(askPrefill)}
          style={{ padding: "3px 8px", fontSize: 11 }}
        >
          💬 问 agent
        </button>
        {terminal && (
          <button
            onClick={toggleLessons}
            style={{ padding: "3px 8px", fontSize: 11 }}
          >
            {showLessons ? "▾ 收起 lessons" : "▸ 查看 lessons"}
          </button>
        )}
      </div>
      {terminal && showLessons && (
        <div
          style={{
            marginTop: 6,
            padding: 8,
            background: "#f8fafc",
            border: "1px solid #e5e7eb",
            borderRadius: 4,
            fontSize: 12,
          }}
        >
          {lessonsLoading && <div style={{ color: "#94a3b8" }}>加载中…</div>}
          {!lessonsLoading && lessons !== null && lessons.length === 0 && (
            <div style={{ color: "#94a3b8" }}>没有关联 lesson</div>
          )}
          {!lessonsLoading &&
            lessons?.map((l) => (
              <div
                key={l.id}
                style={{
                  borderLeft: `3px solid ${OUTCOME_COLOR[l.outcome]}`,
                  paddingLeft: 8,
                  marginBottom: 6,
                }}
              >
                <div>
                  <span
                    style={{
                      background: OUTCOME_COLOR[l.outcome],
                      color: "white",
                      padding: "0 6px",
                      borderRadius: 3,
                      fontSize: 10,
                      marginRight: 6,
                    }}
                  >
                    {l.outcome}
                  </span>
                  {l.observation}
                </div>
                {l.takeaway && (
                  <div style={{ marginTop: 2, fontStyle: "italic", color: "#475569" }}>
                    💡 {l.takeaway}
                  </div>
                )}
              </div>
            ))}
        </div>
      )}
    </div>
  );
}

export function ExpectationsPage({ onAskAgent }: { onAskAgent?: (msg: string) => void }) {
  const [list, setList] = useState<Expectation[]>([]);
  const [filter, setFilter] = useState<string>("pending");
  const [themeFilter, setThemeFilter] = useState<string>("");

  const load = async (s: string) => {
    const result = await invoke<Expectation[]>("list_expectations", {
      state: s,
      limit: 200,
    });
    setList(result);
  };

  useEffect(() => {
    void load(filter);
    // 监听后端 expectations-changed 事件 → 自动 refetch
    const unsubscribePromise = listen("expectations-changed", () => {
      void load(filter);
    });
    return () => {
      void unsubscribePromise.then((unlisten) => unlisten());
    };
  }, [filter]);

  const themes = useMemo(() => {
    const set = new Set<string>();
    list.forEach((e) => e.theme && set.add(e.theme));
    return Array.from(set);
  }, [list]);

  const filtered = themeFilter
    ? list.filter((e) => e.theme === themeFilter)
    : list;

  const jumpToSupersedes = async (id: string) => {
    // 若被替换条不在当前过滤集合里，先把过滤切换到 superseded，再滚动
    if (!list.some((e) => e.id === id)) {
      const all = await invoke<Expectation[]>("list_expectations", {
        state: "superseded",
        limit: 200,
      }).catch(() => [] as Expectation[]);
      setList(all);
      setFilter("superseded");
    }
    // 等下一帧渲染完再 scroll
    requestAnimationFrame(() => {
      const el = document.getElementById(`exp-${id}`);
      if (el) {
        el.scrollIntoView({ behavior: "smooth", block: "center" });
        el.style.outline = "2px solid #0ea5e9";
        setTimeout(() => {
          el.style.outline = "";
        }, 1500);
      }
    });
  };

  return (
    <div style={{ padding: 20 }}>
      <h1>Expectations</h1>
      <p style={{ color: "#64748b" }}>
        Agent 当前跟踪的投资预期——可量化、可代码自动验证。
      </p>

      <div style={{ marginBottom: 16 }}>
        {["pending", "hit", "missed", "expired", "cancelled", "superseded"].map((s) => (
          <button
            key={s}
            onClick={() => setFilter(s)}
            style={{
              marginRight: 6,
              padding: "4px 10px",
              background: filter === s ? "#3b82f6" : "transparent",
              color: filter === s ? "white" : "#64748b",
              border: "1px solid #e5e7eb",
              borderRadius: 4,
              cursor: "pointer",
            }}
          >
            {s}
          </button>
        ))}
      </div>

      {themes.length > 0 && (
        <div style={{ marginBottom: 12, fontSize: 12 }}>
          theme 过滤：
          <button
            onClick={() => setThemeFilter("")}
            style={{ marginLeft: 6, padding: "2px 6px", fontSize: 11 }}
          >
            全部
          </button>
          {themes.map((t) => (
            <button
              key={t}
              onClick={() => setThemeFilter(t)}
              style={{
                marginLeft: 4,
                padding: "2px 6px",
                fontSize: 11,
                background: themeFilter === t ? "#dbeafe" : "transparent",
              }}
            >
              #{t}
            </button>
          ))}
        </div>
      )}

      {filtered.length === 0 ? (
        <p style={{ color: "#94a3b8" }}>暂无 expectation</p>
      ) : (
        filtered.map((e) => (
          <ExpectationCard
            key={e.id}
            exp={e}
            onAsk={(msg) => onAskAgent?.(msg)}
            onJumpToSupersedes={(id) => void jumpToSupersedes(id)}
          />
        ))
      )}
    </div>
  );
}
