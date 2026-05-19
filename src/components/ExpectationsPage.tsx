import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Target } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { EmptyState, Stat } from "./LessonsPage";

type Lesson = {
  id: string;
  expectationId: string;
  code: string;
  observation: string;
  takeaway: string;
  outcome: "hit" | "partial_hit" | "miss" | "expired";
  createdAt: number;
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
  state: "pending" | "hit" | "partial_hit" | "missed" | "expired" | "cancelled" | "superseded";
  referencePrice: number | null;
  regimeAtCreation: string | null;
  createdAt: number;
  expiresAt: number;
  closedAt: number | null;
};

type ExpectationSummary = {
  totalExpectations: number;
  totalClosedExpectations: number;
  expectationsCreatedToday: number;
  expectationCompletenessRate: number | null;
};

const STATE_LABEL: Record<Expectation["state"], string> = {
  pending: "进行中",
  hit: "命中",
  partial_hit: "部分命中",
  missed: "未中",
  expired: "到期",
  cancelled: "已撤",
  superseded: "已替换",
};

const STATE_BADGE: Record<Expectation["state"], "brand" | "good" | "danger" | "warn" | "neutral"> = {
  pending: "brand",
  hit: "good",
  partial_hit: "warn",
  missed: "danger",
  expired: "warn",
  cancelled: "neutral",
  superseded: "neutral",
};

const LESSON_BADGE: Record<Lesson["outcome"], "good" | "danger" | "warn"> = {
  hit: "good",
  partial_hit: "warn",
  miss: "danger",
  expired: "warn",
};

const LESSON_LABEL: Record<Lesson["outcome"], string> = {
  hit: "命中",
  partial_hit: "部分命中",
  miss: "未中",
  expired: "到期",
};

const FILTER_OPTIONS: Array<{ id: Expectation["state"]; label: string }> = [
  { id: "pending", label: "进行中" },
  { id: "hit", label: "命中" },
  { id: "partial_hit", label: "部分命中" },
  { id: "missed", label: "未中" },
  { id: "expired", label: "到期" },
  { id: "cancelled", label: "已撤" },
  { id: "superseded", label: "已替换" },
];

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
      void invoke<Lesson[]>("list_lessons_for_expectation", { expectationId: exp.id })
        .then((res) => setLessons(res))
        .catch(() => setLessons([]))
        .finally(() => setLessonsLoading(false));
    }
  };

  return (
    <article id={`exp-${exp.id}`} className={`agent-card${terminal ? " dim" : ""}`}>
      <div className="agent-card-head">
        <span className={`agent-badge ${STATE_BADGE[exp.state]} dot`}>{STATE_LABEL[exp.state]}</span>
        <span className="agent-card-title">{exp.code}</span>
        <span className="agent-card-sub">
          {exp.direction} · {exp.conviction} · {exp.horizonDays}d
        </span>
        {exp.theme && <span className="agent-badge brand">#{exp.theme}</span>}
        <span className="agent-card-meta">
          <span>建仓 {formatTime(exp.createdAt)}</span>
          <span>到期 {formatTime(exp.expiresAt)}</span>
        </span>
      </div>
      <div className="agent-card-body">
        目标 ·{" "}
        <strong style={{ color: "var(--fg-strong)" }}>
          {exp.targetPrice ?? "（观察型）"}
          {exp.targetPriceCeiling && ` ~ ${exp.targetPriceCeiling}`}
        </strong>
        <br />
        {exp.reasoning}
      </div>
      <div className="agent-card-sub">
        信号 · {exp.signalsUsed.map((s) => s.kind).join("，") || "（无）"}
      </div>
      {exp.supersedes && (
        <div className="agent-card-sub">
          替换了 ·{" "}
          <button
            type="button"
            className="agent-mini-btn"
            style={{ fontFamily: "var(--font-mono)" }}
            onClick={() => onJumpToSupersedes(exp.supersedes!)}
            title="跳转到被替换的 expectation"
          >
            #{exp.supersedes.slice(0, 8)} ↗
          </button>
        </div>
      )}
      <div className="agent-card-actions">
        <button type="button" className="agent-mini-btn" onClick={() => onAsk(askPrefill)}>
          问 agent
        </button>
        {terminal && (
          <button type="button" className="agent-mini-btn" onClick={toggleLessons}>
            {showLessons ? "收起 lessons" : "查看 lessons"}
          </button>
        )}
      </div>
      {terminal && showLessons && (
        <div
          style={{
            marginTop: 4,
            padding: 12,
            background: "var(--bg-card)",
            border: "1px solid var(--border-soft)",
            borderRadius: "var(--radius-sm)",
            display: "flex",
            flexDirection: "column",
            gap: 8,
          }}
        >
          {lessonsLoading && <div className="agent-card-sub">加载中…</div>}
          {!lessonsLoading && lessons !== null && lessons.length === 0 && (
            <div className="agent-card-sub">没有关联 lesson</div>
          )}
          {!lessonsLoading &&
            lessons?.map((l) => (
              <div key={l.id} style={{ borderLeft: "2px solid var(--border-default)", paddingLeft: 10 }}>
                <div className="agent-card-head" style={{ gap: 6, marginBottom: 2 }}>
                  <span className={`agent-badge ${LESSON_BADGE[l.outcome]}`}>
                    {LESSON_LABEL[l.outcome]}
                  </span>
                  <span className="agent-card-sub">{l.observation}</span>
                </div>
                {l.takeaway && (
                  <div className="agent-card-sub" style={{ fontStyle: "italic" }}>
                    takeaway · {l.takeaway}
                  </div>
                )}
              </div>
            ))}
        </div>
      )}
    </article>
  );
}

export function ExpectationsPage({ onAskAgent }: { onAskAgent?: (msg: string) => void }) {
  const [list, setList] = useState<Expectation[]>([]);
  const [filter, setFilter] = useState<Expectation["state"]>("pending");
  const [themeFilter, setThemeFilter] = useState<string>("");
  const [summary, setSummary] = useState<ExpectationSummary | null>(null);

  const load = async (s: string) => {
    const result = await invoke<Expectation[]>("list_expectations", { state: s, limit: 200 });
    setList(result);
  };

  const refreshSummary = () => {
    void invoke<ExpectationSummary>("get_agent_health")
      .then((h) => setSummary(h))
      .catch(() => setSummary(null));
  };

  useEffect(() => {
    void load(filter);
    refreshSummary();
    const unsubscribePromise = listen("expectations-changed", () => {
      void load(filter);
      refreshSummary();
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

  const filtered = themeFilter ? list.filter((e) => e.theme === themeFilter) : list;

  const jumpToSupersedes = async (id: string) => {
    // 若被替换条不在当前过滤集合里，先把过滤切到 superseded，再滚动
    if (!list.some((e) => e.id === id)) {
      const all = await invoke<Expectation[]>("list_expectations", {
        state: "superseded",
        limit: 200,
      }).catch(() => [] as Expectation[]);
      setList(all);
      setFilter("superseded");
    }
    requestAnimationFrame(() => {
      const el = document.getElementById(`exp-${id}`);
      if (el) {
        el.scrollIntoView({ behavior: "smooth", block: "center" });
        el.style.outline = "2px solid var(--brand)";
        setTimeout(() => {
          el.style.outline = "";
        }, 1500);
      }
    });
  };

  return (
    <section className="page-shell agent-subpage">
      <header className="section-head">
        <div>
          <h2>预期</h2>
          <p>Agent 当前跟踪的投资预期——可量化、可代码自动验证。</p>
        </div>
      </header>

      {summary && summary.totalExpectations === 0 ? (
        <EmptyState
          icon={<Target size={28} strokeWidth={1.4} />}
          title="还没有投资预期"
          body="预期是 agent 跟踪的「这只票在 N 天内会涨到 X / 跌到 Y」一类可验证判断。每个预期到期或命中后会自动生成一条 lesson，沉淀成可复用的启发式。"
          hint="去「对话」让 agent 帮你建第一个 expectation——也可以让它扫盘后自己 propose。"
        />
      ) : (
        <>
          {summary && (
            <div className="agent-stats-strip">
              <Stat label="累计" value={summary.totalExpectations} />
              <Stat label="已关闭" value={summary.totalClosedExpectations} />
              <Stat
                label="今日新建"
                value={summary.expectationsCreatedToday}
                tone={summary.expectationsCreatedToday === 0 ? "muted" : undefined}
              />
              <Stat
                label="完整度"
                value={
                  summary.expectationCompletenessRate != null
                    ? `${Math.round(summary.expectationCompletenessRate * 100)}%`
                    : "—"
                }
                hint="同时含 signals + target + reasoning"
              />
              <Stat label="当前显示" value={filtered.length} />
            </div>
          )}

          <div className="agent-filter-row">
            {FILTER_OPTIONS.map((opt) => (
              <button
                key={opt.id}
                type="button"
                onClick={() => setFilter(opt.id)}
                className={`agent-chip${filter === opt.id ? " active" : ""}`}
              >
                {opt.label}
              </button>
            ))}
          </div>

          {themes.length > 0 && (
            <div className="agent-filter-row">
              <button
                type="button"
                onClick={() => setThemeFilter("")}
                className={`agent-chip${themeFilter === "" ? " active" : ""}`}
              >
                所有 theme
              </button>
              {themes.map((t) => (
                <button
                  key={t}
                  type="button"
                  onClick={() => setThemeFilter(t)}
                  className={`agent-chip${themeFilter === t ? " active" : ""}`}
                >
                  #{t}
                </button>
              ))}
            </div>
          )}

          {filtered.length === 0 ? (
            <p className="agent-empty-state" style={{ background: "transparent", border: 0 }}>
              当前筛选下没有 expectation，换个 state 试试。
            </p>
          ) : (
            <div className="agent-card-list">
              {filtered.map((e) => (
                <ExpectationCard
                  key={e.id}
                  exp={e}
                  onAsk={(msg) => onAskAgent?.(msg)}
                  onJumpToSupersedes={(id) => void jumpToSupersedes(id)}
                />
              ))}
            </div>
          )}
        </>
      )}
    </section>
  );
}
