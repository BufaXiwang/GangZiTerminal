// NewsRowMenu — 资讯行右键浮层。
//
// Spec: docs/design/frontend-design.md §4 资讯页（右键打开原文）
//
// 在点击位置渲染浮层，提供「用浏览器打开原文」「复制链接」。
// 原文走 Rust open_external（系统默认浏览器）；无 url 时禁用。
// 点击 menu 外区域或按 ESC 关闭。

import { useEffect, useRef } from "react";
import { ExternalLink, Link2 } from "lucide-react";
import { commands } from "../../bindings";

interface NewsRowMenuProps {
  x: number;
  y: number;
  url: string | null;
  onClose: () => void;
}

export function NewsRowMenu({ x, y, url, onClose }: NewsRowMenuProps) {
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

  const adjustedX = Math.min(x, window.innerWidth - 200);
  const adjustedY = Math.min(y, window.innerHeight - 90);

  const openOriginal = () => {
    if (url) {
      void commands.openExternal(url).then((res) => {
        if (res.status === "error") {
          // eslint-disable-next-line no-console
          console.error("openExternal failed:", res.error.code, res.error.message);
        }
      });
    }
    onClose();
  };

  const copyLink = () => {
    if (url) void navigator.clipboard?.writeText(url).catch(() => {});
    onClose();
  };

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
        onClick={openOriginal}
        disabled={!url}
        title={url ?? "该条无原文链接"}
      >
        <ExternalLink size={14} />
        <span>用浏览器打开原文</span>
      </button>
      <button
        type="button"
        role="menuitem"
        className="row-context-menu-item"
        onClick={copyLink}
        disabled={!url}
      >
        <Link2 size={14} />
        <span>复制链接</span>
      </button>
    </div>
  );
}
