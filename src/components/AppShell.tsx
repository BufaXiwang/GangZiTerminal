// AppShell — 72px sidebar + main content container。
//
// Spec: docs/design/frontend-design.md §3 全局布局
//
// 提供 main-scroll 容器；页面 PageShell 渲染在 main-scroll 内部。

import { useCallback, useEffect, type ReactNode } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Sidebar } from "./Sidebar";

interface AppShellProps {
  children: ReactNode;
}

export function AppShell({ children }: AppShellProps) {
  useEffect(() => {
    const onContextMenu = (e: MouseEvent) => {
      const t = e.target as HTMLElement | null;
      if (t?.closest("input, textarea, [contenteditable=true]")) return;
      e.preventDefault();
    };
    document.addEventListener("contextmenu", onContextMenu);
    return () => document.removeEventListener("contextmenu", onContextMenu);
  }, []);

  const onDragStart = useCallback((e: React.MouseEvent) => {
    if ((e.target as HTMLElement).closest("button, a, input, select")) return;
    e.preventDefault();
    void getCurrentWindow().startDragging();
  }, []);

  return (
    <div className="app-shell">
      <div className="app-drag-region" onMouseDown={onDragStart} />
      <Sidebar />
      <div className="main-content">
        <div className="main-scroll">{children}</div>
      </div>
    </div>
  );
}
