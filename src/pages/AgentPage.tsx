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
import { Bot, ChevronDown, ChevronRight, ImagePlus, Loader, Send, User, X, Zap } from "lucide-react";
import { PageShell } from "../components/PageShell";
import { KlineModal } from "../components/KlineModal";
import { ROUTES } from "../lib/router";
import { renderMarkdown } from "../lib/simpleMarkdown";
import {
  commands,
  type AgentRun,
  type AgentStateSnapshot,
  type AnalysisResult,
  type ConversationSummary,
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
  | { type: "usage"; input: number; output: number; cacheRead?: number }
  | {
      // 子 agent 实时活动（fork 子 run 跑时让用户看到它在干嘛；仅展示，不进 LLM 上下文）。
      type: "subagent";
      agentId: string;
      tools: Array<{ name: string; done: boolean }>;
      text: string;
      done: boolean;
    };

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

/** Format ISO timestamp to local timezone */
function fmtTime(iso: string): string {
  try {
    return new Date(iso).toLocaleString("zh-CN", {
      year: "numeric", month: "2-digit", day: "2-digit",
      hour: "2-digit", minute: "2-digit", second: "2-digit",
      hour12: false,
    });
  } catch { return iso; }
}

/** Format ISO timestamp to shorter sidebar format */
function fmtTimeShort(iso: string): string {
  try {
    const d = new Date(iso);
    const now = new Date();
    if (d.toDateString() === now.toDateString()) {
      return d.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false });
    }
    return d.toLocaleDateString("zh-CN", { month: "2-digit", day: "2-digit" });
  } catch { return iso; }
}

/** Extract a clean one-line title from markdown summary */
function summarizeLine(summary: string): string {
  // Strip markdown headers/bold, take first meaningful line
  const clean = summary
    .replace(/^#{1,4}\s+/gm, "")
    .replace(/\*\*/g, "")
    .trim();
  const firstLine = clean.split("\n").find((l) => l.trim().length > 0) ?? clean;
  return firstLine.length > 60 ? firstLine.slice(0, 60) + "…" : firstLine;
}

/** Parse persisted message text into ChatBlocks (text + tool_call from XML markers) */
function parsePersistedBlocks(text: string, role: "user" | "assistant"): ChatBlock[] {
  const blocks: ChatBlock[] = [];
  // For user messages containing only tool results, parse them as tool_call blocks
  if (role === "user") {
    const resultRe = /<tool_(?:result|error)\s+name="([^"]*)"[^>]*call_id="([^"]*)"[^>]*>([\s\S]*?)<\/tool_(?:result|error)>/g;
    let match;
    while ((match = resultRe.exec(text)) !== null) {
      const isError = match[0].startsWith("<tool_error");
      blocks.push({
        type: "tool_call",
        id: match[2],
        name: match[1],
        input: "",
        output: match[3].slice(0, 500),
        isError,
        status: "done",
      });
    }
    // If we parsed tool results, skip adding raw text
    if (blocks.length > 0) return blocks;
  }
  // For assistant messages, parse <use_tool> as tool calls
  if (role === "assistant") {
    const toolRe = /<use_tool\s+name="([^"]*)">([\s\S]*?)<\/use_tool>/g;
    let lastIdx = 0;
    let match;
    while ((match = toolRe.exec(text)) !== null) {
      const before = text.slice(lastIdx, match.index).trim();
      if (before) blocks.push({ type: "text", text: before });
      blocks.push({
        type: "tool_call",
        id: `persisted_${match.index}`,
        name: match[1],
        input: match[2].slice(0, 500),
        status: "done",
      });
      lastIdx = match.index + match[0].length;
    }
    const after = text.slice(lastIdx).trim();
    if (after) blocks.push({ type: "text", text: after });
    if (blocks.length > 0) return blocks;
  }
  // Fallback: plain text
  const cleaned = text
    .replace(/<use_tool[\s\S]*?<\/use_tool>/g, "")
    .replace(/<tool_result[\s\S]*?<\/tool_result>/g, "")
    .replace(/<tool_error[\s\S]*?<\/tool_error>/g, "")
    .trim();
  if (cleaned) blocks.push({ type: "text", text: cleaned });
  return blocks;
}

/** Convert persisted AgentMessages into merged ChatMessages.
 * Tool result messages (user role with <tool_result>) get merged into the preceding
 * assistant message as tool_call blocks with output filled in. */
function mergePersistedMessages(msgs: import("../bindings").AgentMessage[]): ChatMessage[] {
  const out: ChatMessage[] = [];
  for (const m of msgs) {
    if (m.role !== "user" && m.role !== "assistant") continue;
    const blocks: ChatBlock[] = m.blocks.flatMap(b => {
      if (b.type === "thinking") return [{ type: "thinking" as const, text: b.text, collapsed: true }];
      if (b.type === "text") return parsePersistedBlocks((b as {text:string}).text, m.role as "user"|"assistant");
      return [];
    });
    if (blocks.length === 0) continue;
    // User messages with only tool_call blocks (tool results) → merge into last assistant msg
    const allToolCalls = blocks.every(b => b.type === "tool_call");
    if (m.role === "user" && allToolCalls && out.length > 0 && out[out.length - 1].role === "assistant") {
      // Merge tool results into preceding assistant's tool_call blocks
      const lastAssistant = out[out.length - 1];
      for (const block of blocks) {
        if (block.type !== "tool_call") continue;
        // 历史回填：assistant 的 <use_tool> 不带 call_id（id=persisted_X），<tool_result> 带真实
        // call_id，两者 id 不匹配。改按 **name + 顺序** 配对：填第一个同名、尚无 output 的 tool_call。
        const existing = lastAssistant.blocks.find(
          b => b.type === "tool_call" && b.name === block.name && b.output === undefined
        );
        if (existing && existing.type === "tool_call") {
          existing.output = block.output;
          existing.isError = block.isError;
          existing.status = "done";
        } else {
          lastAssistant.blocks.push(block);
        }
      }
      continue;
    }
    out.push({
      id: m.messageId,
      role: m.role as "user" | "assistant",
      blocks,
      timestamp: m.createdAt,
    });
  }
  return out;
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

/* ---------- inline sub-components ---------- */

function AnalysisDetail({ data, runs }: { data: AnalysisResult; runs: AgentRun[] }) {
  const [codeInfo, setCodeInfo] = useState<Map<string, { name: string; pct?: number }>>(new Map());
  const [klineCode, setKlineCode] = useState<{ tsCode: string; name?: string } | null>(null);
  const [newsItems, setNewsItems] = useState<Array<{id: string; title: string; source?: string}>>([]);

  useEffect(() => {
    if (!data.relatedCodes?.length) return;
    commands.fetchData({
      tsCodes: data.relatedCodes,
      include: { quote: true },
      limit: null,
    }).then(res => {
      if (res.status === "ok") {
        const map = new Map<string, { name: string; pct?: number }>();
        for (const item of res.data.items) {
          map.set(item.tsCode, {
            name: item.name ?? item.tsCode,
            pct: item.quote?.changePercent ?? undefined,
          });
        }
        setCodeInfo(map);
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
    if (trigger?.kind === "news_batch" && Array.isArray(trigger.newsIds)) {
      return trigger.newsIds as string[];
    }
    return null;
  }, [relatedRun]);

  // Fetch actual news items when newsIds are available.
  useEffect(() => {
    if (!newsIds?.length) {
      setNewsItems([]);
      return;
    }
    commands.fetchNews({
      ids: newsIds,
      limit: newsIds.length,
      query: null,
      sources: null,
      publishedFrom: null,
      publishedTo: null,
      includeArticle: null,
      offset: null,
      order: null,
    }).then(res => {
      if (res.status === "ok") {
        setNewsItems(res.data.items.map(n => ({
          id: n.id,
          title: n.title ?? n.id,
          source: n.source,
        })));
      }
    });
  }, [newsIds]);

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
            {data.relatedCodes.map((code: string) => {
              const info = codeInfo.get(code);
              const pct = info?.pct;
              const pctClass = pct != null ? (pct > 0 ? "up" : pct < 0 ? "down" : "flat") : "";
              return (
                <button
                  key={code}
                  type="button"
                  className="detail-code-link"
                  onClick={() => setKlineCode({ tsCode: code, name: info?.name })}
                >
                  <span>{info?.name ?? ""} {code}</span>
                  {pct != null && (
                    <span className={`detail-code-pct ${pctClass}`}>
                      {pct > 0 ? "+" : ""}{pct.toFixed(2)}%
                    </span>
                  )}
                </button>
              );
            })}
          </div>
        </div>
      )}
      <KlineModal
        open={!!klineCode}
        tsCode={klineCode?.tsCode ?? null}
        name={klineCode?.name}
        onClose={() => setKlineCode(null)}
      />
      {data.tradeIds?.length > 0 && (
        <div className="detail-field">
          <span className="detail-label">关联交易</span>
          <span className="tabular">{data.tradeIds.join(", ")}</span>
        </div>
      )}
      {/* Related news: show titles fetched from backend */}
      {newsItems.length > 0 && (
        <div className="detail-field">
          <span className="detail-label">触发新闻（{newsItems.length} 条）</span>
          <div className="detail-news-list">
            {newsItems.map(n => (
              <div key={n.id} className="detail-news-item">
                {n.source && <span className="detail-news-source">{n.source}</span>}
                <span className="detail-news-title">{n.title}</span>
              </div>
            ))}
          </div>
        </div>
      )}
      <div className="detail-field">
        <span className="detail-label">时间</span>
        <span>{fmtTime(data.createdAt)}</span>
      </div>
    </div>
  );
}

function ReportDetail({ name, path }: { name: string; path: string }) {
  const [content, setContent] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  useEffect(() => {
    commands.agentReadReviewReport(path).then((res) => {
      if (res.status === "ok") {
        setContent(res.data);
      } else {
        setLoadError(res.error.message ?? "读取失败");
      }
    });
  }, [path]);

  if (loadError) {
    return (
      <div className="agent-report-detail">
        <p className="muted">{name} — {loadError}</p>
      </div>
    );
  }
  if (content === null) {
    return (
      <div className="agent-report-detail">
        <p className="muted">加载中…</p>
      </div>
    );
  }
  // Strip the run_id line from report markdown before rendering.
  const cleaned = content.replace(/^-\s*run_id:.*$/m, "").trim();
  return (
    <div className="agent-report-detail">
      <div className="md-content" dangerouslySetInnerHTML={{ __html: renderMarkdown(cleaned) }} />
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
      // todo_write 渲染成 live checklist（从 output/input 的 {items} 派生，复用 ToolEnd 信道）。
      if (block.name === "todo_write") {
        return <TodoBlockView input={block.input} output={block.output} />;
      }
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
    case "subagent":
      return (
        <SubAgentBlockView agentId={block.agentId} tools={block.tools} text={block.text} done={block.done} />
      );
    default:
      return null;
  }
}

// 子 agent 实时活动嵌套面板（fork 子 run 阻塞跑时，用户看到它在调什么工具 / 输出什么）。
function SubAgentBlockView({
  tools,
  text,
  done,
}: {
  agentId: string;
  tools: Array<{ name: string; done: boolean }>;
  text: string;
  done: boolean;
}) {
  const [collapsed, setCollapsed] = useState(false);
  return (
    <div
      style={{
        border: "1px solid var(--border, #1e293b)",
        borderLeft: "2px solid #a78bfa",
        borderRadius: 8,
        padding: "6px 10px",
        margin: "6px 0",
        background: "var(--bg-raised, rgba(167,139,250,0.05))",
        fontSize: 12,
      }}
    >
      <div
        style={{ display: "flex", gap: 6, alignItems: "center", cursor: "pointer", color: "#a78bfa" }}
        onClick={() => setCollapsed((v) => !v)}
      >
        {collapsed ? <ChevronRight size={13} /> : <ChevronDown size={13} />}
        <span>🤖 子 agent {done ? "· 已完成" : "· 调研中…"}</span>
      </div>
      {!collapsed && (
        <div style={{ marginLeft: 18, marginTop: 4 }}>
          {tools.map((t, i) => (
            <div key={i} style={{ color: t.done ? "var(--text-dim,#94a3b8)" : "#fbbf24", fontFamily: "ui-monospace, monospace" }}>
              {t.done ? "■" : "▸"} {t.name}
            </div>
          ))}
          {text && (
            <div style={{ color: "#cbd5e1", marginTop: 4, whiteSpace: "pre-wrap" }}>{text}</div>
          )}
        </div>
      )}
    </div>
  );
}

// todo_write 的 live checklist。从 output（执行后回显）或 input（执行中预览）解析 {items}。
function TodoBlockView({ input, output }: { input: string; output?: string }) {
  let items: Array<{ content: string; status: string }> = [];
  try {
    const src = output && output !== "{}" ? output : input;
    const parsed = JSON.parse(src);
    if (Array.isArray(parsed?.items)) items = parsed.items;
  } catch {
    /* malformed → 不渲染 */
  }
  if (items.length === 0) return null;
  const done = items.filter((i) => i.status === "completed").length;
  const mark = (s: string) => (s === "completed" ? "✔" : s === "in_progress" ? "▸" : "○");
  const color = (s: string) =>
    s === "completed" ? "#34d399" : s === "in_progress" ? "#fbbf24" : "#94a3b8";
  return (
    <div
      style={{
        border: "1px solid var(--border, #1e293b)",
        borderRadius: 8,
        padding: "8px 12px",
        margin: "6px 0",
        background: "var(--bg-raised, rgba(255,255,255,0.02))",
        fontSize: 13,
      }}
    >
      <div style={{ color: "var(--text-dim, #94a3b8)", fontSize: 12, marginBottom: 6 }}>
        📋 任务清单 · {done}/{items.length}
      </div>
      {items.map((it, i) => (
        <div key={i} style={{ display: "flex", gap: 8, alignItems: "baseline", padding: "1px 0" }}>
          <span style={{ color: color(it.status), width: 14, flexShrink: 0 }}>{mark(it.status)}</span>
          <span
            style={{
              color: it.status === "completed" ? "var(--text-dim, #94a3b8)" : "var(--text, #cbd5e1)",
              textDecoration: it.status === "completed" ? "line-through" : "none",
              fontWeight: it.status === "in_progress" ? 600 : 400,
            }}
          >
            {it.content}
          </span>
        </div>
      ))}
    </div>
  );
}

function TextBlockView({ text }: { text: string }) {
  const cleaned = text
    .replace(/<use_tool[\s\S]*?<\/use_tool>/g, "")
    .replace(/<tool_result[\s\S]*?<\/tool_result>/g, "")
    .replace(/<tool_error[\s\S]*?<\/tool_error>/g, "")
    .trim();
  if (!cleaned) return null;
  return (
    <div
      className="chat-block-text md-content"
      dangerouslySetInnerHTML={{ __html: renderMarkdown(cleaned) }}
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
  const [conversationIdValue] = useState<string>(() => {
    const stored = localStorage.getItem("agent_conversation_id");
    if (stored) return stored;
    const id = globalThis.crypto?.randomUUID?.() ?? `conv_${Date.now()}`;
    localStorage.setItem("agent_conversation_id", id);
    return id;
  });
  const conversationId = useRef(conversationIdValue);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState("");
  const [pendingImages, setPendingImages] = useState<string[]>([]);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const [sending, setSending] = useState(false);
  // 非阻塞 pending 消息队列（CC 式）：run 进行中提交的消息排队、显示在对话区，
  // 当前 run 结束后自动逐条发出。
  type QueuedMsg = { id: string; text: string; images: string[] };
  const [queued, setQueued] = useState<QueuedMsg[]>([]);
  const queuedRef = useRef<QueuedMsg[]>([]);
  queuedRef.current = queued;
  const runActiveRef = useRef<string | null>(null); // 当前在跑的 assistant 消息 id（null=空闲）
  const startRunRef = useRef<((text: string, images: string[]) => Promise<void>) | null>(null);
  const finishRunRef = useRef<((assistantId: string) => void) | null>(null);
  const [state, setState] = useState<AgentStateSnapshot | null>(null);
  const [strategy, setStrategy] = useState<InvestmentStrategy | null>(null);
  const [reports, setReports] = useState<ReviewReportRef[]>([]);
  const [reviewing, setReviewing] = useState(false);
  const [newsAuto, setNewsAuto] = useState(false);
  const [togglingNewsAuto, setTogglingNewsAuto] = useState(false);
  const [detailView, setDetailView] = useState<DetailView>(null);
  const [strategyModalOpen, setStrategyModalOpen] = useState(false);
  const [strategyHistory, setStrategyHistory] = useState<StrategyHistoryEntry[] | null>(null);
  const [conversations, setConversations] = useState<ConversationSummary[]>([]);
  const currentRunId = useRef<string | null>(null);
  const chatEndRef = useRef<HTMLDivElement | null>(null);
  const chatScrollRef = useRef<HTMLDivElement | null>(null);

  // Load conversation list from backend.
  const loadConversations = useCallback(async () => {
    const res = await commands.agentListConversations();
    if (res.status === "ok") setConversations(res.data);
  }, []);

  // Create a new conversation.
  const newConversation = useCallback(() => {
    const id = crypto.randomUUID();
    localStorage.setItem("agent_conversation_id", id);
    conversationId.current = id;
    setMessages([]);
    void loadConversations();
  }, [loadConversations]);

  // Switch to an existing conversation.
  const switchConversation = useCallback((cid: string) => {
    localStorage.setItem("agent_conversation_id", cid);
    conversationId.current = cid;
    setMessages([]);
    commands.agentLoadConversation(cid).then(res => {
      if (res.status === "ok" && res.data.length > 0) {
        const converted = mergePersistedMessages(res.data);
        setMessages(converted);
      }
    });
  }, []);

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
    void loadConversations();
  }, [refreshState, loadConversations]);

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
            blocks: m.blocks.flatMap(b => {
              if (b.type === "thinking") return [{ type: "thinking" as const, text: b.text, collapsed: true }];
              if (b.type === "text") return parsePersistedBlocks((b as {text:string}).text, m.role as "user"|"assistant");
              return [];
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
  // Refs to break closure dependency — avoids re-subscribing on every render.
  const updateCurrentMessageRef = useRef(updateCurrentMessage);
  updateCurrentMessageRef.current = updateCurrentMessage;
  const refreshStateRef = useRef(refreshState);
  refreshStateRef.current = refreshState;

  useEffect(() => {
    let stale = false;
    const uns: Array<() => void> = [];
    void listen<Record<string, unknown>>("agent-event", (e) => {
      if (stale) return;
      const env = e.payload as Record<string, unknown>;
      const p = (env?.payload ?? env) as Record<string, unknown>;
      const type = p?.type as string | undefined;

      switch (type) {
        case "text_delta": {
          const delta = p.delta as string;
          updateCurrentMessageRef.current((blocks) => {
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
          updateCurrentMessageRef.current((blocks) => {
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
          updateCurrentMessageRef.current((blocks) => {
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
          updateCurrentMessageRef.current((blocks) => {
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
        case "sub_agent_activity": {
          // 子 agent 实时活动 → 嵌套面板（按 agentId 分组）。仅展示，不进 LLM 上下文。
          const agentId = p.agentId as string;
          const kind = p.kind as string;
          const txt = (p.text as string) ?? "";
          updateCurrentMessageRef.current((blocks) => {
            let idx = blocks.findIndex((b) => b.type === "subagent" && b.agentId === agentId);
            if (idx < 0) {
              blocks.push({ type: "subagent", agentId, tools: [], text: "", done: false });
              idx = blocks.length - 1;
            }
            const b = blocks[idx];
            if (b.type !== "subagent") return blocks;
            if (kind === "tool_start") {
              blocks[idx] = { ...b, tools: [...b.tools, { name: txt, done: false }] };
            } else if (kind === "tool_end") {
              const tools = b.tools.slice();
              for (let i = tools.length - 1; i >= 0; i--) {
                if (!tools[i].done && txt.startsWith(tools[i].name)) {
                  tools[i] = { ...tools[i], done: true };
                  break;
                }
              }
              blocks[idx] = { ...b, tools };
            } else if (kind === "text") {
              blocks[idx] = { ...b, text: b.text + txt };
            } else if (kind === "done") {
              blocks[idx] = { ...b, done: true, text: b.text || txt };
            }
            return blocks;
          });
          break;
        }
        case "usage": {
          updateCurrentMessageRef.current((blocks) => {
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
          // finalize 当前 run 的 assistant 消息（按 runActiveRef 精确定位，支持 pending 队列流水线）。
          const aid = runActiveRef.current;
          if (aid) {
            setMessages((prev) => {
              const idx = prev.findIndex((m) => m.id === aid);
              if (idx < 0 || prev[idx].role !== "assistant") return prev;
              const next = [...prev];
              const last = next[idx];
              const hasText = last.blocks.some((b) => b.type === "text" && b.text);
              next[idx] = {
                ...last,
                streaming: false,
                blocks: hasText
                  ? last.blocks
                  : [...last.blocks, { type: "text" as const, text: "（已完成，无文本输出）" }],
              };
              return next;
            });
            finishRunRef.current?.(aid); // 收尾 + 自动发下一条 pending 消息
          } else {
            setSending(false);
          }
          break;
        }
        case "error": {
          // provider / loop 报错 → 在气泡里显示真实错误，而不是被兜底显示成「无文本输出」。
          const msg = (p.message as string) || (p.code as string) || "运行出错";
          const aid = runActiveRef.current;
          setMessages((prev) => {
            const idx = aid ? prev.findIndex((m) => m.id === aid) : prev.length - 1;
            if (idx < 0 || prev[idx].role !== "assistant") return prev;
            const next = [...prev];
            const last = next[idx];
            next[idx] = {
              ...last,
              streaming: false,
              error: true,
              blocks: [...last.blocks, { type: "text" as const, text: `运行失败：${msg}` }],
            };
            return next;
          });
          if (aid) finishRunRef.current?.(aid);
          else setSending(false);
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
      void refreshStateRef.current();
    }).then((u) => uns.push(u));
    void listen("agent-analysis-result", () => void refreshStateRef.current()).then((u) =>
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
    return () => { stale = true; uns.forEach((u) => u()); };
  }, []); // stable refs via useRef — no re-subscription needed

  const cancelCurrent = useCallback(async () => {
    const id = currentRunId.current;
    if (id)
      await commands.agentCancelRun({
        runId: id,
        reason: "用户停止当前 run",
      });
  }, []);

  // 只在「已经贴近底部」时才自动滚到底；用户往上滚看历史时不打扰（修复流式中无法上滚）。
  useEffect(() => {
    const el = chatScrollRef.current;
    if (!el) return;
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 120;
    if (nearBottom) chatEndRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages]);

  // 一轮 run 收尾（done 事件 / error / 兜底 timeout 调用）。按 assistantId 去重，避免重复收尾；
  // 收尾后若队列非空，自动发下一条 pending 消息（CC 式：做完一起发，不打断当前对话）。
  const finishRun = useCallback((assistantId: string) => {
    if (runActiveRef.current !== assistantId) return; // 已被别处收尾
    runActiveRef.current = null;
    setSending(false);
    const q = queuedRef.current;
    if (q.length > 0) {
      const [next, ...rest] = q;
      setQueued(rest);
      queuedRef.current = rest;
      void startRunRef.current?.(next.text, next.images);
    }
  }, []);

  // 真正发起一轮 run。finalize 仍由 `done` 事件负责（与 text_delta 同通道、顺序在其后），
  // 这里只在 error / done 丢失时按 assistantId 兜底，绝不在命令 resolve 时提前 finalize
  // （那会重演「晚到 text_delta 被丢弃」的 race）。
  const startRun = useCallback(
    async (text: string, images: string[]) => {
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
      const assistantId = assistantMsg.id;
      runActiveRef.current = assistantId;
      setMessages((prev) => [...prev, userMsg, assistantMsg]);
      setSending(true);

      const res = await commands.agentSendMessage({
        content: text,
        images: images.length > 0 ? images : null,
        conversationId: conversationId.current,
      });
      if (res.status === "error") {
        setMessages((prev) => {
          const idx = prev.findIndex((m) => m.id === assistantId);
          if (idx < 0) return prev;
          const next = [...prev];
          const last = next[idx];
          const hasText = last.blocks.some((b) => b.type === "text" && b.text);
          const errorBlocks = hasText
            ? last.blocks
            : [...last.blocks, { type: "text" as const, text: `运行失败：${res.error.message ?? res.error.code}` }];
          next[idx] = { ...last, streaming: false, error: true, blocks: errorBlocks };
          return next;
        });
        finishRun(assistantId);
      } else {
        // 兜底：done 事件丢失时，按 assistantId 收尾（不会误伤已开始的下一轮）。
        setTimeout(() => {
          setMessages((prev) => {
            const idx = prev.findIndex((m) => m.id === assistantId);
            if (idx < 0 || !prev[idx].streaming) return prev;
            const next = [...prev];
            const last = next[idx];
            const hasText = last.blocks.some((b) => b.type === "text" && b.text);
            next[idx] = {
              ...last,
              streaming: false,
              blocks: hasText ? last.blocks : [...last.blocks, { type: "text" as const, text: "（已完成，无文本输出）" }],
            };
            return next;
          });
          finishRun(assistantId);
        }, 5000);
      }
      void refreshState();
      void loadConversations();
    },
    [refreshState, loadConversations, finishRun],
  );
  startRunRef.current = startRun;
  finishRunRef.current = finishRun;

  // 提交输入框：有 run 在跑 → 入队（pending，不打断当前对话）；空闲 → 立即发起。
  const submitComposer = useCallback(() => {
    const text = input.trim();
    if (!text && pendingImages.length === 0) return;
    const images = pendingImages;
    setInput("");
    setPendingImages([]);
    if (runActiveRef.current !== null) {
      setQueued((q) => {
        const nq = [...q, { id: crypto.randomUUID(), text, images }];
        queuedRef.current = nq;
        return nq;
      });
    } else {
      void startRun(text, images);
    }
  }, [input, pendingImages, startRun]);

  const removeQueued = useCallback((id: string) => {
    setQueued((q) => {
      const nq = q.filter((m) => m.id !== id);
      queuedRef.current = nq;
      return nq;
    });
  }, []);

  const runningCount = useMemo(
    () =>
      state?.recentRuns?.filter((r) => r.status === "running").length ?? 0,
    [state],
  );

  // Sidebar shows only successful analysis results (no dialogue/failed runs).
  // Use recentResults directly; attach the parent run for trigger info.
  const analysisItems = useMemo(() => {
    const results = state?.recentResults ?? [];
    const runs = state?.recentRuns ?? [];
    const runMap = new Map(runs.map(r => [r.runId, r]));

    return results.map(result => ({
      result,
      run: runMap.get(result.runId) ?? null,
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
        {/* Left sidebar: conversations */}
        <aside className="agent-sidebar">
          <div className="agent-sidebar-section">
            <div className="agent-sidebar-section-header">
              <h3>对话</h3>
              <button className="agent-link-btn" onClick={newConversation}>+ 新对话</button>
            </div>
            {conversations.map(conv => (
              <div
                key={conv.conversationId}
                className={`agent-sidebar-item${conv.conversationId === conversationId.current ? " active" : ""}`}
                onClick={() => switchConversation(conv.conversationId)}
              >
                <div className="agent-conv-preview">{conv.preview || "新对话"}</div>
                <div className="agent-conv-time">{fmtTimeShort(conv.lastAt)}</div>
              </div>
            ))}
            {conversations.length === 0 && (
              <div className="agent-empty">暂无对话历史</div>
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
            <div className="agent-chat-scroll" ref={chatScrollRef}>
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
              {queued.map((q) => (
                <div key={q.id} className="agent-msg agent-msg-user agent-msg-queued">
                  <div className="agent-msg-avatar"><User size={16} /></div>
                  <div className="agent-msg-body">
                    <div className="agent-queued-row">
                      <span className="agent-queued-tag">排队中</span>
                      <span className="agent-queued-text">{q.text}</span>
                      <button
                        className="agent-queued-remove"
                        title="移除"
                        onClick={() => removeQueued(q.id)}
                      >
                        <X size={12} />
                      </button>
                    </div>
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
                {pendingImages.length > 0 && (
                  <div className="agent-image-preview">
                    {pendingImages.map((src, i) => (
                      <div key={i} className="agent-image-thumb">
                        <img src={src} alt="" />
                        <button
                          className="agent-image-remove"
                          onClick={() => setPendingImages((prev) => prev.filter((_, j) => j !== i))}
                        >
                          <X size={12} />
                        </button>
                      </div>
                    ))}
                  </div>
                )}
                <div className="agent-input-wrap">
                  <textarea
                    className="agent-input"
                    value={input}
                    placeholder={
                      sending
                        ? "运行中…可继续输入，Ctrl+Enter 排队，完成后自动发送（Ctrl+C 停止当前）"
                        : "输入消息，Ctrl+Enter 发送"
                    }
                    rows={3}
                    onChange={(e) => setInput(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
                        e.preventDefault();
                        void submitComposer();
                      }
                      if (e.key === "c" && e.ctrlKey && sending) {
                        void cancelCurrent();
                      }
                    }}
                  />
                  <div className="agent-input-bar">
                    <input
                      ref={fileInputRef}
                      type="file"
                      accept=".png,.jpg,.jpeg,.gif,.webp,.bmp,.svg"
                      multiple
                      hidden
                      onChange={(e) => {
                        const files = e.target.files;
                        if (!files) return;
                        Array.from(files).filter((f) => f.type.startsWith("image/")).forEach((f) => {
                          const reader = new FileReader();
                          reader.onload = () => {
                            if (typeof reader.result === "string") {
                              setPendingImages((prev) => [...prev, reader.result as string]);
                            }
                          };
                          reader.readAsDataURL(f);
                        });
                        e.target.value = "";
                      }}
                    />
                    {channels.length > 0 && (
                      <select
                        className="agent-model-select"
                        value={activeChannel?.channelId ?? ""}
                        onChange={(e) => void switchModel(e.target.value)}
                        disabled={sending}
                      >
                        {channels.map(ch => (
                          <option key={ch.channelId} value={ch.channelId}>
                            {ch.model} ({ch.provider})
                          </option>
                        ))}
                      </select>
                    )}
                    <button
                      className="agent-bar-btn"
                      onClick={() => fileInputRef.current?.click()}
                      disabled={sending}
                      title="上传图片"
                    >
                      <ImagePlus size={15} />
                    </button>
                    <span className="agent-input-hint">
                      {sending ? "运行中… 回车排队" : "Ctrl+Enter 发送"}
                    </span>
                  </div>
                </div>
              </>
            )}
          </div>
        </section>

        {/* Right sidebar: strategy + analysis + reports */}
        <aside className="agent-right-sidebar">
          <div
            className="agent-sidebar-strategy"
            onClick={() => void openStrategyModal()}
          >
            <span>投资策略{strategy ? ` V${strategy.version}` : ""}</span>
            <span className="agent-link-btn">查看</span>
          </div>

          <div className="agent-sidebar-section">
            <div className="agent-sidebar-section-header">
              <h3>资讯分析</h3>
              <button
                className="agent-link-btn"
                disabled={togglingNewsAuto}
                title="开启后，新资讯会自动进入分析队列（默认关闭）"
                onClick={() => void toggleNewsAuto()}
              >
                {newsAuto ? "自动分析：开" : "自动分析：关"}
              </button>
            </div>
            {analysisItems.map(({ result }) => {
              const title = summarizeLine(result.summary);
              return (
                <div
                  key={result.resultId}
                  className={`agent-sidebar-item agent-timeline-item${detailView?.type === "analysis" && (detailView as { type: "analysis"; data: AnalysisResult }).data.resultId === result.resultId ? " active" : ""}`}
                  onClick={() => toggleDetail({ type: "analysis", data: result })}
                >
                  <div className="agent-timeline-top">
                    <span className={`agent-timeline-kind kind-${result.kind}`}>
                      {result.kind === "action" ? "操作" : "观望"}
                    </span>
                    <span className="agent-timeline-time">{fmtTime(result.createdAt)}</span>
                  </div>
                  <div className="agent-timeline-summary">{title}</div>
                </div>
              );
            })}
            {analysisItems.length === 0 && (
              <div className="agent-empty">暂无分析结果</div>
            )}
          </div>

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
        </aside>
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
