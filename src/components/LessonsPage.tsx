import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { BookOpen } from "lucide-react";
import { type ReactNode, useEffect, useMemo, useState } from "react";

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

const OUTCOME_BADGE: Record<Lesson["outcome"], "good" | "danger" | "warn"> = {
  hit: "good",
  miss: "danger",
  expired: "warn",
};

const OUTCOME_LABEL: Record<Lesson["outcome"], string> = {
  hit: "命中",
  miss: "未中",
  expired: "到期",
};

export function LessonsPage({ onAskAgent }: { onAskAgent?: (prefill: string) => void }) {
  const [list, setList] = useState<Lesson[]>([]);

  useEffect(() => {
    const load = () => void invoke<Lesson[]>("list_lessons", { limit: 200 }).then(setList);
    load();
    const unsub = listen("lessons-changed", load);
    return () => {
      void unsub.then((u) => u());
    };
  }, []);

  // 摘要——直接从列表派生，不额外请求。
  // emptyTakeaway > 0 是关键告警：reflection 写了 lesson 但 LLM takeaway fill
  // 没接上，emerge 链路下一轮拿不到这条数据。
  const summary = useMemo(() => {
    const sevenDaysAgo = Date.now() - 7 * 24 * 3600 * 1000;
    let hit = 0;
    let miss = 0;
    let expired = 0;
    let emptyTakeaway = 0;
    let recent = 0;
    for (const l of list) {
      if (l.outcome === "hit") hit++;
      else if (l.outcome === "miss") miss++;
      else if (l.outcome === "expired") expired++;
      if (!l.takeaway || !l.takeaway.trim()) emptyTakeaway++;
      if (l.createdAt >= sevenDaysAgo) recent++;
    }
    return { hit, miss, expired, emptyTakeaway, recent };
  }, [list]);

  return (
    <section className="page-shell agent-subpage">
      <header className="section-head">
        <div>
          <h2>复盘</h2>
          <p>每个 expectation 终态时自动生成的原子观察——学习闭环的底层原料，启发式从这里 emerge。</p>
        </div>
      </header>

      {list.length === 0 ? (
        <EmptyState
          icon={<BookOpen size={28} strokeWidth={1.4} />}
          title="还没有复盘记录"
          body="每个 expectation 到期或命中后，agent 会在 15:30 reflection 自动写一条 lesson 进来——内容是「这次预期为什么对/为什么错」的原子观察。"
          hint="先在「对话」里跟 agent 创建第一个 expectation，等到它进入终态就会有数据。"
        />
      ) : (
        <>
          <div className="agent-stats-strip">
            <Stat label="总数" value={list.length} />
            <Stat label="近 7 天" value={summary.recent} />
            <Stat label="命中" value={summary.hit} tone="good" />
            <Stat label="未中" value={summary.miss} tone="danger" />
            <Stat label="到期" value={summary.expired} tone="warn" />
            <Stat
              label="空 takeaway"
              value={summary.emptyTakeaway}
              tone={summary.emptyTakeaway > 0 ? "danger" : "muted"}
              hint={summary.emptyTakeaway > 0 ? "LLM 没接上——emerge 拿不到" : undefined}
            />
          </div>
          <div className="agent-card-list">
          {list.map((l) => (
            <article key={l.id} className="agent-card">
              <div className="agent-card-head">
                <span className={`agent-badge ${OUTCOME_BADGE[l.outcome]} dot`}>
                  {OUTCOME_LABEL[l.outcome]}
                </span>
                <span className="agent-card-title">{l.code}</span>
                {l.regimeAtClose && (
                  <span className="agent-badge neutral">regime · {l.regimeAtClose}</span>
                )}
                <span className="agent-card-meta">
                  {l.pnlPct != null && <span>{(l.pnlPct * 100).toFixed(2)}%</span>}
                  <span>{new Date(l.createdAt).toLocaleDateString("zh-CN")}</span>
                </span>
              </div>
              <div className="agent-card-body">{l.observation}</div>
              {l.takeaway && (
                <div className="agent-card-sub" style={{ fontStyle: "italic" }}>
                  takeaway · {l.takeaway}
                </div>
              )}
              {onAskAgent && (
                <div className="agent-card-actions">
                  <button
                    type="button"
                    className="agent-mini-btn"
                    onClick={() =>
                      onAskAgent(`[关于 lesson #${l.id.slice(0, 8)} (${l.code} ${l.outcome})]: `)
                    }
                  >
                    问 agent
                  </button>
                </div>
              )}
            </article>
          ))}
          </div>
        </>
      )}
    </section>
  );
}

// ============================================================================
// EmptyState——四个 agent 子页共用。当 list 为空时显示，告诉用户：
//   - 这个页面平常展示什么
//   - 数据从哪来（什么 trigger 后会自动出现）
//   - 下一步建议（去对话页等）
// 比"暂无 X"那种孤立提示有用得多。
// ============================================================================
export function EmptyState({
  icon,
  title,
  body,
  hint,
}: {
  icon?: ReactNode;
  title: string;
  body: string;
  hint?: string;
}) {
  return (
    <div className="agent-empty-state">
      {icon && <div className="agent-empty-icon">{icon}</div>}
      <h3>{title}</h3>
      <p>{body}</p>
      {hint && <p className="agent-empty-hint">{hint}</p>}
    </div>
  );
}

// ============================================================================
// 共享 Stat——单元素，不带容器。Strategies / Heuristics / Expectations 都引用。
// 容器（.agent-stats-strip）由各 page 自己包，决定分隔/留白。
// ============================================================================

export function Stat({
  label,
  value,
  tone,
  hint,
}: {
  label: string;
  value: number | string;
  tone?: "muted" | "good" | "danger" | "warn";
  hint?: string;
}) {
  return (
    <div className="agent-stat">
      <span className="agent-stat-label">{label}</span>
      <span className={`agent-stat-value${tone ? ` ${tone}` : ""}`}>{value}</span>
      {hint && <span className="agent-stat-hint">{hint}</span>}
    </div>
  );
}
