// KlineModal — 弹窗显示某只标的的 K 线图。
//
// 用于：模拟账户页点击自选股 → 弹窗展示 K 线，避免页面切换。
//
// 结构：
//   半透明背景 backdrop（点击关闭）
//   居中 modal 容器 66vw × 76vh
//     header：name + tsCode + 关闭按钮
//     KlinePeriodTabs
//     KlineCanvas（autoHeight）

import { X } from "lucide-react";
import { useEffect, useState } from "react";
import { KlineCanvas, type ChartPeriod } from "./KlineCanvas";
import { KlinePeriodTabs } from "../pages/market/KlinePeriodTabs";

interface KlineModalProps {
  open: boolean;
  tsCode: string | null;
  name?: string | null;
  category?: "stock" | "index" | "fund" | null;
  onClose: () => void;
}

export function KlineModal({
  open,
  tsCode,
  name,
  category,
  onClose,
}: KlineModalProps) {
  const [period, setPeriod] = useState<ChartPeriod>("day");

  // ESC 关闭
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  if (!open || !tsCode) return null;

  const pricePrecision = category === "fund" ? 3 : 2;

  return (
    <div
      className="kline-modal-backdrop"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
      role="dialog"
      aria-modal
    >
      <div className="kline-modal">
        <div className="kline-modal-header">
          <div className="kline-modal-title">
            <span className="detail-header-name">{name ?? tsCode}</span>
            <span className="detail-header-code tabular">{tsCode}</span>
          </div>
          <button
            type="button"
            className="btn ghost"
            onClick={onClose}
            aria-label="关闭"
          >
            <X size={14} />
          </button>
        </div>
        <div className="kline-modal-period">
          <KlinePeriodTabs value={period} onChange={setPeriod} />
        </div>
        <div className="kline-modal-chart">
          <KlineCanvas
            tsCode={tsCode}
            period={period}
            pricePrecision={pricePrecision}
          />
        </div>
      </div>
    </div>
  );
}
