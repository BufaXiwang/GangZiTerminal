// Agent 对话的前端消息模型 + 持久化历史解析/合并 + 文案格式化（纯函数，无 JSX）。
//
// Spec: docs/design/agent-runtime-module.md §9（前端 chat）+ agent-infra §2（<use_tool>/<tool_result> 文本协议）

import type { AgentMessage, AnalysisResult, ReviewReportRef } from "../../bindings";

/* ---------- Rich chat message model ---------- */

export type ChatBlock =
  | { type: "text"; text: string }
  | { type: "thinking"; text: string; collapsed: boolean }
  | {
      type: "tool_call";
      id: string;
      name: string;
      input: string;
      output?: string;
      isError?: boolean;
      durationMs?: number;
      status: "running" | "done";
    }
  | { type: "usage"; input: number; output: number; cacheRead?: number }
  | {
      // 子 agent 实时活动（fork 子 run 跑时让用户看到它在干嘛；仅展示，不进 LLM 上下文）。
      type: "subagent";
      agentId: string;
      tools: Array<{ name: string; done: boolean }>;
      text: string;
      done: boolean;
    };

export interface ChatMessage {
  id: string;
  role: "user" | "assistant" | "system";
  blocks: ChatBlock[];
  streaming?: boolean;
  error?: boolean;
  timestamp?: string;
}

/** Format token count: 1200 → "1.2k", 500 → "500" */
export function fmtTokens(n: number): string {
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return String(n);
}

/** Format ISO timestamp to local timezone */
export function fmtTime(iso: string): string {
  try {
    return new Date(iso).toLocaleString("zh-CN", {
      year: "numeric", month: "2-digit", day: "2-digit",
      hour: "2-digit", minute: "2-digit", second: "2-digit",
      hour12: false,
    });
  } catch { return iso; }
}

/** Format ISO timestamp to shorter sidebar format */
export function fmtTimeShort(iso: string): string {
  try {
    const d = new Date(iso);
    const now = new Date();
    if (d.toDateString() === now.toDateString()) {
      return d.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false });
    }
    return d.toLocaleDateString("zh-CN", { month: "2-digit", day: "2-digit" });
  } catch { return iso; }
}

/** Extract a clean one-line title from markdown summary */
export function summarizeLine(summary: string): string {
  // Strip markdown headers/bold, take first meaningful line
  const clean = summary
    .replace(/^#{1,4}\s+/gm, "")
    .replace(/\*\*/g, "")
    .trim();
  // 跳过纯样板标签行（"结论"/"理由"/"总结"…）——模型常用 "## 结论" 开头，取它当标题毫无信息量。
  const boilerplate = /^(结论|理由|总结|分析|判断|摘要)\s*[:：]?\s*$/;
  const firstLine =
    clean.split("\n").find((l) => {
      const t = l.trim();
      return t.length > 0 && !boilerplate.test(t);
    }) ?? clean;
  // 去掉行首的 kind 重复（左侧已有「观望/出手」chip）与样板前缀，剩下的才是主题。
  const title = firstLine
    .trim()
    .replace(/^(no_action|action)\s*[:：]\s*/i, "")
    .replace(/^(结论|理由|总结)\s*[:：]\s*/, "");
  return title.length > 60 ? title.slice(0, 60) + "…" : title;
}

/** Parse persisted message text into ChatBlocks (text + tool_call from XML markers) */
export function parsePersistedBlocks(text: string, role: "user" | "assistant"): ChatBlock[] {
  const blocks: ChatBlock[] = [];
  // For user messages containing only tool results, parse them as tool_call blocks
  if (role === "user") {
    const resultRe = /<tool_(?:result|error)\s+name="([^"]*)"[^>]*call_id="([^"]*)"[^>]*>([\s\S]*?)<\/tool_(?:result|error)>/g;
    let match;
    while ((match = resultRe.exec(text)) !== null) {
      const isError = match[0].startsWith("<tool_error");
      blocks.push({
        type: "tool_call",
        id: match[2],
        name: match[1],
        input: "",
        output: match[3].slice(0, 500),
        isError,
        status: "done",
      });
    }
    // If we parsed tool results, skip adding raw text
    if (blocks.length > 0) return blocks;
  }
  // For assistant messages, parse <use_tool> as tool calls
  if (role === "assistant") {
    const toolRe = /<use_tool\s+name="([^"]*)">([\s\S]*?)<\/use_tool>/g;
    let lastIdx = 0;
    let match;
    while ((match = toolRe.exec(text)) !== null) {
      const before = text.slice(lastIdx, match.index).trim();
      if (before) blocks.push({ type: "text", text: before });
      blocks.push({
        type: "tool_call",
        id: `persisted_${match.index}`,
        name: match[1],
        input: match[2].slice(0, 500),
        status: "done",
      });
      lastIdx = match.index + match[0].length;
    }
    const after = text.slice(lastIdx).trim();
    if (after) blocks.push({ type: "text", text: after });
    if (blocks.length > 0) return blocks;
  }
  // Fallback: plain text
  const cleaned = text
    .replace(/<use_tool[\s\S]*?<\/use_tool>/g, "")
    .replace(/<tool_result[\s\S]*?<\/tool_result>/g, "")
    .replace(/<tool_error[\s\S]*?<\/tool_error>/g, "")
    .replace(/<tool_result_stub[^>]*\/>/g, "")
    .replace(/<task-notification[\s\S]*?<\/task-notification>/g, "")
    .trim();
  if (cleaned) blocks.push({ type: "text", text: cleaned });
  return blocks;
}

/** Convert persisted AgentMessages into merged ChatMessages.
 * Tool result messages (user role with <tool_result>) get merged into the preceding
 * assistant message as tool_call blocks with output filled in. */
export function mergePersistedMessages(msgs: import("../../bindings").AgentMessage[]): ChatMessage[] {
  const out: ChatMessage[] = [];
  for (const m of msgs) {
    if (m.role !== "user" && m.role !== "assistant") continue;
    const blocks: ChatBlock[] = m.blocks.flatMap(b => {
      if (b.type === "thinking") return [{ type: "thinking" as const, text: b.text, collapsed: true }];
      if (b.type === "text") return parsePersistedBlocks((b as {text:string}).text, m.role as "user"|"assistant");
      return [];
    });
    if (blocks.length === 0) continue;
    // User messages with only tool_call blocks (tool results) → merge into last assistant msg
    const allToolCalls = blocks.every(b => b.type === "tool_call");
    if (m.role === "user" && allToolCalls && out.length > 0 && out[out.length - 1].role === "assistant") {
      // Merge tool results into preceding assistant's tool_call blocks
      const lastAssistant = out[out.length - 1];
      for (const block of blocks) {
        if (block.type !== "tool_call") continue;
        // 历史回填：assistant 的 <use_tool> 不带 call_id（id=persisted_X），<tool_result> 带真实
        // call_id，两者 id 不匹配。改按 **name + 顺序** 配对：填第一个同名、尚无 output 的 tool_call。
        const existing = lastAssistant.blocks.find(
          b => b.type === "tool_call" && b.name === block.name && b.output === undefined
        );
        if (existing && existing.type === "tool_call") {
          existing.output = block.output;
          existing.isError = block.isError;
          existing.status = "done";
        } else {
          lastAssistant.blocks.push(block);
        }
      }
      continue;
    }
    out.push({
      id: m.messageId,
      role: m.role as "user" | "assistant",
      blocks,
      timestamp: m.createdAt,
    });
  }
  // 历史里以工具卡片收尾、没有任何后续文本的 assistant 消息 = 该轮 run 未正常收尾
  // （网络中断 / token 预算截断 / 应用重启）。正常完成的 run 必以文本结论收尾，
  // 所以这个启发式不会误标。给用户一个看得懂的标注，而不是莫名戛然而止。
  const last = out[out.length - 1];
  if (last && last.role === "assistant" && last.blocks.length > 0) {
    const tail = last.blocks[last.blocks.length - 1];
    if (tail.type === "tool_call") {
      last.blocks = [
        ...last.blocks,
        {
          type: "text" as const,
          text: "⚠️ 本轮运行未正常收尾（网络中断 / token 预算截断 / 应用重启），结论未产出——可重新提问继续。",
        },
      ];
      last.error = true;
    }
  }
  return out;
}

/** Truncate a string with an ellipsis if it exceeds maxLen */
export function truncate(s: string, maxLen: number): string {
  if (s.length <= maxLen) return s;
  return s.slice(0, maxLen) + "...";
}

export type DetailView =
  | { type: "analysis"; data: AnalysisResult }
  | { type: "report"; name: string; path: string }
  | null;
