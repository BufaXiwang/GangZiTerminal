import { listen } from "@tauri-apps/api/event";
import { useEffect } from "react";
import { safeUnlisten } from "../lib/tauriEvents";
import type { ChatMessage } from "../types";

/**
 * 监听后端 chat_messages 的实时追加事件，合并到本地 messages state。
 *
 * 后端 db::append_chat_message 写盘后 emit `chat-message-appended` —— briefing /
 * review / chat 三条流水线写消息时都会触发，前端不必单独 refetch。
 *
 * 注意：listen() 是异步注册的——unmount 必须等到 promise 兑现后再 unlisten，
 * 否则会出现 cleanup 跑完后 handler 才挂上去导致泄漏。
 */
export function useChatMessageStream({
  enabled,
  setMessages,
}: {
  enabled: boolean;
  setMessages: React.Dispatch<React.SetStateAction<ChatMessage[]>>;
}) {
  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    listen<ChatMessage>("chat-message-appended", (event) => {
      const message = event.payload;
      if (!message?.id) return;
      setMessages((current) => {
        if (current.some((entry) => entry.id === message.id)) return current;
        // 按 createdAt 排序——避免乱序到达把旧消息挤到队首
        const next = [message, ...current];
        next.sort((a, b) => Date.parse(b.createdAt) - Date.parse(a.createdAt));
        return next.slice(0, 200);
      });
    })
      .then((handler) => {
        if (cancelled) safeUnlisten(handler);
        else unlisten = handler;
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
      safeUnlisten(unlisten);
    };
  }, [enabled, setMessages]);
}
