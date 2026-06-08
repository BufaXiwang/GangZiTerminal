// AgentPage — Agent Runtime 交互页（三栏：左 runs / 中 对话 / 右 策略 + 分析）。
//
// Spec: docs/design/agent-runtime-module.md §9 对外接口（前端：中间 chat + 右侧 AnalysisResult +
//        左侧 runs/报告 + 投资策略面板 + 熔断状态条）
//
// 数据流（前端不持有业务真源）：
//   - 命令：agentSendMessage（dialogue run）/ agentFetchState / agentFetchStrategy / agentUpsertStrategy
//   - 流式：listen("agent-event") → text_delta 增量进当前 assistant 气泡
//   - 状态推送：listen("agent-run-finished" / "agent-analysis-result") → 刷新总览
//
// 注：复盘报告列表 / 熔断状态条待后端 review fork + 熔断状态接线后补（见 runtime §WP2 余 / §WP3）。

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { Bot, Send, User } from "lucide-react";
import { PageShell } from "../components/PageShell";
import {
  commands,
  type AgentStateSnapshot,
  type InvestmentStrategy,
  type ReviewReportRef,
} from "../bindings";

interface ChatMsg {
  role: "user" | "assistant";
  text: string;
  streaming?: boolean;
  error?: boolean;
}

function shortId(id: string): string {
  return id.length > 10 ? `${id.slice(0, 6)}…${id.slice(-4)}` : id;
}

const MODE_LABEL: Record<string, string> = {
  dialogue: "对话",
  news: "资讯",
  account_trigger: "账户触发",
  review: "复盘",
};
const STATUS_LABEL: Record<string, string> = {
  running: "运行中",
  completed: "完成",
  failed: "失败",
  cancelled: "已取消",
};

export default function AgentPage() {
  const conversationId = useRef<string>(
    (globalThis.crypto?.randomUUID?.() ?? `conv_${Date.now()}`),
  );
  const [messages, setMessages] = useState<ChatMsg[]>([]);
  const [input, setInput] = useState("");
  const [sending, setSending] = useState(false);
  const [state, setState] = useState<AgentStateSnapshot | null>(null);
  const [strategy, setStrategy] = useState<InvestmentStrategy | null>(null);
  const [strategyDraft, setStrategyDraft] = useState("");
  const [editingStrategy, setEditingStrategy] = useState(false);
  const [savingStrategy, setSavingStrategy] = useState(false);
  const [reports, setReports] = useState<ReviewReportRef[]>([]);
  const [reviewing, setReviewing] = useState(false);
  const [newsAuto, setNewsAuto] = useState(false);
  const [togglingNewsAuto, setTogglingNewsAuto] = useState(false);
  const currentRunId = useRef<string | null>(null);
  const chatEndRef = useRef<HTMLDivElement | null>(null);

  const refreshState = useCallback(async () => {
    const res = await commands.agentFetchState({
      include: {
        strategy: true,
        runs: true,
        analysisResults: true,
        trades: null,
        messages: null,
        toolCalls: null,
        circuitBreaker: true,
      },
      limit: 50,
      offset: null,
    });
    if (res.status === "ok") setState(res.data);
    const sres = await commands.agentFetchStrategy(null);
    if (sres.status === "ok") {
      const active = sres.data.active ?? null;
      setStrategy(active);
      if (!editingStrategy) setStrategyDraft(active?.strategy ?? "");
    }
    const rres = await commands.agentListReviewReports(null);
    if (rres.status === "ok") setReports(rres.data);
    const nres = await commands.agentGetNewsAutoAnalysis();
    if (nres.status === "ok") setNewsAuto(nres.data);
  }, [editingStrategy]);

  // news 自动分析开关（spec §5：默认关闭；开启时后端回填最近窗口内 news 入 buffer）。
  const toggleNewsAuto = useCallback(async () => {
    if (togglingNewsAuto) return;
    setTogglingNewsAuto(true);
    const next = !newsAuto;
    const res = await commands.agentSetNewsAutoAnalysis(next);
    if (res.status === "ok") {
      setNewsAuto(res.data.enabled);
      if (next && res.data.backfilled > 0) {
        // 提示回填了多少条（可选）。
        console.info(`已回填最近 ${res.data.backfilled} 条资讯入分析队列`);
      }
    }
    setTogglingNewsAuto(false);
  }, [newsAuto, togglingNewsAuto]);

  const runReviewToday = useCallback(async () => {
    if (reviewing) return;
    setReviewing(true);
    const today = new Date();
    const yyyymmdd = `${today.getFullYear()}${String(today.getMonth() + 1).padStart(2, "0")}${String(today.getDate()).padStart(2, "0")}`;
    await commands.agentRunReview({ tradeDate: yyyymmdd });
    setReviewing(false);
    void refreshState();
  }, [reviewing, refreshState]);

  useEffect(() => {
    void refreshState();
  }, [refreshState]);

  // 流式 token → 当前 assistant 气泡；run 终态 / 分析产出 → 刷新总览。
  useEffect(() => {
    const uns: Array<() => void> = [];
    void listen<Record<string, unknown>>("agent-event", (e) => {
      const env = e.payload as Record<string, unknown>;
      const p = (env?.payload ?? env) as { type?: string; delta?: string };
      if (p?.type === "text_delta" && typeof p.delta === "string") {
        const delta = p.delta;
        setMessages((prev) => {
          const next = [...prev];
          const last = next[next.length - 1];
          if (last && last.role === "assistant" && last.streaming) {
            next[next.length - 1] = { ...last, text: last.text + delta };
          }
          return next;
        });
      }
    }).then((u) => uns.push(u));
    void listen<Record<string, unknown>>("agent-run-started", (e) => {
      const p = ((e.payload as Record<string, unknown>)?.payload ?? e.payload) as { runId?: string };
      if (p?.runId) currentRunId.current = p.runId;
    }).then((u) => uns.push(u));
    void listen("agent-run-finished", () => {
      currentRunId.current = null;
      void refreshState();
    }).then((u) => uns.push(u));
    void listen("agent-analysis-result", () => void refreshState()).then((u) => uns.push(u));
    void listen("agent-circuit-breaker", () => void refreshState()).then((u) => uns.push(u));
    // news age-out 丢弃计数（spec §5/§7）：提示用户丢了多少。
    void listen<Record<string, unknown>>("agent-news-buffer-dropped", (e) => {
      const p = ((e.payload as Record<string, unknown>)?.payload ?? e.payload) as { count?: number };
      if (p?.count) console.warn(`资讯分析队列丢弃 ${p.count} 条（超时未分析）`);
    }).then((u) => uns.push(u));
    return () => uns.forEach((u) => u());
  }, [refreshState]);

  const cancelCurrent = useCallback(async () => {
    const id = currentRunId.current;
    if (id) await commands.agentCancelRun({ runId: id, reason: "用户停止当前 run" });
  }, []);

  useEffect(() => {
    chatEndRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages]);

  const send = useCallback(async () => {
    const text = input.trim();
    if (!text || sending) return;
    setInput("");
    setSending(true);
    setMessages((prev) => [
      ...prev,
      { role: "user", text },
      { role: "assistant", text: "", streaming: true },
    ]);
    const res = await commands.agentSendMessage({
      content: text,
      images: null,
      conversationId: conversationId.current,
    });
    setMessages((prev) => {
      const next = [...prev];
      const last = next[next.length - 1];
      if (last && last.role === "assistant") {
        if (res.status === "error") {
          next[next.length - 1] = {
            ...last,
            streaming: false,
            error: true,
            text: last.text || `运行失败：${res.error.message ?? res.error.code}`,
          };
        } else {
          next[next.length - 1] = {
            ...last,
            streaming: false,
            text: last.text || "（已完成，无文本输出）",
          };
        }
      }
      return next;
    });
    setSending(false);
    void refreshState();
  }, [input, sending, refreshState]);

  const saveStrategy = useCallback(async () => {
    const text = strategyDraft.trim();
    if (!text || savingStrategy) return;
    setSavingStrategy(true);
    const res = await commands.agentUpsertStrategy({
      strategyId: null,
      strategy: text,
      baseVersion: strategy?.version ?? null,
      reason: "对话页手动更新",
      status: "active",
    });
    setSavingStrategy(false);
    if (res.status === "ok") {
      setEditingStrategy(false);
      void refreshState();
    } else {
      // 版本冲突等：保持编辑态，提示。
      alert(`策略保存失败：${res.error.message ?? res.error.code}`);
    }
  }, [strategyDraft, savingStrategy, strategy, refreshState]);

  const runningCount = useMemo(
    () => state?.recentRuns.filter((r) => r.status === "running").length ?? 0,
    [state],
  );
  const circuitBroken = state?.circuitBreakerActive ?? false;

  // 熔断只由系统自动触发；用户只能解除（spec §9 set_circuit_breaker resume=true）。
  const resumeCircuitBreaker = useCallback(async () => {
    const res = await commands.agentSetCircuitBreaker({
      resume: true,
      reason: "对话页手动解除熔断",
    });
    if (res.status === "ok") void refreshState();
  }, [refreshState]);

  return (
    <PageShell
      title="Agent"
      status={runningCount > 0 ? `${runningCount} 个 run 运行中` : "空闲"}
      statusTone={circuitBroken ? "error" : runningCount > 0 ? "loading" : "ok"}
    >
      <div className="agent-page">
      {circuitBroken ? (
        <div className="agent-cb-bar agent-cb-on">
          <span>⚠ 熔断激活：自动下单已降级为 no_action/建议。</span>
          <button className="agent-cb-btn" onClick={() => void resumeCircuitBreaker()}>
            解除熔断
          </button>
        </div>
      ) : (
        <div className="agent-cb-bar">
          <span>风控正常，自动下单已启用。</span>
        </div>
      )}
      <div className="agent-grid">
        {/* 左：复盘报告 + 最近 runs */}
        <aside className="agent-col agent-runs">
          <div className="agent-col-title-row">
            <h3 className="agent-col-title">复盘报告</h3>
            <button className="agent-link-btn" disabled={reviewing} onClick={() => void runReviewToday()}>
              {reviewing ? "复盘中…" : "复盘今日"}
            </button>
          </div>
          <div className="agent-reports-list">
            {reports.map((r) => (
              <div key={r.path} className="agent-report-file" title={r.path}>
                📄 {r.name}
              </div>
            ))}
            {reports.length === 0 && <div className="agent-empty">暂无复盘报告</div>}
          </div>
          <h3 className="agent-col-title">最近运行</h3>
          <div className="agent-runs-list">
            {(state?.recentRuns ?? []).map((r) => (
              <div key={r.runId} className={`agent-run-item status-${r.status}`}>
                <div className="agent-run-top">
                  <span className="agent-run-mode">{MODE_LABEL[r.mode] ?? r.mode}</span>
                  <span className={`agent-run-status status-${r.status}`}>
                    {STATUS_LABEL[r.status] ?? r.status}
                  </span>
                </div>
                <div className="agent-run-id">{shortId(r.runId)}</div>
              </div>
            ))}
            {(!state || state.recentRuns.length === 0) && (
              <div className="agent-empty">暂无运行记录</div>
            )}
          </div>
        </aside>

        {/* 中：对话 */}
        <section className="agent-col agent-chat">
          <div className="agent-chat-scroll">
            {messages.length === 0 && (
              <div className="agent-empty agent-chat-hint">
                和 Agent 对话：让它分析行情/资讯、检查持仓、复盘策略，或在你确认后下单（模拟盘）。
              </div>
            )}
            {messages.map((m, i) => (
              <div key={i} className={`agent-msg agent-msg-${m.role}${m.error ? " agent-msg-error" : ""}`}>
                <div className="agent-msg-avatar">
                  {m.role === "user" ? <User size={15} /> : <Bot size={15} />}
                </div>
                <div className="agent-msg-body">
                  {m.text || (m.streaming ? "思考中…" : "")}
                  {m.streaming && <span className="agent-cursor">▋</span>}
                </div>
              </div>
            ))}
            <div ref={chatEndRef} />
          </div>
          <div className="agent-input-row">
            <textarea
              className="agent-input"
              value={input}
              placeholder="输入消息，Enter 发送（Shift+Enter 换行）"
              rows={2}
              disabled={sending}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  void send();
                }
              }}
            />
            {sending ? (
              <button className="agent-send-btn agent-stop-btn" onClick={() => void cancelCurrent()} title="停止当前 run">
                ■
              </button>
            ) : (
              <button className="agent-send-btn" disabled={!input.trim()} onClick={() => void send()}>
                <Send size={16} />
              </button>
            )}
          </div>
        </section>

        {/* 右：策略 + 分析结果 */}
        <aside className="agent-col agent-side">
          <div className="agent-strategy">
            <div className="agent-col-title-row">
              <h3 className="agent-col-title">投资策略{strategy ? ` v${strategy.version}` : ""}</h3>
              {!editingStrategy && (
                <button className="agent-link-btn" onClick={() => setEditingStrategy(true)}>
                  编辑
                </button>
              )}
            </div>
            {editingStrategy ? (
              <>
                <textarea
                  className="agent-strategy-edit"
                  value={strategyDraft}
                  rows={6}
                  onChange={(e) => setStrategyDraft(e.target.value)}
                />
                <div className="agent-strategy-actions">
                  <button
                    className="agent-link-btn"
                    onClick={() => {
                      setEditingStrategy(false);
                      setStrategyDraft(strategy?.strategy ?? "");
                    }}
                  >
                    取消
                  </button>
                  <button className="agent-save-btn" disabled={savingStrategy} onClick={() => void saveStrategy()}>
                    保存新版本
                  </button>
                </div>
              </>
            ) : (
              <div className="agent-strategy-text">
                {strategy?.strategy ?? "（未设置 active 策略，自动下单默认禁用）"}
              </div>
            )}
          </div>

          <div className="agent-results">
            <div className="agent-col-title-row">
              <h3 className="agent-col-title">分析结果</h3>
              <button
                className="agent-link-btn"
                disabled={togglingNewsAuto}
                title="开启后，新资讯会自动进入分析队列（默认关闭）"
                onClick={() => void toggleNewsAuto()}
              >
                {newsAuto ? "自动分析：开" : "自动分析：关"}
              </button>
            </div>
            <div className="agent-results-list">
              {(state?.recentResults ?? []).map((a) => (
                <div key={a.resultId} className={`agent-result-item kind-${a.kind}`}>
                  <span className={`agent-result-kind kind-${a.kind}`}>
                    {a.kind === "action" ? "操作" : "观望"}
                  </span>
                  <span className="agent-result-summary">{a.summary}</span>
                </div>
              ))}
              {(!state || state.recentResults.length === 0) && (
                <div className="agent-empty">暂无分析结果</div>
              )}
            </div>
          </div>
        </aside>
      </div>
      </div>
    </PageShell>
  );
}
