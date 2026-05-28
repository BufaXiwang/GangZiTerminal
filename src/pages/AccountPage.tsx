// AccountPage — 模拟账户页 placeholder。
//
// Spec: docs/design/frontend-design.md §4 模拟账户页
//
// F4 sub-agent 实现：账户总览 / 自选 / 风控 / 持仓 + K 线 / 最近平仓。

import { PageShell } from "../components/PageShell";

export default function AccountPage() {
  return (
    <PageShell title="模拟账户" status="待实现" statusTone="stale">
      <div className="placeholder-block">
        <div className="placeholder-title">模拟账户页（F4 sub-agent 负责）</div>
        <div>账户总览 / 自选 / 风控 / 持仓 / 复盘</div>
      </div>
    </PageShell>
  );
}
