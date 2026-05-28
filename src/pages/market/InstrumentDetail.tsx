// InstrumentDetail — 选中标的的右侧详情面板（C4 fully implemented）。
//
// C2 阶段为占位 stub；C4 commit 中加 K 线 + 摘要。

import type { ListMarketItem } from "../../bindings";

interface InstrumentDetailProps {
  item: ListMarketItem | null;
}

export function InstrumentDetail({ item }: InstrumentDetailProps) {
  if (!item) {
    return (
      <div className="detail-empty">
        <div className="muted">从列表选择一只标的以查看详情</div>
      </div>
    );
  }
  return (
    <div className="detail-empty">
      <div className="instrument-name">{item.name}</div>
      <div className="muted tabular">{item.tsCode}</div>
      <div className="muted" style={{ marginTop: 8 }}>
        K 线与摘要详情待 C4 实现
      </div>
    </div>
  );
}
