// AgentPage — Agent Runtime 交互页（两栏：左 sidebar / 中 对话+详情）。
//
// Spec: docs/design/agent-runtime-module.md §9 对外接口（前端：中间 chat + 左侧 sidebar
//        含策略/复盘/runs/分析）
//
// 数据流（前端不持有业务真源）：
//   - 命令：agentSendMessage（dialogue run）/ agentFetchState / agentFetchStrategy / agentUpsertStrategy
//   - 流式：listen("agent-event") → rich blocks（text_delta / thinking_delta / tool_start / tool_end / usage / done）
//   - 状态推送：listen("agent-run-finished" / "agent-analysis-result") → 刷新总览

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { Link, useLocation } from "react-router-dom";
import { Bot, ChevronDown, ChevronRight, Loader, Send, User, X, Zap } from "lucide-react";
import { PageShell } from "../components/PageShell";
import { ROUTES } from "../lib/router";
import { renderMarkdown } from "../lib/simpleMarkdown";
import {
  commands,
  type AgentRun,
  type AgentStateSnapshot,
  type AnalysisResult,
  type InvestmentStrategy,
  type ProviderChannelView,
  type ReviewReportRef,
  type StrategyHistoryEntry,
} from "../bindings";

/* ---------- Rich chat message model ---------- */

type ChatBlock =
  | { type: "text"; text: string }
  | { type: "thinking"; text: string; collapsed: boolean }
  | {
      type: "tool_call";
      id: string;
      name: string;
      input: string;
      output?: string;
      isError?: boolean;
      durationMs?: number;
      status: "running" | "done";
    }
  | { type: "usage"; input: number; output: number; cacheRead?: number };

interface ChatMessage {
  id: string;
  role: "user" | "assistant" | "system";
  blocks: ChatBlock[];
  streaming?: boolean;
  error?: boolean;
  timestamp?: string;
}

/** Format token count: 1200 → "1.2k", 500 → "500" */
function fmtTokens(n: number): string {
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return String(n);
}

/** Truncate a string with an ellipsis if it exceeds maxLen */
function truncate(s: string, maxLen: number): string {
  if (s.length <= maxLen) return s;
  return s.slice(0, maxLen) + "...";
}

type DetailView =
  | { type: "analysis"; data: AnalysisResult }
  | { type: "report"; name: string; path: string }
  | null;

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

/* ---------- inline sub-components ---------- */

function AnalysisDetail({ data, runs }: { data: AnalysisResult; runs: AgentRun[] }) {
  const [codeNames, setCodeNames] = useState<Map<string, string>>(new Map());

  useEffect(() => {
    if (!data.relatedCodes?.length) return;
    // Fetch instrument names via fetchData (supports tsCodes filter).
    commands.fetchData({
      tsCodes: data.relatedCodes,
      include: { quote: false },
      limit: null,
    }).then(res => {
      if (res.status === "ok") {
        const map = new Map<string, string>();
        for (const item of res.data.items) {
          if (item.name) map.set(item.tsCode, item.name);
        }
        setCodeNames(map);
      }
    });
  }, [data.relatedCodes]);

  // Find related news IDs from the run's trigger (news_batch mode).
  const relatedRun = useMemo(
    () => runs.find(r => r.runId === data.runId),
    [runs, data.runId],
  );
  const newsIds = useMemo(() => {
    if (!relatedRun) return null;
    const trigger = relatedRun.trigger as Record<string, unknown>;
    if (trigger?.kind === "news_batch" && Array.isArray(trigger.news_ids)) {
      return trigger.news_ids as string[];
    }
    return null;
  }, [relatedRun]);

  return (
    <div className="agent-analysis-detail">
      <div className="detail-field">
        <span className="detail-label">判定</span>
        <span className={`detail-kind kind-${data.kind}`}>
          {data.kind === "action" ? "操作" : "观望"}
        </span>
      </div>
      <div className="detail-field">
        <span className="detail-label">摘要</span>
        <div className="md-content" dangerouslySetInnerHTML={{ __html: renderMarkdown(data.summary) }} />
      </div>
      {data.relatedCodes?.length > 0 && (
        <div className="detail-field">
          <span className="detail-label">相关标的</span>
          <div className="detail-codes">
            {data.relatedCodes.map((code: string) => (
              <Link key={code} to={ROUTES.market} className="detail-code-link">
                {codeNames.get(code) ? `${codeNames.get(code)} ${code}` : code}
              </Link>
            ))}
          </div>
        </div>
      )}
      {data.tradeIds?.length > 0 && (
        <div className="detail-field">
          <span className="detail-label">关联交易</span>
          <span className="tabular">{data.tradeIds.join(", ")}</span>
        </div>
      )}
      {/* Related news: show if trigger is news_batch */}
      {newsIds && newsIds.length > 0 && (
        <div className="detail-field">
          <span className="detail-label">相关新闻</span>
          <span className="muted" style={{ fontSize: 12 }}>
            {newsIds.length} 条资讯触发（暂未支持跳转详情）
          </span>
        </div>
      )}
      <div className="detail-field">
        <span className="detail-label">时间</span>
        <span>{data.createdAt}</span>
      </div>
    </div>
  );
}

function ReportDetail({ name, path }: { name: string; path: string }) {
  return (
    <div className="agent-report-detail">
      <div className="detail-field">
        <span className="detail-label">文件名</span>
        <span>{name}</span>
      </div>
      <div className="detail-field">
        <span className="detail-label">路径</span>
        <span className="tabular" style={{ fontSize: 12, wordBreak: "break-all" }}>{path}</span>
      </div>
      <div className="detail-field">
        <span className="detail-label muted" style={{ fontStyle: "italic", marginTop: 12 }}>
          复盘报告已落盘为 Markdown 文件，可在文件系统中查看完整内容。
        </span>
      </div>
    </div>
  );
}

function StrategyModal({
  strategy,
  history,
  onClose,
}: {
  strategy: InvestmentStrategy | null;
  history: StrategyHistoryEntry[] | null;
  onClose: () => void;
}) {
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  return (
    <div className="agent-modal-backdrop" onClick={onClose}>
      <div className="agent-modal" onClick={(e) => e.stopPropagation()}>
        <div className="agent-modal-header">
          <h2>投资策略{strategy ? ` V${strategy.version}` : ""}</h2>
          <span className="muted" style={{ fontSize: 12 }}>
            通过对话修改
          </span>
          <button className="agent-modal-close" onClick={onClose}>
            <X size={18} />
          </button>
        </div>
        <div className="agent-modal-body">
          <div className="md-content" dangerouslySetInnerHTML={{ __html: renderMarkdown(strategy?.strategy ?? "（未设置策略）") }} />
          {history && history.length > 0 && (
            <div className="agent-strategy-history">
              <h4 className="agent-strategy-history-title">版本历史</h4>
              {history.map((h) => (
                <div key={h.version} className="agent-strategy-history-item">
                  <span className="tabular">V{h.version}</span>
                  <span className="muted">{h.updatedAt}</span>
                  <span className="muted">{h.reason}</span>
                </div>
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

/* ---------- Chat block renderers ---------- */

function ChatBlockView({ block }: { block: ChatBlock }) {
  switch (block.type) {
    case "text":
      return <TextBlockView text={block.text} />;
    case "thinking":
      return <ThinkingBlockView text={block.text} defaultCollapsed={block.collapsed} />;
    case "tool_call":
      return (
        <ToolCallBlockView
          name={block.name}
          input={block.input}
          output={block.output}
          isError={block.isError}
          durationMs={block.durationMs}
          status={block.status}
        />
      );
    case "usage":
      return <UsageBlockView input={block.input} output={block.output} cacheRead={block.cacheRead} />;
    default:
      return null;
  }
}

function TextBlockView({ text }: { text: string }) {
  return (
    <div
      className="chat-block-text md-content"
      dangerouslySetInnerHTML={{ __html: renderMarkdown(text) }}
    />
  );
}

function ThinkingBlockView({
  text,
  defaultCollapsed,
}: {
  text: string;
  defaultCollapsed: boolean;
}) {
  const [collapsed, setCollapsed] = useState(defaultCollapsed);
  return (
    <div
      className="chat-block-thinking"
      onClick={() => setCollapsed(!collapsed)}
    >
      <div className="chat-block-thinking-header">
        {collapsed ? <ChevronRight size={14} /> : <ChevronDown size={14} />}
        <span>思考过程</span>
      </div>
      {!collapsed && (
        <div className="chat-block-thinking-text">{text}</div>
      )}
    </div>
  );
}

function ToolCallBlockView({
  name,
  input,
  output,
  isError,
  durationMs,
  status,
}: {
  name: string;
  input: string;
  output?: string;
  isError?: boolean;
  durationMs?: number;
  status: "running" | "done";
}) {
  const [expanded, setExpanded] = useState(false);
  return (
    <div className={`chat-block-tool${isError ? " is-error" : ""}`}>
      <div
        className="chat-block-tool-header"
        onClick={() => setExpanded(!expanded)}
      >
        {status === "running" ? (
          <Loader size={14} className="tool-spinner" />
        ) : (
          <Zap size={14} />
        )}
        <span className="tool-name">{name}</span>
        {status === "done" && durationMs != null && (
          <span className="tool-duration">{durationMs}ms</span>
        )}
        {status === "running" && (
          <span className="tool-duration">运行中...</span>
        )}
        <span className="tool-expand-hint">
          {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        </span>
      </div>
      {expanded && (
        <div className="chat-block-tool-body">
          <div className="tool-io-section">
            <span className="tool-io-label">Input</span>
            <pre>{truncate(input, 800)}</pre>
          </div>
          {output != null && (
            <div className="tool-io-section">
              <span className={`tool-io-label${isError ? " tool-io-error" : ""}`}>
                {isError ? "Error" : "Output"}
              </span>
              <pre>{truncate(output, 800)}</pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function UsageBlockView({
  input,
  output,
  cacheRead,
}: {
  input: number;
  output: number;
  cacheRead?: number;
}) {
  return (
    <div className="chat-block-usage">
      <Zap size={10} />
      {" "}
      {fmtTokens(input)} input · {fmtTokens(output)} output
      {cacheRead != null && cacheRead > 0 && (
        <> · {fmtTokens(cacheRead)} cache</>
      )}
    </div>
  );
}

/* ---------- main page ---------- */

export default function AgentPage() {
  const [hasChannel, setHasChannel] = useState<boolean | null>(null); // null = loading
  const [channels, setChannels] = useState<ProviderChannelView[]>([]);
  const conversationId = useRef<string>(
    globalThis.crypto?.randomUUID?.() ?? `conv_${Date.now()}`,
  );
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState("");
  const [sending, setSending] = useState(false);
  const [state, setState] = useState<AgentStateSnapshot | null>(null);
  const [strategy, setStrategy] = useState<InvestmentStrategy | null>(null);
  const [reports, setReports] = useState<ReviewReportRef[]>([]);
  const [reviewing, setReviewing] = useState(false);
  const [newsAuto, setNewsAuto] = useState(false);
  const [togglingNewsAuto, setTogglingNewsAuto] = useState(false);
  const [detailView, setDetailView] = useState<DetailView>(null);
  const [strategyModalOpen, setStrategyModalOpen] = useState(false);
  const [strategyHistory, setStrategyHistory] = useState<StrategyHistoryEntry[] | null>(null);
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
      },
      limit: 50,
      offset: null,
    });
    if (res.status === "ok") setState(res.data);
    const sres = await commands.agentFetchStrategy(null);
    if (sres.status === "ok") {
      const active = sres.data.active ?? null;
      setStrategy(active);
    }
    const rres = await commands.agentListReviewReports(null);
    if (rres.status === "ok") setReports(rres.data);
    const nres = await commands.agentGetNewsAutoAnalysis();
    if (nres.status === "ok") setNewsAuto(nres.data);
  }, []);

  // news 自动分析开关（spec §5：默认关闭；开启时后端回填最近窗口内 news 入 buffer）。
  const toggleNewsAuto = useCallback(async () => {
    if (togglingNewsAuto) return;
    setTogglingNewsAuto(true);
    const next = !newsAuto;
    const res = await commands.agentSetNewsAutoAnalysis(next);
    if (res.status === "ok") {
      setNewsAuto(res.data.enabled);
      if (next && res.data.backfilled > 0) {
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

  const { pathname } = useLocation();

  // 每次路由切到 Agent 页时加载渠道列表（keep-alive 不会重新 mount）。
  useEffect(() => {
    if (pathname === ROUTES.agent) {
      commands.agentListChannels().then(res => {
        if (res.status === "ok") {
          setChannels(res.data);
          setHasChannel(res.data.length > 0);
        }
      }).catch(() => setHasChannel(false));
    }
  }, [pathname]);

  useEffect(() => {
    void refreshState();
  }, [refreshState]);

  // Conversation persistence: load persisted messages on mount.
  useEffect(() => {
    const cid = conversationId.current;
    if (!cid) return;
    commands.agentLoadConversation(cid).then(res => {
      if (res.status === "ok" && res.data.length > 0) {
        const converted: ChatMessage[] = res.data
          .filter(m => m.role === "user" || m.role === "assistant")
          .map(m => ({
            id: m.messageId,
            role: m.role as "user" | "assistant",
            blocks: m.blocks
              .filter(b => b.type === "text" || b.type === "thinking")
              .map(b => {
                if (b.type === "thinking") return { type: "thinking" as const, text: b.text, collapsed: true };
                return { type: "text" as const, text: (b as { text: string }).text };
              }),
            timestamp: m.createdAt,
          }));
        if (converted.length > 0) setMessages(converted);
      }
    }).catch(() => { /* first conversation, no history */ });
  }, []);

  // Helper: immutable update of the last assistant (streaming) message's blocks.
  const updateCurrentMessage = useCallback(
    (updater: (blocks: ChatBlock[]) => ChatBlock[]) => {
      setMessages((prev) => {
        const next = [...prev];
        const last = next[next.length - 1];
        if (last && last.role === "assistant" && last.streaming) {
          const newBlocks = updater([...last.blocks]);
          next[next.length - 1] = { ...last, blocks: newBlocks };
        }
        return next;
      });
    },
    [],
  );

  // 流式 events → rich blocks；run 终态 / 分析产出 → 刷新总览。
  useEffect(() => {
    const uns: Array<() => void> = [];
    void listen<Record<string, unknown>>("agent-event", (e) => {
      const env = e.payload as Record<string, unknown>;
      const p = (env?.payload ?? env) as Record<string, unknown>;
      const type = p?.type as string | undefined;

      switch (type) {
        case "text_delta": {
          const delta = p.delta as string;
          updateCurrentMessage((blocks) => {
            const lastBlock = blocks[blocks.length - 1];
            if (lastBlock?.type === "text") {
              blocks[blocks.length - 1] = { ...lastBlock, text: lastBlock.text + delta };
            } else {
              blocks.push({ type: "text", text: delta });
            }
            return blocks;
          });
          break;
        }
        case "thinking_delta": {
          const delta = p.delta as string;
          updateCurrentMessage((blocks) => {
            const lastBlock = blocks[blocks.length - 1];
            if (lastBlock?.type === "thinking") {
              blocks[blocks.length - 1] = { ...lastBlock, text: lastBlock.text + delta };
            } else {
              blocks.push({ type: "thinking", text: delta, collapsed: true });
            }
            return blocks;
          });
          break;
        }
        case "tool_start": {
          const toolCallId = p.toolCallId as string;
          const name = p.name as string;
          const inputSummary = JSON.stringify(p.inputSummary ?? {});
          updateCurrentMessage((blocks) => {
            blocks.push({
              type: "tool_call",
              id: toolCallId,
              name,
              input: inputSummary,
              status: "running",
            });
            return blocks;
          });
          break;
        }
        case "tool_end": {
          const toolCallId = p.toolCallId as string;
          const outputSummary = JSON.stringify(p.outputSummary ?? {});
          const isError = p.isError as boolean;
          const durationMs = p.durationMs as number;
          updateCurrentMessage((blocks) => {
            return blocks.map((b) => {
              if (b.type === "tool_call" && b.id === toolCallId) {
                return {
                  ...b,
                  output: outputSummary,
                  isError,
                  durationMs,
                  status: "done" as const,
                };
              }
              return b;
            });
          });
          break;
        }
        case "usage": {
          updateCurrentMessage((blocks) => {
            blocks.push({
              type: "usage",
              input: (p.inputTokens as number) ?? 0,
              output: (p.outputTokens as number) ?? 0,
              cacheRead: (p.cacheReadTokens as number | undefined) ?? undefined,
            });
            return blocks;
          });
          break;
        }
        case "done": {
          setMessages((prev) => {
            const next = [...prev];
            const last = next[next.length - 1];
            if (last?.role === "assistant") {
              next[next.length - 1] = { ...last, streaming: false };
            }
            return next;
          });
          break;
        }
        default:
          break;
      }
    }).then((u) => uns.push(u));
    void listen<Record<string, unknown>>("agent-run-started", (e) => {
      const p = ((e.payload as Record<string, unknown>)?.payload ??
        e.payload) as { runId?: string };
      if (p?.runId) currentRunId.current = p.runId;
    }).then((u) => uns.push(u));
    void listen("agent-run-finished", () => {
      currentRunId.current = null;
      void refreshState();
    }).then((u) => uns.push(u));
    void listen("agent-analysis-result", () => void refreshState()).then((u) =>
      uns.push(u),
    );
    // news age-out 丢弃计数（spec §5/§7）：提示用户丢了多少。
    void listen<Record<string, unknown>>(
      "agent-news-buffer-dropped",
      (e) => {
        const p = ((e.payload as Record<string, unknown>)?.payload ??
          e.payload) as { count?: number };
        if (p?.count)
          console.warn(`资讯分析队列丢弃 ${p.count} 条（超时未分析）`);
      },
    ).then((u) => uns.push(u));
    return () => uns.forEach((u) => u());
  }, [refreshState, updateCurrentMessage]);

  const cancelCurrent = useCallback(async () => {
    const id = currentRunId.current;
    if (id)
      await commands.agentCancelRun({
        runId: id,
        reason: "用户停止当前 run",
      });
  }, []);

  useEffect(() => {
    chatEndRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages]);

  const send = useCallback(async () => {
    const text = input.trim();
    if (!text || sending) return;
    setInput("");
    setSending(true);
    // Switch back to chat when sending a new message
    setDetailView(null);

    const userMsg: ChatMessage = {
      id: crypto.randomUUID(),
      role: "user",
      blocks: [{ type: "text", text }],
      timestamp: new Date().toISOString(),
    };
    const assistantMsg: ChatMessage = {
      id: crypto.randomUUID(),
      role: "assistant",
      blocks: [],
      streaming: true,
    };
    setMessages((prev) => [...prev, userMsg, assistantMsg]);

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
          // If no text blocks were streamed, add an error text block
          const hasText = last.blocks.some((b) => b.type === "text" && b.text);
          const errorBlocks = hasText
            ? last.blocks
            : [
                ...last.blocks,
                { type: "text" as const, text: `运行失败：${res.error.message ?? res.error.code}` },
              ];
          next[next.length - 1] = {
            ...last,
            streaming: false,
            error: true,
            blocks: errorBlocks,
          };
        } else {
          const hasText = last.blocks.some((b) => b.type === "text" && b.text);
          const doneBlocks = hasText
            ? last.blocks
            : [...last.blocks, { type: "text" as const, text: "（已完成，无文本输出）" }];
          next[next.length - 1] = {
            ...last,
            streaming: false,
            blocks: doneBlocks,
          };
        }
      }
      return next;
    });
    setSending(false);
    void refreshState();
  }, [input, sending, refreshState]);

  const runningCount = useMemo(
    () =>
      state?.recentRuns?.filter((r) => r.status === "running").length ?? 0,
    [state],
  );

  // Build a unified timeline merging runs with analysis results (by runId).
  const timeline = useMemo(() => {
    const runs = state?.recentRuns ?? [];
    const results = state?.recentResults ?? [];
    const resultsByRun = new Map(results.map(r => [r.runId, r]));

    return runs.map(run => ({
      run,
      analysis: resultsByRun.get(run.runId) ?? null,
    }));
  }, [state]);

  // Toggle detail: clicking the same item closes it, clicking a different one switches.
  const toggleDetail = useCallback(
    (next: NonNullable<DetailView>) => {
      setDetailView((prev) => {
        if (prev === null) return next;
        if (prev.type === next.type) {
          if (
            prev.type === "analysis" &&
            next.type === "analysis" &&
            prev.data.resultId === next.data.resultId
          ) {
            return null;
          }
          if (
            prev.type === "report" &&
            next.type === "report" &&
            prev.path === next.path
          ) {
            return null;
          }
        }
        return next;
      });
    },
    [],
  );

  // Open strategy modal and fetch history
  const openStrategyModal = useCallback(async () => {
    setStrategyModalOpen(true);
    const res = await commands.agentFetchStrategy({ includeHistory: true });
    if (res.status === "ok") {
      if (res.data.active) setStrategy(res.data.active);
      setStrategyHistory(res.data.history ?? null);
    }
  }, []);

  const activeChannel = channels.find(c => c.isActive);

  const switchModel = useCallback(async (channelId: string) => {
    await commands.agentSetActiveChannel(channelId);
    const res = await commands.agentListChannels();
    if (res.status === "ok") setChannels(res.data);
  }, []);

  return (
    <PageShell
      title="Agent"
      status={
        hasChannel === false
          ? "未配置服务商"
          : runningCount > 0
            ? `${runningCount} 个 run 运行中`
            : "空闲"
      }
      statusTone={
        hasChannel === false
          ? "warn"
          : runningCount > 0
            ? "loading"
            : "ok"
      }
    >
      <div className="agent-page">
        {/* Left sidebar */}
        <aside className="agent-sidebar">
          {/* Strategy label */}
          <div
            className="agent-sidebar-strategy"
            onClick={() => void openStrategyModal()}
          >
            <span>投资策略{strategy ? ` V${strategy.version}` : ""}</span>
            <span className="agent-link-btn">查看</span>
          </div>

          {/* Model selector */}
          {channels.length > 0 && (
            <div className="agent-sidebar-model">
              <select
                value={activeChannel?.channelId ?? ""}
                onChange={(e) => void switchModel(e.target.value)}
              >
                {channels.map(ch => (
                  <option key={ch.channelId} value={ch.channelId}>
                    {ch.model} ({ch.provider})
                  </option>
                ))}
              </select>
            </div>
          )}

          {/* Review reports */}
          <div className="agent-sidebar-section">
            <div className="agent-sidebar-section-header">
              <h3>复盘报告</h3>
              <button
                className="agent-link-btn"
                disabled={reviewing}
                onClick={() => void runReviewToday()}
              >
                {reviewing ? "复盘中…" : "复盘今日"}
              </button>
            </div>
            {reports.map((r) => (
              <div
                key={r.path}
                className={`agent-sidebar-item${detailView?.type === "report" && (detailView as { type: "report"; path: string }).path === r.path ? " active" : ""}`}
                onClick={() =>
                  toggleDetail({ type: "report", name: r.name, path: r.path })
                }
              >
                {r.name}
              </div>
            ))}
            {reports.length === 0 && (
              <div className="agent-empty">暂无复盘报告</div>
            )}
          </div>

          {/* Unified run timeline (merges runs + analysis results) */}
          <div className="agent-sidebar-section">
            <div className="agent-sidebar-section-header">
              <h3>运行记录</h3>
              <button
                className="agent-link-btn"
                disabled={togglingNewsAuto}
                title="开启后，新资讯会自动进入分析队列（默认关闭）"
                onClick={() => void toggleNewsAuto()}
              >
                {newsAuto ? "自动分析：开" : "自动分析：关"}
              </button>
            </div>
            {timeline.map(({ run, analysis }) => (
              <div
                key={run.runId}
                className={`agent-sidebar-item agent-timeline-item${analysis && detailView?.type === "analysis" && (detailView as { type: "analysis"; data: AnalysisResult }).data.resultId === analysis.resultId ? " active" : ""}`}
                onClick={() => analysis && toggleDetail({ type: "analysis", data: analysis })}
                style={{ cursor: analysis ? "pointer" : "default" }}
              >
                <div className="agent-timeline-top">
                  <span className="agent-timeline-mode">{MODE_LABEL[run.mode] ?? run.mode}</span>
                  {analysis ? (
                    <span className={`agent-timeline-kind kind-${analysis.kind}`}>
                      {analysis.kind === "action" ? "操作" : "观望"}
                    </span>
                  ) : (
                    <span className={`agent-timeline-status status-${run.status}`}>
                      {STATUS_LABEL[run.status] ?? run.status}
                    </span>
                  )}
                </div>
                {analysis && (
                  <div className="agent-timeline-summary">{analysis.summary}</div>
                )}
              </div>
            ))}
            {timeline.length === 0 && (
              <div className="agent-empty">暂无运行记录</div>
            )}
          </div>
        </aside>

        {/* Center area */}
        <section className="agent-center">
          {detailView ? (
            <div className="agent-detail">
              <div className="agent-detail-header">
                <h3>
                  {detailView.type === "analysis" ? "分析详情" : "复盘报告"}
                </h3>
                <button
                  className="agent-detail-close"
                  onClick={() => setDetailView(null)}
                >
                  <X size={16} />
                </button>
              </div>
              <div className="agent-detail-body">
                {detailView.type === "analysis" ? (
                  <AnalysisDetail data={detailView.data} runs={state?.recentRuns ?? []} />
                ) : (
                  <ReportDetail
                    name={detailView.name}
                    path={detailView.path}
                  />
                )}
              </div>
            </div>
          ) : (
            <div className="agent-chat-scroll">
              {messages.length === 0 && (
                <div className="agent-empty agent-chat-hint">
                  和 Agent
                  对话：让它分析行情/资讯、检查持仓、复盘策略，或在你确认后下单（模拟盘）。
                </div>
              )}
              {messages.map((m) => (
                <div
                  key={m.id}
                  className={`agent-msg agent-msg-${m.role}${m.error ? " agent-msg-error" : ""}`}
                >
                  <div className="agent-msg-avatar">
                    {m.role === "user" ? (
                      <User size={15} />
                    ) : (
                      <Bot size={15} />
                    )}
                  </div>
                  <div className="agent-msg-body">
                    {m.role === "user" ? (
                      // User messages: simple text
                      <span>{m.blocks[0]?.type === "text" ? m.blocks[0].text : ""}</span>
                    ) : (
                      // Assistant messages: render rich blocks
                      <>
                        {m.blocks.length === 0 && m.streaming && (
                          <span className="muted">思考中...</span>
                        )}
                        {m.blocks.map((block, bi) => (
                          <ChatBlockView key={bi} block={block} />
                        ))}
                        {m.streaming && (
                          <span className="agent-cursor">▋</span>
                        )}
                      </>
                    )}
                  </div>
                </div>
              ))}
              <div ref={chatEndRef} />
            </div>
          )}
          <div className="agent-input-row">
            {hasChannel === false ? (
              <Link
                to={ROUTES.settings}
                className="agent-input agent-input-placeholder-link"
              >
                尚未配置 AI 服务商，点此前往设置
              </Link>
            ) : (
              <>
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
                  <button
                    className="agent-send-btn agent-stop-btn"
                    onClick={() => void cancelCurrent()}
                    title="停止当前 run"
                  >
                    ■
                  </button>
                ) : (
                  <button
                    className="agent-send-btn"
                    disabled={!input.trim()}
                    onClick={() => void send()}
                  >
                    <Send size={16} />
                  </button>
                )}
              </>
            )}
          </div>
        </section>
      </div>

      {/* Strategy modal */}
      {strategyModalOpen && (
        <StrategyModal
          strategy={strategy}
          history={strategyHistory}
          onClose={() => setStrategyModalOpen(false)}
        />
      )}
    </PageShell>
  );
}
