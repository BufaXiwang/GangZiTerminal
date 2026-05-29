// AppShell — 72px sidebar + main content container。
//
// Spec: docs/design/frontend-design.md §3 全局布局
//
// 提供 main-scroll 容器；页面 PageShell 渲染在 main-scroll 内部。

import { useEffect, type ReactNode } from "react";
import { Sidebar } from "./Sidebar";

interface AppShellProps {
  children: ReactNode;
}

export function AppShell({ children }: AppShellProps) {
  // 屏蔽 WebView 原生右键菜单（后退/重载/检查）——改由各列表自定义浮层接管。
  // 例外：输入框 / 文本域 / contenteditable 保留原生菜单（复制粘贴）。
  useEffect(() => {
    const onContextMenu = (e: MouseEvent) => {
      const t = e.target as HTMLElement | null;
      if (t?.closest("input, textarea, [contenteditable=true]")) return;
      e.preventDefault();
    };
    document.addEventListener("contextmenu", onContextMenu);
    return () => document.removeEventListener("contextmenu", onContextMenu);
  }, []);

  return (
    <div className="app-shell">
      <Sidebar />
      <div className="main-content">
        <div className="main-scroll">{children}</div>
      </div>
    </div>
  );
}
