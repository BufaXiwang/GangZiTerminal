// Route 常量，避免硬编码字符串散落。
//
// Spec: docs/design/frontend-design.md §4 核心页面模式（市场 / 资讯 / 模拟账户）
//
// 使用 HashRouter（详见 main.tsx 注释），路径不带 `#` 前缀。

export const ROUTES = {
  market: "/",
  news: "/news",
  account: "/account",
  settings: "/settings",
} as const;

export type RouteKey = keyof typeof ROUTES;
