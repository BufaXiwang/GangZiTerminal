// AppShell — 72px sidebar + main content container。
//
// Spec: docs/design/frontend-design.md §3 全局布局
//
// 提供 main-scroll 容器；页面 PageShell 渲染在 main-scroll 内部。

import type { ReactNode } from "react";
import { Sidebar } from "./Sidebar";

interface AppShellProps {
  children: ReactNode;
}

export function AppShell({ children }: AppShellProps) {
  return (
    <div className="app-shell">
      <Sidebar />
      <div className="main-content">
        <div className="main-scroll">{children}</div>
      </div>
    </div>
  );
}
