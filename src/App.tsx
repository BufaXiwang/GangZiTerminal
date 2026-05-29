// App root — 路由 + AppShell 装配。
//
// Spec: docs/design/architecture.md（前端不持有业务真源） + frontend-design.md §3 App Shell
//
// 路由选型：HashRouter — Tauri 桌面壳 webview 在生产环境是 file:// / tauri://
// 协议，BrowserRouter 的 history.pushState 在某些 platform 上有兼容问题；
// hash 路由对桌面端最稳。

import { useEffect } from "react";
import { useLocation } from "react-router-dom";
import { AppShell } from "./components/AppShell";
import { ROUTES } from "./lib/router";
import { useWatchlistStore } from "./lib/watchlistStore";
import MarketPage from "./pages/MarketPage";
import NewsPage from "./pages/NewsPage";
import AccountPage from "./pages/AccountPage";
import SettingsPage from "./pages/SettingsPage";

// Keep-alive：所有 tab 同时挂载，靠 display 切换。切回来时不重建组件树，
// 不重跑 useEffect / IPC / KLineChart init，几乎瞬时。代价是常驻内存。
export default function App() {
  // 一次性加载自选列表，市场页 ⭐ 与模拟账户页共享这份集合。
  const loadWatchlist = useWatchlistStore((s) => s.load);
  useEffect(() => {
    void loadWatchlist();
  }, [loadWatchlist]);

  const { pathname } = useLocation();
  const active =
    pathname === ROUTES.news
      ? "news"
      : pathname === ROUTES.account
        ? "account"
        : pathname === ROUTES.settings
          ? "settings"
          : "market";

  return (
    <AppShell>
      <div style={{ display: active === "market" ? "contents" : "none" }}>
        <MarketPage />
      </div>
      <div style={{ display: active === "news" ? "contents" : "none" }}>
        <NewsPage />
      </div>
      <div style={{ display: active === "account" ? "contents" : "none" }}>
        <AccountPage />
      </div>
      <div style={{ display: active === "settings" ? "contents" : "none" }}>
        <SettingsPage />
      </div>
    </AppShell>
  );
}
