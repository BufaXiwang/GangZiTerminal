// PageShell — 标准页面骨架。
//
// Spec: docs/design/frontend-design.md §3 页面骨架
//
// page-shell
//   section-head (title + status + actions)
//   optional control strip (filter / search / segment / refresh)
//   workspace (list + detail / chart + table / timeline + inspector)

import type { ReactNode } from "react";
import { SectionHead, type SectionStatusTone } from "./SectionHead";

interface PageShellProps {
  title: string;
  status?: string;
  statusTone?: SectionStatusTone;
  meta?: ReactNode;
  actions?: ReactNode;
  /** 可选 control strip：filter / search / segment / refresh */
  controls?: ReactNode;
  /** 紧凑头部：隐藏大标题、压成单行状态条，给内容腾空间。 */
  compact?: boolean;
  children: ReactNode;
}

export function PageShell({
  title,
  status,
  statusTone,
  meta,
  actions,
  controls,
  compact,
  children,
}: PageShellProps) {
  return (
    <section className="page-shell">
      <SectionHead
        title={title}
        status={status}
        statusTone={statusTone}
        meta={meta}
        actions={actions}
        compact={compact}
      />
      {controls && <div className="page-shell-control-strip">{controls}</div>}
      <div className="page-shell-workspace">{children}</div>
    </section>
  );
}
