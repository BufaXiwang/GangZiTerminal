import { invoke } from "@tauri-apps/api/core";
import { useEffect, useMemo, useState } from "react";

// 与 adapters/thesis_commands.rs::ThesisDto 对齐
type ThesisDto = {
  id: string;
  hypothesis: string;
  invalidation: string;
  validationChecks: string[];
  conviction: "low" | "medium" | "high";
  state:
    | "drafted"
    | "active"
    | "validated"
    | "drifted"
    | "invalidated"
    | "abandoned";
  targetCodes: string[];
  regimeAtCreation: "bull" | "bear" | "choppy" | null;
  createdAt: number;
  updatedAt: number;
  closedAt: number | null;
};

type ThesisEventDto = {
  thesisId: string;
  event: Record<string, unknown> | string;
  occurredAt: number;
};

const STATE_LABELS: Record<ThesisDto["state"], string> = {
  drafted: "Drafted",
  active: "Active",
  validated: "Validated",
  drifted: "Drifted",
  invalidated: "Invalidated",
  abandoned: "Abandoned",
};

const STATE_COLORS: Record<ThesisDto["state"], string> = {
  drafted: "#9ca3af",
  active: "#10b981",
  validated: "#3b82f6",
  drifted: "#f59e0b",
  invalidated: "#ef4444",
  abandoned: "#6b7280",
};

function formatTime(ms: number): string {
  return new Date(ms).toLocaleString("zh-CN");
}

function ConvictionBadge({ c }: { c: ThesisDto["conviction"] }) {
  const color = c === "high" ? "#0ea5e9" : c === "medium" ? "#64748b" : "#94a3b8";
  return (
    <span
      style={{
        background: color,
        color: "white",
        padding: "2px 6px",
        borderRadius: 4,
        fontSize: 11,
      }}
    >
      {c}
    </span>
  );
}

function ThesisCard({
  thesis,
  onOpen,
}: {
  thesis: ThesisDto;
  onOpen: (t: ThesisDto) => void;
}) {
  return (
    <div
      onClick={() => onOpen(thesis)}
      style={{
        border: `1px solid ${STATE_COLORS[thesis.state]}`,
        borderRadius: 8,
        padding: 12,
        marginBottom: 8,
        background: "#fff",
        cursor: "pointer",
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 6 }}>
        <span
          style={{
            background: STATE_COLORS[thesis.state],
            color: "white",
            padding: "2px 8px",
            borderRadius: 4,
            fontSize: 11,
          }}
        >
          {STATE_LABELS[thesis.state]}
        </span>
        <ConvictionBadge c={thesis.conviction} />
        {thesis.targetCodes.length > 0 && (
          <span style={{ fontSize: 12, color: "#64748b" }}>
            {thesis.targetCodes.join(" / ")}
          </span>
        )}
        <span style={{ marginLeft: "auto", fontSize: 11, color: "#94a3b8" }}>
          {formatTime(thesis.updatedAt)}
        </span>
      </div>
      <div style={{ fontWeight: 600, marginBottom: 4 }}>{thesis.hypothesis}</div>
      <div style={{ fontSize: 12, color: "#64748b" }}>
        validation_checks: {thesis.validationChecks.length} 条
      </div>
    </div>
  );
}

function ThesisDetail({
  thesis,
  events,
  onClose,
  onAsk,
}: {
  thesis: ThesisDto;
  events: ThesisEventDto[];
  onClose: () => void;
  onAsk: (prefill: string) => void;
}) {
  const askPrefill = `[关于 thesis #${thesis.id} "${thesis.hypothesis.slice(0, 30)}"]: `;
  return (
    <div
      style={{
        position: "fixed",
        top: 0,
        right: 0,
        bottom: 0,
        width: "min(720px, 90vw)",
        background: "#fff",
        borderLeft: "1px solid #e5e7eb",
        boxShadow: "-4px 0 16px rgba(0,0,0,0.08)",
        overflowY: "auto",
        padding: 20,
        zIndex: 100,
      }}
    >
      <div style={{ display: "flex", alignItems: "center", marginBottom: 16 }}>
        <h2 style={{ margin: 0, flex: 1 }}>{thesis.hypothesis}</h2>
        <button onClick={onClose} style={{ padding: "4px 12px" }}>关闭</button>
      </div>
      <div style={{ display: "flex", gap: 8, marginBottom: 12 }}>
        <span
          style={{
            background: STATE_COLORS[thesis.state],
            color: "white",
            padding: "4px 10px",
            borderRadius: 4,
          }}
        >
          {STATE_LABELS[thesis.state]}
        </span>
        <ConvictionBadge c={thesis.conviction} />
        {thesis.regimeAtCreation && (
          <span style={{ fontSize: 12, color: "#64748b" }}>
            创建时 regime: {thesis.regimeAtCreation}
          </span>
        )}
      </div>

      <button
        onClick={() => onAsk(askPrefill)}
        style={{ marginBottom: 16, padding: "6px 12px" }}
      >
        💬 问 agent 关于这个 thesis
      </button>

      <section style={{ marginBottom: 16 }}>
        <h3>失效条件 (invalidation)</h3>
        <p style={{ background: "#fef3c7", padding: 10, borderRadius: 4 }}>
          {thesis.invalidation}
        </p>
      </section>

      <section style={{ marginBottom: 16 }}>
        <h3>验证清单 (validation_checks)</h3>
        {thesis.validationChecks.length === 0 ? (
          <p style={{ color: "#94a3b8" }}>（无）</p>
        ) : (
          <ol>
            {thesis.validationChecks.map((v, i) => (
              <li key={i}>{v}</li>
            ))}
          </ol>
        )}
      </section>

      <section>
        <h3>事件链</h3>
        <ul style={{ listStyle: "none", padding: 0 }}>
          {events.map((e, i) => (
            <li
              key={i}
              style={{
                padding: 8,
                marginBottom: 4,
                background: "#f9fafb",
                borderRadius: 4,
                fontSize: 13,
              }}
            >
              <div style={{ fontSize: 11, color: "#64748b" }}>
                {formatTime(e.occurredAt)}
              </div>
              <pre style={{ margin: 0, fontSize: 12, whiteSpace: "pre-wrap" }}>
                {typeof e.event === "string" ? e.event : JSON.stringify(e.event, null, 2)}
              </pre>
            </li>
          ))}
        </ul>
      </section>
    </div>
  );
}

export function ThesesPage({
  onAskAgent,
}: {
  onAskAgent?: (prefill: string) => void;
}) {
  const [theses, setTheses] = useState<ThesisDto[]>([]);
  const [selected, setSelected] = useState<ThesisDto | null>(null);
  const [events, setEvents] = useState<ThesisEventDto[]>([]);
  const [filter, setFilter] = useState<string>("open");

  const refresh = async (f: string = filter) => {
    const list = await invoke<ThesisDto[]>("list_theses", { filter: f, limit: 200 });
    setTheses(list);
  };

  useEffect(() => {
    void refresh(filter);
  }, [filter]);

  const openThesis = async (t: ThesisDto) => {
    setSelected(t);
    const evts = await invoke<ThesisEventDto[]>("list_thesis_events", {
      thesisId: t.id,
    });
    setEvents(evts);
  };

  const grouped = useMemo(() => {
    const m = new Map<ThesisDto["state"], ThesisDto[]>();
    for (const t of theses) {
      const arr = m.get(t.state) ?? [];
      arr.push(t);
      m.set(t.state, arr);
    }
    return m;
  }, [theses]);

  const order: ThesisDto["state"][] = [
    "active",
    "drafted",
    "drifted",
    "validated",
    "invalidated",
    "abandoned",
  ];

  return (
    <div style={{ padding: 20 }}>
      <h1>Theses（投资论点）</h1>
      <p style={{ color: "#64748b" }}>
        Agent 跟踪的所有投资论点。点卡片看详情 + 事件链。
      </p>

      <div style={{ marginBottom: 16 }}>
        <label>过滤：</label>
        {["open", "active", "drafted", "validated", "drifted", "invalidated", "abandoned"].map(
          (f) => (
            <button
              key={f}
              onClick={() => setFilter(f)}
              style={{
                marginRight: 6,
                padding: "4px 10px",
                background: filter === f ? "#3b82f6" : "transparent",
                color: filter === f ? "white" : "#64748b",
                border: "1px solid #e5e7eb",
                borderRadius: 4,
                cursor: "pointer",
              }}
            >
              {f}
            </button>
          ),
        )}
        <button onClick={() => void refresh()} style={{ marginLeft: 12 }}>
          刷新
        </button>
      </div>

      {theses.length === 0 && (
        <p style={{ color: "#94a3b8" }}>（暂无 thesis；agent 在 chat 里 create_thesis 后这里会出现）</p>
      )}

      {order.map((state) => {
        const items = grouped.get(state);
        if (!items || items.length === 0) return null;
        return (
          <section key={state} style={{ marginBottom: 24 }}>
            <h3 style={{ color: STATE_COLORS[state] }}>
              {STATE_LABELS[state]} ({items.length})
            </h3>
            {items.map((t) => (
              <ThesisCard key={t.id} thesis={t} onOpen={openThesis} />
            ))}
          </section>
        );
      })}

      {selected && (
        <ThesisDetail
          thesis={selected}
          events={events}
          onClose={() => setSelected(null)}
          onAsk={(prefill) => {
            onAskAgent?.(prefill);
            setSelected(null);
          }}
        />
      )}
    </div>
  );
}
