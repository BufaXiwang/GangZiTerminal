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
import { Bot, ImagePlus, Send, User, X } from "lucide-react";
import { PageShell } from "../components/PageShell";
import { ROUTES } from "../lib/router";
import {
  commands,
  type AgentStateSnapshot,
  type AnalysisResult,
  type ConversationSummary,
  type InvestmentStrategy,
  type ProviderChannelView,
  type ReviewReportRef,
  type StrategyHistoryEntry,
} from "../bindings";

import {
  fmtTime,
  fmtTimeShort,
  mergePersistedMessages,
  summarizeLine,
  type ChatBlock,
  type ChatMessage,
  type DetailView,
} from "./agent/chatModel";
import { ChatBlockView, collapseTodoBlocks } from "./agent/blocks";
import { AnalysisDetail, ReportDetail, StrategyModal } from "./agent/detail";

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
  /// 本页对话 run 的 runId（agent-event 按它路由；后台 news/review run 的事件不写对话气泡）。
  const dialogueRunId = useRef<string | null>(null);
  /// 历史加载/切会话后强制滚到底（绕过「贴近底部才自动滚」的判定——首屏 scrollTop=0 永远不算贴底）。
  const scrollToBottomPending = useRef(false);
  const chatEndRef = useRef<HTMLDivElement | null>(null);
  const chatScrollRef = useRef<HTMLDivElement | null>(null);

  // Load conversation list from backend.
  const loadConversations = useCallback(async () => {
    const res = await commands.agentListConversations();
    if (res.status === "ok") setConversations(res.data);
  }, []);

  // Create a new conversation.
  const newConversation = useCallback(() => {
    const id = globalThis.crypto?.randomUUID?.() ?? `conv_${Date.now()}`;
    localStorage.setItem("agent_conversation_id", id);
    conversationId.current = id;
    setMessages([]);
    void loadConversations();
  }, [loadConversations]);

  // Switch to an existing conversation.
  const switchConversation = useCallback((cid: string) => {
    localStorage.setItem("agent_conversation_id", cid);
    conversationId.current = cid;
    // 重置瞬态运行状态：防止上一会话/被杀 run 残留的 sending/队列/runActive 串到新会话。
    runActiveRef.current = null;
    queuedRef.current = [];
    setQueued([]);
    setSending(false);
    setMessages([]);
    commands.agentLoadConversation(cid).then(res => {
      if (res.status === "ok" && res.data.length > 0) {
        const converted = mergePersistedMessages(res.data);
        scrollToBottomPending.current = true;
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
  // 与 switchConversation 同一条转换路径（mergePersistedMessages）：tool_result user 消息
  // 合并进前一条 assistant 的 tool_call block，首屏与切会话渲染一致。
  useEffect(() => {
    const cid = conversationId.current;
    if (!cid) return;
    commands.agentLoadConversation(cid).then(res => {
      if (res.status === "ok" && res.data.length > 0) {
        const converted = mergePersistedMessages(res.data);
        if (converted.length > 0) {
          scrollToBottomPending.current = true;
          setMessages(converted);
        }
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
  const loadConversationsRef = useRef(loadConversations);
  loadConversationsRef.current = loadConversations;

  useEffect(() => {
    let stale = false;
    const uns: Array<() => void> = [];
    // listen() promise 在 unmount 后才 resolve 时立即注销，避免监听器泄漏。
    const track = (u: () => void) => {
      if (stale) u();
      else uns.push(u);
    };
    void listen<Record<string, unknown>>("agent-event", (e) => {
      if (stale) return;
      const env = e.payload as Record<string, unknown>;
      const p = (env?.payload ?? env) as Record<string, unknown>;
      const type = p?.type as string | undefined;
      const evRunId = p?.runId as string | undefined;

      // —— 按 runId 路由（关键）：后端所有 run（对话 / news 自动分析 / 复盘 / 账户触发）共用
      // `agent-event` 通道。对话气泡只消费**本页对话 run** 的事件；后台 run 的流不写气泡，
      // 否则会出现流污染 / 气泡被提前 finalize / 错误写错气泡。
      // 绑定时机：loop 的 run_start 事件先于该 run 的一切 delta（同一 mpsc 顺序），
      // trigger === "user_chat" 即本页发起的对话 run。
      if (type === "run_start") {
        if ((p.trigger as string) === "user_chat" && runActiveRef.current !== null) {
          dialogueRunId.current = evRunId ?? null;
          // run 启动时用户消息已落库 → 立刻刷新左侧会话列表（新对话即时出现，
          // 不等 agentSendMessage 在 run 终态后才 resolve）。
          void loadConversationsRef.current();
        }
        return;
      }
      const isDialogueEvent =
        evRunId !== undefined && evRunId === dialogueRunId.current;
      if (!isDialogueEvent) {
        // 后台 run：终态时仅刷新右侧总览，不碰对话区。
        if (type === "done" || type === "error") void refreshStateRef.current();
        return;
      }

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
              // 新工具调用 = 之前的文本是非末轮铺垫 → 清空（与「只回末轮结论」语义一致，防面板爆量）。
              blocks[idx] = { ...b, tools: [...b.tools, { name: txt, done: false }], text: "" };
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
              // 只保留尾部 ~4000 字符：长调研子任务的流式文本无上限累积会拖垮渲染。
              const merged = b.text + txt;
              blocks[idx] = { ...b, text: merged.length > 4000 ? merged.slice(-4000) : merged };
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
          // finalize 当前对话 run 的 assistant 消息（按 runActiveRef 精确定位，支持 pending 队列流水线）。
          dialogueRunId.current = null;
          const aid = runActiveRef.current;
          const stopReason = p.stopReason as string | undefined;
          if (aid) {
            setMessages((prev) => {
              const idx = prev.findIndex((m) => m.id === aid);
              if (idx < 0 || prev[idx].role !== "assistant") return prev;
              const next = [...prev];
              const last = next[idx];
              // 有任何可见内容（文本/工具/思考/子agent）就不贴占位；只在**真的空**时才提示。
              const hasContent = last.blocks.some(
                (b) =>
                  (b.type === "text" && b.text) ||
                  b.type === "tool_call" ||
                  b.type === "thinking" ||
                  b.type === "subagent",
              );
              // 非正常终态必须让用户看见（否则「跑了半天没结果也不知道为什么」）。
              const abnormal: Record<string, string> = {
                token_budget_exceeded:
                  "⚠️ 已达本次运行的 token 预算上限，回答被截断（任务太深可拆小步问，或在设置里调高 agent_run_token_budget）",
                max_turns: "⚠️ 已达最大工具轮数上限，任务可能未完成（可继续追问让它接着做）",
                context_limit: "⚠️ 上下文超出模型窗口，回答被截断（建议新开对话）",
                cancelled: "⏹ 已停止",
              };
              const notice = stopReason ? abnormal[stopReason] : undefined;
              let blocks = last.blocks;
              if (notice) {
                blocks = [...blocks, { type: "text" as const, text: notice }];
              } else if (!hasContent) {
                blocks = [...blocks, { type: "text" as const, text: "（已完成，无输出）" }];
              }
              next[idx] = { ...last, streaming: false, blocks };
              return next;
            });
            finishRunRef.current?.(aid); // 收尾 + 自动发下一条 pending 消息
          } else {
            setSending(false);
          }
          break;
        }
        case "error": {
          // provider / loop 报错 → 在**本对话 run 的气泡**里显示真实错误（已按 runId 路由到这里；
          // 不再回退「最后一条 assistant」——那会把后台 run 的错误写进别人的气泡）。
          const msg = (p.message as string) || (p.code as string) || "运行出错";
          const aid = runActiveRef.current;
          if (!aid) break;
          setMessages((prev) => {
            const idx = prev.findIndex((m) => m.id === aid);
            if (idx < 0 || prev[idx].role !== "assistant") return prev;
            const next = [...prev];
            const last = next[idx];
            // 只标 error + 追加错误文本；finalize（streaming=false + finishRun）交给随后的
            // done 事件（loop 的致命错误路径必发 done；非致命错误后 run 还会继续流）。
            next[idx] = {
              ...last,
              error: true,
              blocks: [...last.blocks, { type: "text" as const, text: `运行失败：${msg}` }],
            };
            return next;
          });
          break;
        }
        default:
          break;
      }
    }).then(track);
    // run 起止只刷新右侧总览；对话 run 的跟踪经 agent-event 的 run_start（trigger=user_chat）绑定，
    // 不再用「任何 run 启动都覆盖 currentRunId」——那会让 Ctrl+C 取消错后台 run。
    void listen("agent-run-started", () => void refreshStateRef.current()).then(track);
    void listen("agent-run-finished", () => {
      void refreshStateRef.current();
    }).then(track);
    void listen("agent-analysis-result", () => void refreshStateRef.current()).then(track);
    // news age-out 丢弃计数（spec §5/§7）：提示用户丢了多少。
    void listen<Record<string, unknown>>(
      "agent-news-buffer-dropped",
      (e) => {
        const p = ((e.payload as Record<string, unknown>)?.payload ??
          e.payload) as { count?: number };
        if (p?.count)
          console.warn(`资讯分析队列丢弃 ${p.count} 条（超时未分析）`);
      },
    ).then(track);
    return () => { stale = true; uns.forEach((u) => u()); };
  }, []); // stable refs via useRef — no re-subscription needed

  const cancelCurrent = useCallback(async () => {
    // 只取消**本页对话 run**（dialogueRunId）；绝不取消后台 news/review run。
    const id = dialogueRunId.current;
    if (id)
      await commands.agentCancelRun({
        runId: id,
        reason: "用户停止当前 run",
      });
  }, []);

  // 历史加载/切会话 → 立即滚到底（最新消息）；流式期间只在「已经贴近底部」时自动滚，
  // 用户往上滚看历史时不打扰（修复流式中无法上滚）。
  useEffect(() => {
    const el = chatScrollRef.current;
    if (!el) return;
    if (scrollToBottomPending.current) {
      scrollToBottomPending.current = false;
      chatEndRef.current?.scrollIntoView({ behavior: "auto" });
      return;
    }
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
      // 防空 user 气泡：空文本且无图片绝不发起 run。记栈以便定位是谁(队列 flush / 直发)传了空。
      if (!text.trim() && images.length === 0) {
        console.warn("[agent] startRun skipped: empty text + no images", new Error().stack);
        return;
      }
      setDetailView(null);
      const userMsg: ChatMessage = {
        id: globalThis.crypto?.randomUUID?.() ?? `msg_${Date.now()}`,
        role: "user",
        blocks: [
          ...(text ? [{ type: "text" as const, text }] : []),
          ...(images.length > 0
            ? [{ type: "text" as const, text: `📎 ${images.length} 张图片` }]
            : []),
        ],
        timestamp: new Date().toISOString(),
      };
      const assistantMsg: ChatMessage = {
        id: globalThis.crypto?.randomUUID?.() ?? `as_${Date.now()}`,
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
            const hasContent = last.blocks.some(
              (b) =>
                (b.type === "text" && b.text) ||
                b.type === "tool_call" ||
                b.type === "thinking" ||
                b.type === "subagent",
            );
            next[idx] = {
              ...last,
              streaming: false,
              blocks: hasContent ? last.blocks : [...last.blocks, { type: "text" as const, text: "（已完成，无输出）" }],
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
        const nq = [...q, { id: globalThis.crypto?.randomUUID?.() ?? `q_${Date.now()}`, text, images }];
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
              {messages
                // 不渲染「空 user 气泡」：role=user 但无任何非空文本块（孤儿/状态错乱产物）直接不显。
                .filter(
                  (m) =>
                    m.role !== "user" ||
                    m.blocks.some((b) => b.type === "text" && b.text.trim().length > 0),
                )
                .map((m) => (
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
                      // 用户消息：拼所有文本块（不只 blocks[0]），避免奇怪状态下渲染成空气泡。
                      <span>
                        {m.blocks
                          .filter((b) => b.type === "text")
                          .map((b) => (b as { text: string }).text)
                          .join("")
                          .trim() || "（空消息）"}
                      </span>
                    ) : (
                      // Assistant messages: render rich blocks
                      <>
                        {m.blocks.length === 0 && m.streaming && (
                          <span className="muted">思考中...</span>
                        )}
                        {collapseTodoBlocks(m.blocks).map((block, bi) => (
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

