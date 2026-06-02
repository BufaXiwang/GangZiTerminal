// App root — 路由 + AppShell 装配。
//
// Spec: docs/design/architecture.md（前端不持有业务真源） + frontend-design.md §3 App Shell
//
// 路由选型：HashRouter — Tauri 桌面壳 webview 在生产环境是 file:// / tauri://
// 协议，BrowserRouter 的 history.pushState 在某些 platform 上有兼容问题；
// hash 路由对桌面端最稳。

import { useEffect, useRef } from "react";
import { useLocation } from "react-router-dom";
import { AppShell } from "./components/AppShell";
import { ROUTES } from "./lib/router";
import { useWatchlistStore } from "./lib/watchlistStore";
import MarketPage from "./pages/MarketPage";
import NewsPage from "./pages/NewsPage";
import AccountPage from "./pages/AccountPage";
import SettingsPage from "./pages/SettingsPage";

// Keep-alive，但**懒挂载**：一个 tab 的内容只在「首次被激活」时挂载，之后靠 display 切换、
// 不再重建组件树（切回来几乎瞬时）。
//
// 为什么不一开始就全挂载：那样非默认页（如市场页）会在 display:none（0 尺寸）下挂载，
// KLineChart 在隐藏容器里 init → 画布 0 尺寸且加载不被重新触发；切到该 tab 只切 display、
// 不重挂载，K 线就一直空白，必须切周期才重跑 effect。懒挂载保证图表**首次在可见状态下 init**。
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

  // 记录已激活过的 tab；当前 active 同步标记（首次渲染即挂载，无空帧）。
  // 之后该 tab 一直保留在树里（keep-alive），只是用 display 隐藏。
  const mountedTabs = useRef<Set<string>>(new Set());
  mountedTabs.current.add(active);
  const mounted = (tab: string) => mountedTabs.current.has(tab);

  return (
    <AppShell>
      <div style={{ display: active === "market" ? "contents" : "none" }}>
        {mounted("market") && <MarketPage />}
      </div>
      <div style={{ display: active === "news" ? "contents" : "none" }}>
        {mounted("news") && <NewsPage />}
      </div>
      <div style={{ display: active === "account" ? "contents" : "none" }}>
        {mounted("account") && <AccountPage />}
      </div>
      <div style={{ display: active === "settings" ? "contents" : "none" }}>
        {mounted("settings") && <SettingsPage />}
      </div>
    </AppShell>
  );
}
