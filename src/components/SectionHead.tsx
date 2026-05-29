// SectionHead — 页面标题区。
//
// Spec: docs/design/frontend-design.md §3 页面骨架 — 标题 + 状态/更新时间 + 主操作
//
// 不做 hero 级放大；标题区只放当前页面的操作状态和少数主操作。

import type { ReactNode } from "react";

export type SectionStatusTone = "ok" | "warn" | "error" | "loading" | "stale";

interface SectionHeadProps {
  title: string;
  /** 简短状态文案，如 "已更新 9:30:12"、"加载中"、"行情过期" */
  status?: string;
  /** 状态点 tone，控制颜色 */
  statusTone?: SectionStatusTone;
  /** 额外的 meta 信息（更新时间、记录数等） */
  meta?: ReactNode;
  /** 主操作区，如刷新按钮、筛选切换 */
  actions?: ReactNode;
  /** 紧凑模式：隐藏大标题，状态/meta/操作压成单行，给下方内容腾空间。 */
  compact?: boolean;
}

export function SectionHead({
  title,
  status,
  statusTone = "ok",
  meta,
  actions,
  compact = false,
}: SectionHeadProps) {
  return (
    <header
      className={`page-shell-section-head${compact ? " compact" : ""}`}
    >
      <div className="section-head-title-block">
        {/* compact 下隐藏 h1，仅保留状态行（视觉去标题、可访问性保留） */}
        <h1 className="section-head-title">{title}</h1>
        {(status || meta) && (
          <div className="section-head-meta">
            {status && (
              <span>
                <span className={`status-dot ${statusTone}`} aria-hidden="true" />
                {status}
              </span>
            )}
            {meta}
          </div>
        )}
      </div>
      {actions && <div className="section-head-actions">{actions}</div>}
    </header>
  );
}
