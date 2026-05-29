// Sidebar — 72px 窄图标导航。
//
// Spec: docs/design/frontend-design.md §3 全局布局 / §1 决策优先
//
// 保留窄图标导航；不在侧栏塞复杂状态。

import { NavLink } from "react-router-dom";
import { Newspaper, Settings, TrendingUp, Wallet } from "lucide-react";
import { ROUTES } from "../lib/router";

const NAV_ITEMS = [
  { to: ROUTES.market, label: "市场", icon: TrendingUp },
  { to: ROUTES.news, label: "资讯", icon: Newspaper },
  { to: ROUTES.account, label: "模拟账户", icon: Wallet },
  { to: ROUTES.settings, label: "设置", icon: Settings },
] as const;

export function Sidebar() {
  return (
    <nav className="sidebar" aria-label="主导航">
      <div className="sidebar-logo" title="GangZi Terminal">G</div>
      <div className="sidebar-nav">
        {NAV_ITEMS.map(({ to, label, icon: Icon }) => (
          <NavLink
            key={to}
            to={to}
            end={to === ROUTES.market}
            className={({ isActive }) =>
              `sidebar-link${isActive ? " active" : ""}`
            }
            title={label}
            aria-label={label}
          >
            <Icon size={20} strokeWidth={1.75} />
          </NavLink>
        ))}
      </div>
    </nav>
  );
}
