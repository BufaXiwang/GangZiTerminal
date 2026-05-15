import { ChatPage } from "./ChatPage";
import { SettingsPage } from "./SettingsPage";
import { SimulationPage } from "./SimulationPage";
import type {
  ArticleContent,
  ChatMessage,
  NewsItem,
  RiskAlert,
} from "../types";

export type SecondaryViewId = "simulation" | "chat" | "settings";

type Props = {
  activeView: SecondaryViewId;
  agentEnabled: boolean;
  agentStatus: string;
  autoRefresh: boolean;
  briefingInterval: number;
  bufferSize: number;
  databasePath: string | null;
  fetchArticle: (item: NewsItem) => Promise<ArticleContent | null>;
  hasMoreMessages: boolean;
  isChatting: boolean;
  loadMoreMessages: () => void;
  messages: ChatMessage[];
  pendingNewsCount: number;
  refreshInterval: number;
  reviewStatus: string;
  riskAlerts: RiskAlert[];
  searchMessages: (query: string) => void;
  sendChatMessage: (content: string, images?: string[]) => void;
  setAgentEnabled: (value: boolean) => void;
  setAutoRefresh: (value: boolean) => void;
  setBriefingInterval: (value: number) => void;
  setBufferSize: (value: number) => void;
  setRefreshInterval: (value: number) => void;
  triggerBriefingNow: () => void;
  triggerReviewNow: () => void;
  isBriefing: boolean;
  isReviewing: boolean;
  hasDueReview: boolean;
};

export function SecondaryView({
  activeView,
  agentEnabled,
  agentStatus,
  autoRefresh,
  briefingInterval,
  bufferSize,
  databasePath,
  fetchArticle,
  hasMoreMessages,
  isChatting,
  loadMoreMessages,
  messages,
  pendingNewsCount,
  refreshInterval,
  reviewStatus,
  riskAlerts,
  searchMessages,
  sendChatMessage,
  setAgentEnabled,
  setAutoRefresh,
  setBriefingInterval,
  setBufferSize,
  setRefreshInterval,
  triggerBriefingNow,
  triggerReviewNow,
  isBriefing,
  isReviewing,
  hasDueReview,
}: Props) {
  if (activeView === "simulation") {
    return (
      <SimulationPage riskAlerts={riskAlerts} />
    );
  }

  if (activeView === "chat") {
    return (
      <ChatPage
        fetchArticle={fetchArticle}
        hasMoreMessages={hasMoreMessages}
        isChatting={isChatting}
        loadMoreMessages={loadMoreMessages}
        messages={messages}
        searchMessages={searchMessages}
        sendChatMessage={sendChatMessage}
      />
    );
  }

  return (
    <SettingsPage
      agentEnabled={agentEnabled}
      agentStatus={agentStatus}
      autoRefresh={autoRefresh}
      briefingInterval={briefingInterval}
      bufferSize={bufferSize}
      databasePath={databasePath}
      hasDueReview={hasDueReview}
      isBriefing={isBriefing}
      isReviewing={isReviewing}
      pendingNewsCount={pendingNewsCount}
      refreshInterval={refreshInterval}
      reviewStatus={reviewStatus}
      setAgentEnabled={setAgentEnabled}
      setAutoRefresh={setAutoRefresh}
      setBriefingInterval={setBriefingInterval}
      setBufferSize={setBufferSize}
      setRefreshInterval={setRefreshInterval}
      triggerBriefingNow={triggerBriefingNow}
      triggerReviewNow={triggerReviewNow}
    />
  );
}
