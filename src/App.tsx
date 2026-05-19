import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  BarChart3,
  Brain,
  MessageSquare,
  Newspaper,
  Settings,
  WalletCards,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { NewsPage } from "./components/NewsPage";
import { SecondaryView } from "./components/SecondaryView";
import { TodayPage } from "./components/TodayPage";
import { ThesesPage } from "./components/ThesesPage";
import { PrinciplesPage } from "./components/PrinciplesPage";
import { ExpectationsPage } from "./components/ExpectationsPage";
import { StrategiesPage } from "./components/StrategiesPage";
import { LessonsPage } from "./components/LessonsPage";
import { HeuristicsPage } from "./components/HeuristicsPage";
import { useAppState } from "./hooks/useAppState";
import { useChatMessageStream } from "./hooks/useChatMessageStream";
import { useNewsRefresh } from "./hooks/useNewsRefresh";
import { useQuotes } from "./hooks/useQuotes";
import { evaluateSimulationRisk } from "./lib/simulation";
import { safeUnlisten } from "./lib/tauriEvents";
import type { ChatMessage, NewsItem, SimulatedPosition } from "./types";

// 所有 app_state 持久化的 key（SQLite，不是 localStorage）
const autoRefreshKey = "gangzi-terminal.auto-refresh";
const refreshIntervalKey = "gangzi-terminal.refresh-interval";
const activeViewKey = "gangzi-terminal.active-view";

const simulationInitialCash = 20000;
const messagesPageSize = 50;

/// 一级 nav——Agent 是聚合视图，内部含 5 个子 tab
type ViewId = "agent" | "today" | "news" | "simulation" | "settings";

/// Agent 视图内的子 tab——chat 是默认入口；其他 4 个看 agent 大脑状态
type AgentSubView =
  | "chat"
  | "expectations"
  | "strategies"
  | "heuristics"
  | "lessons";

const navItems: Array<{ id: ViewId; label: string; icon: typeof BarChart3 }> = [
  { id: "agent", label: "Agent", icon: MessageSquare },
  { id: "today", label: "市场", icon: BarChart3 },
  { id: "news", label: "资讯", icon: Newspaper },
  { id: "simulation", label: "模拟账户", icon: WalletCards },
  { id: "settings", label: "设置", icon: Settings },
];

/// Agent 内子 nav——左侧 rail 显示
const agentSubTabs: Array<{
  id: AgentSubView;
  icon: string;
  label: string;
  hint: string;
}> = [
  { id: "chat", icon: "💬", label: "Chat", hint: "和 agent 对话——决策入口" },
  { id: "expectations", icon: "📊", label: "Expectations", hint: "agent 当前跟踪的投资预期" },
  { id: "strategies", icon: "🎯", label: "Strategies", hint: "触发 expectation 的规则集" },
  { id: "heuristics", icon: "🧠", label: "Heuristics", hint: "agent 学到的启发式规则" },
  { id: "lessons", icon: "📝", label: "Lessons", hint: "每次复盘的原子观察" },
];

function App() {
  // ====== Ephemeral UI state ======
  const [isChatting, setIsChatting] = useState(false);
  const [status, setStatus] = useState("正在准备数据源。");
  const [databaseLoaded, setDatabaseLoaded] = useState(false);
  const [databasePath, setDatabasePath] = useState<string | null>(null);

  // ====== DB-table-backed state（hydrated on init, written via dedicated commands） ======
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [hasMoreMessages, setHasMoreMessages] = useState(false);
  const [isLoadingMoreMessages, setIsLoadingMoreMessages] = useState(false);
  const [simulatedPositions, setSimulatedPositions] = useState<SimulatedPosition[]>([]);

  // ====== app_state-backed state（唯一持久化路径 = SQLite app_state） ======
  const [activeView, setActiveView] = useAppState<ViewId>(
    activeViewKey,
    "agent",
    // 兜底：老视图值落到磁盘上时重置到 agent
    (value) => (navItems.some((nav) => nav.id === value) ? value : "agent"),
  );
  /// Agent 内子 tab 不持久化——session 内有效，每次进 Agent 默认 chat
  const [agentSubView, setAgentSubView] = useState<AgentSubView>("chat");
  const [autoRefresh, setAutoRefresh] = useAppState<boolean>(autoRefreshKey, true);
  const [refreshInterval, setRefreshInterval] = useAppState<number>(refreshIntervalKey, 60000);

  // ====== 子模块（hook 内部维护各自 state + 副作用） ======
  const { setItems } = useNewsRefresh();
  const { quotes } = useQuotes();

  useChatMessageStream({ enabled: databaseLoaded, setMessages });

  // ---------- 初始化（仅从 DB 表 hydrate；app_state 的 key 由 useAppState 自管） ----------
  useEffect(() => {
    let cancelled = false;
    async function loadDatabaseState() {
      try {
        const databaseInfo = await invoke<{ path: string; schemaVersion: number }>("initialize_database");
        if (!cancelled) setDatabasePath(databaseInfo.path);
        const [loadedPositions, loadedNews, loadedMessages] = await Promise.all([
          invoke<SimulatedPosition[]>("list_simulated_positions").catch(() => []),
          invoke<NewsItem[]>("list_news_items", { limit: 300 }).catch(() => []),
          invoke<ChatMessage[]>("list_chat_messages", { limit: messagesPageSize }).catch(() => []),
        ]);
        if (cancelled) return;
        setSimulatedPositions(loadedPositions);
        setItems(loadedNews);
        setMessages(loadedMessages);
        setHasMoreMessages(loadedMessages.length >= messagesPageSize);
        setDatabaseLoaded(true);
      } catch (error) {
        if (cancelled) return;
        setStatus(`SQLite 初始化失败：${error instanceof Error ? error.message : String(error)}`);
      }
    }
    void loadDatabaseState();
    return () => {
      cancelled = true;
    };
  }, [setItems]);

  // ---------- 派生状态 ----------
  const riskAlerts = useMemo(
    () => evaluateSimulationRisk(simulationInitialCash, simulatedPositions, quotes),
    [quotes, simulatedPositions],
  );

  // ---------- 监听后端持仓变化 → refetch ----------
  useEffect(() => {
    if (!databaseLoaded) return;
    let cancelled = false;
    let unlistenPositions: (() => void) | null = null;
    listen("positions-changed", async () => {
      const positions = await invoke<SimulatedPosition[]>("list_simulated_positions").catch(() => []);
      setSimulatedPositions(positions);
    })
      .then((handler) => {
        if (cancelled) safeUnlisten(handler);
        else unlistenPositions = handler;
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
      safeUnlisten(unlistenPositions);
    };
  }, [databaseLoaded]);

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
        {status !== "正在准备数据源。" && (
          <div className="sidebar-status" title={status}>{status}</div>
        )}
      </aside>

      <section className="content">
        <div className="main-scroll">
          {activeView === "today" ? (
            <TodayPage />
          ) : activeView === "news" ? (
            <NewsPage />
          ) : activeView === "agent" ? (
            <div style={{ display: "flex", height: "100%", overflow: "hidden" }}>
              {/* 左侧二级 rail — VS Code 风格：紧凑图标 + 文字 */}
              <nav
                style={{
                  width: 76,
                  flexShrink: 0,
                  borderRight: "1px solid #e5e7eb",
                  background: "#fafafa",
                  display: "flex",
                  flexDirection: "column",
                  padding: "8px 0",
                  gap: 2,
                }}
              >
                {agentSubTabs.map((tab) => {
                  const active = agentSubView === tab.id;
                  return (
                    <button
                      key={tab.id}
                      onClick={() => setAgentSubView(tab.id)}
                      title={tab.hint}
                      style={{
                        display: "flex",
                        flexDirection: "column",
                        alignItems: "center",
                        gap: 2,
                        padding: "10px 4px",
                        margin: "0 6px",
                        background: active ? "#fff" : "transparent",
                        border: active ? "1px solid #cbd5e1" : "1px solid transparent",
                        borderRadius: 6,
                        cursor: "pointer",
                        color: active ? "#0f172a" : "#64748b",
                        fontWeight: active ? 600 : 400,
                        boxShadow: active ? "0 1px 2px rgba(0,0,0,0.04)" : "none",
                        transition: "background 120ms, color 120ms",
                      }}
                    >
                      <span style={{ fontSize: 20, lineHeight: 1 }}>{tab.icon}</span>
                      <span style={{ fontSize: 10, marginTop: 2 }}>{tab.label}</span>
                    </button>
                  );
                })}
              </nav>

              {/* 主区——根据 sub view 渲染 */}
              <div style={{ flex: 1, overflow: "auto" }}>
                {agentSubView === "chat" ? (
                  <SecondaryView
                    activeView="chat"
                    autoRefresh={autoRefresh}
                    databasePath={databasePath}
                    hasMoreMessages={hasMoreMessages}
                    isChatting={isChatting}
                    loadMoreMessages={() => void loadMoreMessages()}
                    messages={messages}
                    refreshInterval={refreshInterval}
                    riskAlerts={riskAlerts}
                    searchMessages={(query) => void searchMessages(query)}
                    sendChatMessage={(content, images) => {
                      const hasImages = images && images.length > 0;
                      if ((!content.trim() && !hasImages) || isChatting) return;
                      setIsChatting(true);
                      const timeoutId = window.setTimeout(() => {
                        setIsChatting(false);
                        setStatus(
                          "对话超时（5 分钟）。Agent 可能仍在后台运行，请稍后查看对话流。",
                        );
                      }, 5 * 60 * 1000);
                      void invoke("send_chat_message_now", {
                        content,
                        images: images ?? [],
                      })
                        .catch((err) =>
                          setStatus(err instanceof Error ? err.message : String(err)),
                        )
                        .finally(() => {
                          window.clearTimeout(timeoutId);
                          setIsChatting(false);
                        });
                    }}
                    setAutoRefresh={setAutoRefresh}
                    setRefreshInterval={setRefreshInterval}
                  />
                ) : agentSubView === "expectations" ? (
                  <ExpectationsPage
                    onAskAgent={(prefill) => {
                      setAgentSubView("chat");
                      window.dispatchEvent(
                        new CustomEvent("agent-prefill", { detail: prefill }),
                      );
                    }}
                  />
                ) : agentSubView === "strategies" ? (
                  <StrategiesPage />
                ) : agentSubView === "heuristics" ? (
                  <HeuristicsPage />
                ) : (
                  <LessonsPage />
                )}
              </div>
            </div>
          ) : (
            <SecondaryView
              activeView={activeView as "simulation" | "settings"}
              autoRefresh={autoRefresh}
              databasePath={databasePath}
              hasMoreMessages={hasMoreMessages}
              isChatting={isChatting}
              loadMoreMessages={() => void loadMoreMessages()}
              messages={messages}
              refreshInterval={refreshInterval}
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
              setAutoRefresh={setAutoRefresh}
              setRefreshInterval={setRefreshInterval}
            />
          )}
        </div>
      </section>
    </main>
  );
}

export default App;
