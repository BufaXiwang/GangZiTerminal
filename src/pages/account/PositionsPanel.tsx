// PositionsPanel — 持仓表 + 选中持仓 K 线。C3 sub-step 填充。
//
// Spec: docs/design/account-module.md §2 仓位模型 + frontend-design.md §4 模拟账户页

import type { Position, TsCode } from "../../bindings";

interface PositionsPanelProps {
  positions: Position[];
  loading: boolean;
  selected: TsCode | null;
  onSelect: (tsCode: TsCode) => void;
  selectedItem: Position | null;
}

export function PositionsPanel(_props: PositionsPanelProps) {
  return (
    <div className="positions-panel-placeholder">
      <div className="muted">持仓表（C3 填充）</div>
    </div>
  );
}
