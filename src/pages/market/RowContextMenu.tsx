// RowContextMenu — 列表行右键浮层。
//
// Spec: docs/design/frontend-design.md §4 市场页（右键自选交互）
//
// 在点击位置渲染浮层，提供"加自选 / 移除自选"。
// 点击 menu 外区域或按 ESC 关闭。

import { useEffect, useRef } from "react";
import { Star, StarOff } from "lucide-react";

interface RowContextMenuProps {
  x: number;
  y: number;
  isStarred: boolean;
  onToggleStar: () => void;
  onClose: () => void;
}

export function RowContextMenu({
  x,
  y,
  isStarred,
  onToggleStar,
  onClose,
}: RowContextMenuProps) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const handleClick = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    // 下一个 tick 注册，避免触发本次 right-click 的关闭
    const t = window.setTimeout(() => {
      document.addEventListener("mousedown", handleClick);
      document.addEventListener("keydown", handleKey);
    }, 0);
    return () => {
      window.clearTimeout(t);
      document.removeEventListener("mousedown", handleClick);
      document.removeEventListener("keydown", handleKey);
    };
  }, [onClose]);

  // 防止菜单超出视口右下边
  const adjustedX = Math.min(x, window.innerWidth - 180);
  const adjustedY = Math.min(y, window.innerHeight - 80);

  return (
    <div
      ref={ref}
      className="row-context-menu"
      style={{ left: adjustedX, top: adjustedY }}
      role="menu"
    >
      <button
        type="button"
        role="menuitem"
        className="row-context-menu-item"
        onClick={() => {
          onToggleStar();
          onClose();
        }}
      >
        {isStarred ? (
          <>
            <StarOff size={14} />
            <span>移出自选</span>
          </>
        ) : (
          <>
            <Star size={14} />
            <span>加入自选</span>
          </>
        )}
      </button>
    </div>
  );
}
