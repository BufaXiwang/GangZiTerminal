import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  BarChart3,
  Brain,
  MessageSquare,
  Settings,
  WalletCards,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { SecondaryView } from "./components/SecondaryView";
import { TodayPage } from "./components/TodayPage";
import { useAppState } from "./hooks/useAppState";
import { useChatMessageStream } from "./hooks/useChatMessageStream";
import { useNewsRefresh } from "./hooks/useNewsRefresh";
import { useQuotes } from "./hooks/useQuotes";
import { buildLearningProfile } from "./lib/learning";
import { isReviewDue, pickEarliestDue } from "./lib/reviewSchedule";
import { evaluateSimulationRisk } from "./lib/simulation";
import { safeUnlisten } from "./lib/tauriEvents";
import type {
  AnalysisRecord,
  ArticleContent,
  ChatMessage,
  NewsItem,
  SimulatedPosition,
} from "./types";

// 所有 app_state 持久化的 key（SQLite，不是 localStorage）
const autoRefreshKey = "gangzi-terminal.auto-refresh";
const refreshIntervalKey = "gangzi-terminal.refresh-interval";
const autoAgentKey = "gangzi-terminal.auto-agent";
const bufferSizeKey = "gangzi-terminal.buffer-size";
const briefingIntervalKey = "gangzi-terminal.briefing-interval";
const activeViewKey = "gangzi-terminal.active-view";

const simulationInitialCash = 20000;
const defaultBufferSize = 10;
const defaultBriefingIntervalMs = 10 * 60 * 1000;
const messagesPageSize = 50;

type ViewId = "today" | "simulation" | "chat" | "settings";

const navItems: Array<{ id: ViewId; label: string; icon: typeof BarChart3 }> = [
  { id: "chat", label: "Agent", icon: MessageSquare },
  { id: "today", label: "今日市场", icon: BarChart3 },
  { id: "simulation", label: "模拟账户", icon: WalletCards },
  { id: "settings", label: "设置", icon: Settings },
];


function App() {
  // ====== Ephemeral UI state ======
  const [isBriefing, setIsBriefing] = useState(false);
  const [isReviewing, setIsReviewing] = useState(false);
  const [isChatting, setIsChatting] = useState(false);
  const [agentStatus, setAgentStatus] = useState("等待资讯进入 buffer。");
  const [reviewStatus, setReviewStatus] = useState("等待到期复盘。");
  const [status, setStatus] = useState("正在准备数据源。");
  const [databaseLoaded, setDatabaseLoaded] = useState(false);
  const [databasePath, setDatabasePath] = useState<string | null>(null);

  // ====== DB-table-backed state（hydrated on init, written via dedicated commands） ======
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [hasMoreMessages, setHasMoreMessages] = useState(false);
  const [isLoadingMoreMessages, setIsLoadingMoreMessages] = useState(false);
  const [records, setRecords] = useState<AnalysisRecord[]>([]);
  const [simulatedPositions, setSimulatedPositions] = useState<SimulatedPosition[]>([]);

  // ====== app_state-backed state（唯一持久化路径 = SQLite app_state） ======
  const [activeView, setActiveView] = useAppState<ViewId>(
    activeViewKey,
    "today",
    // 兜底：上一版本的 "tasks" 视图已经移除；老值落到磁盘上要重置到 today
    (value) => (navItems.some((nav) => nav.id === value) ? value : "today"),
  );
  // 注：investorMemory 和 lastBriefingAt 不在前端 state 里——后端 pipeline 拥有它们的写入。
  // 前端从来不显示 / 不读取这两个值（learningProfile / dueReviewRecords 都不依赖它们）。
  // 删掉前端副本避免"前端 setState → useAppState 200ms debounce → 覆盖 backend 值"的双写问题。
  // agentEnabled 默认 false——在 app_state 真正 hydrate 完成前不会跑 briefing 循环
  // 避免冷启动一瞬间把已关闭的 Agent 误启动
  const [agentEnabled, setAgentEnabled, agentEnabledLoaded] = useAppState<boolean>(autoAgentKey, false);
  const [autoRefresh, setAutoRefresh] = useAppState<boolean>(autoRefreshKey, true);
  const [refreshInterval, setRefreshInterval] = useAppState<number>(refreshIntervalKey, 60000);
  const [bufferSize, setBufferSize] = useAppState<number>(bufferSizeKey, defaultBufferSize);
  const [briefingInterval, setBriefingInterval] = useAppState<number>(briefingIntervalKey, defaultBriefingIntervalMs);

  // ====== 子模块（hook 内部维护各自 state + 副作用） ======
  const {
    items,
    setItems,
    pendingNewsCount,
    setPendingNewsCount,
    isRefreshing,
    lastUpdated,
    refreshFeeds,
  } = useNewsRefresh();

  const { quotes } = useQuotes();

  useChatMessageStream({ enabled: databaseLoaded, setMessages });

  // ---------- 初始化（仅从 DB 表 hydrate；app_state 的 key 由 useAppState 自管） ----------
  useEffect(() => {
    let cancelled = false;
    async function loadDatabaseState() {
      try {
        const databaseInfo = await invoke<{ path: string; schemaVersion: number }>("initialize_database");
        if (!cancelled) setDatabasePath(databaseInfo.path);
        const [loadedRecords, loadedPositions, loadedNews, loadedMessages, pendingCount] =
          await Promise.all([
            invoke<AnalysisRecord[]>("list_analysis_records", { limit: 300 }).catch(() => []),
            invoke<SimulatedPosition[]>("list_simulated_positions").catch(() => []),
            invoke<NewsItem[]>("list_news_items", { limit: 300 }).catch(() => []),
            invoke<ChatMessage[]>("list_chat_messages", { limit: messagesPageSize }).catch(() => []),
            invoke<number>("count_pending_news").catch(() => 0),
          ]);
        if (cancelled) return;
        setRecords(loadedRecords);
        setSimulatedPositions(loadedPositions);
        setItems(loadedNews);
        setMessages(loadedMessages);
        setHasMoreMessages(loadedMessages.length >= messagesPageSize);
        setPendingNewsCount(pendingCount);
        setDatabaseLoaded(true);
      } catch (error) {
        if (cancelled) return;
        // 关键：失败路径不要 setDatabaseLoaded(true)！
        // 一旦 databaseLoaded=true，下面 records/simulatedPositions 的 DB-sync effect
        // 会立刻 fire replace_*([])，把磁盘上的真实数据覆盖成空。让 UI 停在 loading
        // 状态比静默删数据安全得多——用户看到状态栏的失败原因再决定下一步。
        setStatus(`SQLite 初始化失败：${error instanceof Error ? error.message : String(error)}`);
      }
    }
    void loadDatabaseState();
    return () => {
      cancelled = true;
    };
  }, [setItems, setPendingNewsCount]);

  // 注：records / simulated_positions 不再做"前端 state 变化 → 写盘"的 sync。
  // 所有写入由后端 pipeline 完成；前端只通过事件 refetch、render。前端 set* 只是更新视图。


  // ---------- 派生状态 ----------
  const learningProfile = useMemo(
    () => buildLearningProfile(records, simulatedPositions, quotes, simulationInitialCash),
    [quotes, records, simulatedPositions],
  );
  const riskAlerts = useMemo(
    () => evaluateSimulationRisk(simulationInitialCash, simulatedPositions, quotes),
    [quotes, simulatedPositions],
  );
  const dueReviewRecords = useMemo(
    () => records.filter(isReviewDue),
    [records],
  );

  // ---------- Briefing 调度全在后端 ----------
  // briefing_scan_loop（scheduler.rs）每 30s 扫 buffer + lastBriefingAt，命中条件即触发
  // briefing 流水线。前端只负责"在 agentEnabled=false 时显示暂停文案"。
  useEffect(() => {
    if (!databaseLoaded || !agentEnabledLoaded) return;
    if (!agentEnabled) setAgentStatus("自动 briefing 已暂停。");
  }, [agentEnabled, agentEnabledLoaded, databaseLoaded]);

  // ---------- 监听后端 briefing 事件 → refetch ----------
  // 后端流水线落盘完成后 emit `briefing-published` / `agent-status`。
  // 前端听到就刷一遍 records / messages / positions / items / pendingCount / memory
  // ——保持视图和 SQLite 同步。
  useEffect(() => {
    if (!databaseLoaded) return;
    let cancelled = false;
    let unlistenStatus: (() => void) | null = null;
    let unlistenBriefing: (() => void) | null = null;
    let unlistenPositions: (() => void) | null = null;

    listen<{ phase: string; message: string }>("agent-status", (evt) => {
      const { phase, message } = evt.payload ?? { phase: "", message: "" };
      if (phase === "idle" || phase === "done") setIsBriefing(false);
      if (phase === "claiming" || phase === "loading" || phase === "running" || phase === "writing") {
        setIsBriefing(true);
      }
      setAgentStatus(message);
    })
      .then((handler) => {
        if (cancelled) safeUnlisten(handler);
        else unlistenStatus = handler;
      })
      .catch(() => undefined);

    listen<{ messageId: string; tradeCallCount: number; coveredCount: number }>(
      "briefing-published",
      async () => {
        const [records, positions, items, pending, msgs] = await Promise.all([
          invoke<AnalysisRecord[]>("list_analysis_records", { limit: 300 }).catch(() => []),
          invoke<SimulatedPosition[]>("list_simulated_positions").catch(() => []),
          invoke<NewsItem[]>("list_news_items", { limit: 300 }).catch(() => []),
          invoke<number>("count_pending_news").catch(() => 0),
          invoke<ChatMessage[]>("list_chat_messages", { limit: messagesPageSize }).catch(() => []),
        ]);
        setRecords(records);
        setSimulatedPositions(positions);
        setItems(items);
        setPendingNewsCount(pending);
        setMessages(msgs);
        setHasMoreMessages(msgs.length >= messagesPageSize);
      },
    )
      .then((handler) => {
        if (cancelled) safeUnlisten(handler);
        else unlistenBriefing = handler;
      })
      .catch(() => undefined);

    listen("positions-changed", async () => {
      const positions = await invoke<SimulatedPosition[]>("list_simulated_positions").catch(() => []);
      setSimulatedPositions(positions);
    })
      .then((handler) => {
        if (cancelled) safeUnlisten(handler);
        else unlistenPositions = handler;
      })
      .catch(() => undefined);

    let unlistenReview: (() => void) | null = null;
    listen("review-published", async () => {
      // review 改了 analysis_records 和 position_events，刷新两边
      const [records, positions] = await Promise.all([
        invoke<AnalysisRecord[]>("list_analysis_records", { limit: 300 }).catch(() => []),
        invoke<SimulatedPosition[]>("list_simulated_positions").catch(() => []),
      ]);
      setRecords(records);
      setSimulatedPositions(positions);
    })
      .then((handler) => {
        if (cancelled) safeUnlisten(handler);
        else unlistenReview = handler;
      })
      .catch(() => undefined);

    // chat-replied: user/assistant 消息已经通过 chat-message-appended 实时进 messages，
    // investorMemory 后端写后前端没人渲染——不需要单独 refetch。

    return () => {
      cancelled = true;
      safeUnlisten(unlistenStatus);
      safeUnlisten(unlistenBriefing);
      safeUnlisten(unlistenPositions);
      safeUnlisten(unlistenReview);
    };
  }, [databaseLoaded, setItems, setPendingNewsCount]);


  // ---------- Review 调度全在后端 ----------
  // review_scan_loop（scheduler.rs）每 30s 扫到期 record，自动触发 review 流水线。
  // 前端只显示状态文案；具体进度通过 agent-status / review-published 事件回流。
  useEffect(() => {
    if (!agentEnabledLoaded) return;
    if (!agentEnabled) {
      setReviewStatus("自动复盘已暂停。");
    } else {
      setReviewStatus(
        dueReviewRecords.length
          ? `有 ${dueReviewRecords.length} 条假设待复盘。`
          : "暂无到期复盘。",
      );
    }
  }, [agentEnabled, agentEnabledLoaded, dueReviewRecords.length]);

  async function loadMoreMessages() {
    if (isLoadingMoreMessages || !hasMoreMessages) return;
    setIsLoadingMoreMessages(true);
    try {
      const oldest = messages[messages.length - 1];
      const more = await invoke<ChatMessage[]>("list_chat_messages", {
        before: oldest?.createdAt ?? null,
        limit: messagesPageSize,
      }).catch(() => []);
      setMessages((current) => {
        const known = new Set(current.map((m) => m.id));
        return [...current, ...more.filter((m) => !known.has(m.id))];
      });
      setHasMoreMessages(more.length >= messagesPageSize);
    } finally {
      setIsLoadingMoreMessages(false);
    }
  }

  async function searchMessages(query: string) {
    if (!query.trim()) {
      const fresh = await invoke<ChatMessage[]>("list_chat_messages", { limit: messagesPageSize }).catch(() => []);
      setMessages(fresh);
      setHasMoreMessages(fresh.length >= messagesPageSize);
      return;
    }
    const found = await invoke<ChatMessage[]>("search_chat_messages", { query, limit: 200 }).catch(() => []);
    setMessages(found);
    setHasMoreMessages(false);
  }

  // ---------- 资讯原文（hover 展示） ----------
  // 缓存查询 + fetch + save 都在后端的 fetch_article_content 内做了，前端单调一次。
  const fetchArticle = useCallback(async (item: NewsItem): Promise<ArticleContent | null> => {
    if (!item.link) return null;
    return invoke<ArticleContent>("fetch_article_content", {
      url: item.link,
      itemId: item.id,
      source: item.source,
      fallbackTitle: item.title,
      fallbackSummary: item.summary,
      fallbackPublished: item.published,
    }).catch(() => null);
  }, []);

  // ---------- 立即触发 briefing / review ----------
  // 后端 invoke_handler 一直暴露 run_briefing_now / run_review_now；这里给用户一个
  // "不等 scheduler tick"的入口。两个命令都有 AtomicBool 守门，重入安全。
  // pipeline 自己 emit agent-status / briefing-published / review-published——
  // UI 状态由现有 listener 接管，按钮只负责 invoke + 异常文案。
  function triggerBriefingNow() {
    if (isBriefing || pendingNewsCount === 0) return;
    setIsBriefing(true);
    void invoke("run_briefing_now")
      .catch((err) =>
        setStatus(`手动触发 briefing 失败：${err instanceof Error ? err.message : String(err)}`),
      )
      .finally(() => setIsBriefing(false));
  }

  function triggerReviewNow() {
    if (isReviewing) return;
    const record = pickEarliestDue(records);
    if (!record) {
      setStatus("没有到期的交易假设可复盘。");
      return;
    }
    setIsReviewing(true);
    void invoke("run_review_now", { recordId: record.id })
      .catch((err) =>
        setStatus(`手动触发复盘失败：${err instanceof Error ? err.message : String(err)}`),
      )
      .finally(() => setIsReviewing(false));
  }

  return (
    <main className="app">
      <aside className="sidebar">
        <div className="brand">
          <Brain size={24} />
          <div>
            <h1>GangZiTerminal</h1>
            <p>用 Agent 跟踪市场事件，训练可验证的投资判断</p>
          </div>
        </div>
        <nav className="nav-list">
          {navItems.map((item) => {
            const Icon = item.icon;
            return (
              <button className={activeView === item.id ? "active" : ""} key={item.id} onClick={() => setActiveView(item.id)}>
                <Icon size={17} />
                <span>{item.label}</span>
              </button>
            );
          })}
        </nav>
      </aside>

      <section className="content">
        <div className="main-scroll">
          {activeView === "today" ? (
            <TodayPage />
          ) : (
            <SecondaryView
              activeView={activeView}
              agentEnabled={agentEnabled}
              agentStatus={agentStatus}
              autoRefresh={autoRefresh}
              briefingInterval={briefingInterval}
              bufferSize={bufferSize}
              databasePath={databasePath}
              fetchArticle={fetchArticle}
              hasMoreMessages={hasMoreMessages}
              isChatting={isChatting}
              loadMoreMessages={() => void loadMoreMessages()}
              messages={messages}
              pendingNewsCount={pendingNewsCount}
              refreshInterval={refreshInterval}
              reviewStatus={reviewStatus}
              riskAlerts={riskAlerts}
              searchMessages={(query) => void searchMessages(query)}
              sendChatMessage={(content, images) => {
                const hasImages = images && images.length > 0;
                if ((!content.trim() && !hasImages) || isChatting) return;
                setIsChatting(true);
                // 防止 provider 长流卡死：5 分钟没回就强制清 spinner，让用户能重试
                const timeoutId = window.setTimeout(() => {
                  setIsChatting(false);
                  setStatus("对话超时（5 分钟）。Agent 可能仍在后台运行，请稍后查看对话流。");
                }, 5 * 60 * 1000);
                void invoke("send_chat_message_now", { content, images: images ?? [] })
                  .catch((err) => setStatus(err instanceof Error ? err.message : String(err)))
                  .finally(() => {
                    window.clearTimeout(timeoutId);
                    setIsChatting(false);
                  });
              }}
              setAgentEnabled={setAgentEnabled}
              setAutoRefresh={setAutoRefresh}
              setBriefingInterval={setBriefingInterval}
              setBufferSize={setBufferSize}
              setRefreshInterval={setRefreshInterval}
              triggerBriefingNow={triggerBriefingNow}
              triggerReviewNow={triggerReviewNow}
              isBriefing={isBriefing}
              isReviewing={isReviewing}
              hasDueReview={dueReviewRecords.length > 0}
            />
          )}
        </div>
      </section>
    </main>
  );
}

export default App;
