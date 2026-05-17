import { ChatPage } from "./ChatPage";
import { SettingsPage } from "./SettingsPage";
import { SimulationPage } from "./SimulationPage";
import type { ChatMessage, RiskAlert } from "../types";

export type SecondaryViewId = "simulation" | "chat" | "settings";

type Props = {
  activeView: SecondaryViewId;
  autoRefresh: boolean;
  databasePath: string | null;
  hasMoreMessages: boolean;
  isChatting: boolean;
  loadMoreMessages: () => void;
  messages: ChatMessage[];
  refreshInterval: number;
  riskAlerts: RiskAlert[];
  searchMessages: (query: string) => void;
  sendChatMessage: (content: string, images?: string[]) => void;
  setAutoRefresh: (value: boolean) => void;
  setRefreshInterval: (value: number) => void;
};

export function SecondaryView({
  activeView,
  autoRefresh,
  databasePath,
  hasMoreMessages,
  isChatting,
  loadMoreMessages,
  messages,
  refreshInterval,
  riskAlerts,
  searchMessages,
  sendChatMessage,
  setAutoRefresh,
  setRefreshInterval,
}: Props) {
  if (activeView === "simulation") {
    return <SimulationPage riskAlerts={riskAlerts} />;
  }

  if (activeView === "chat") {
    return (
      <ChatPage
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
      autoRefresh={autoRefresh}
      databasePath={databasePath}
      refreshInterval={refreshInterval}
      setAutoRefresh={setAutoRefresh}
      setRefreshInterval={setRefreshInterval}
    />
  );
}
