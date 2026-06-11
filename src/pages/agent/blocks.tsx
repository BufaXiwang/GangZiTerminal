// Agent 对话气泡内的 block 渲染组件（text/thinking/tool_call/usage/subagent/todo）。
// 纯展示组件：数据来自 chatModel.ChatBlock，不持有业务状态。

import { useState } from "react";
import { ChevronDown, ChevronRight, Loader, Zap } from "lucide-react";
import { renderMarkdown } from "../../lib/simpleMarkdown";
import { fmtTokens, truncate, type ChatBlock } from "./chatModel";

/* ---------- Chat block renderers ---------- */

// todo_write 是整表替换语义，只有最新一份清单有意义 → 渲染时只保留最后一个 todo_write 面板，
// 折叠掉更早的（否则 agent 多次 todo_write 会堆叠成 N 个清单，很乱）。
export function collapseTodoBlocks(blocks: ChatBlock[]): ChatBlock[] {
  let lastTodoIdx = -1;
  blocks.forEach((b, i) => {
    if (b.type === "tool_call" && b.name === "todo_write") lastTodoIdx = i;
  });
  if (lastTodoIdx < 0) return blocks;
  return blocks.filter(
    (b, i) => !(b.type === "tool_call" && b.name === "todo_write" && i !== lastTodoIdx),
  );
}

export function ChatBlockView({ block }: { block: ChatBlock }) {
  switch (block.type) {
    case "text":
      return <TextBlockView text={block.text} />;
    case "thinking":
      return <ThinkingBlockView text={block.text} defaultCollapsed={block.collapsed} />;
    case "tool_call":
      // todo_write 渲染成 live checklist（从 output/input 的 {items} 派生，复用 ToolEnd 信道）。
      if (block.name === "todo_write") {
        return <TodoBlockView input={block.input} output={block.output} />;
      }
      return (
        <ToolCallBlockView
          name={block.name}
          input={block.input}
          output={block.output}
          isError={block.isError}
          durationMs={block.durationMs}
          status={block.status}
        />
      );
    case "usage":
      return <UsageBlockView input={block.input} output={block.output} cacheRead={block.cacheRead} />;
    case "subagent":
      return (
        <SubAgentBlockView agentId={block.agentId} tools={block.tools} text={block.text} done={block.done} />
      );
    default:
      return null;
  }
}

// 子 agent 实时活动嵌套面板（fork 子 run 阻塞跑时，用户看到它在调什么工具 / 输出什么）。
// 默认折叠：跑动中头部实时显示当前工具，完成后显示工具计数；展开看完整过程 + markdown 结论。
export function SubAgentBlockView({
  tools,
  text,
  done,
}: {
  agentId: string;
  tools: Array<{ name: string; done: boolean }>;
  text: string;
  done: boolean;
}) {
  const [collapsed, setCollapsed] = useState(true);
  const runningTool = [...tools].reverse().find((t) => !t.done)?.name;
  const status = done
    ? `已完成（${tools.length} 次工具调用）`
    : runningTool
      ? `调研中 · ${runningTool}…`
      : "调研中…";
  return (
    <div
      style={{
        border: "1px solid var(--border, #1e293b)",
        borderLeft: "2px solid #a78bfa",
        borderRadius: 8,
        padding: "6px 10px",
        margin: "6px 0",
        background: "var(--bg-raised, rgba(167,139,250,0.05))",
        fontSize: 12,
      }}
    >
      <div
        style={{ display: "flex", gap: 6, alignItems: "center", cursor: "pointer", color: "#a78bfa" }}
        onClick={() => setCollapsed((v) => !v)}
      >
        {collapsed ? <ChevronRight size={13} /> : <ChevronDown size={13} />}
        <span>🤖 子 agent · {status}</span>
      </div>
      {!collapsed && (
        <div style={{ marginLeft: 18, marginTop: 4 }}>
          {tools.map((t, i) => (
            <div key={i} style={{ color: t.done ? "var(--text-dim,#94a3b8)" : "#fbbf24", fontFamily: "ui-monospace, monospace" }}>
              {t.done ? "■" : "▸"} {t.name}
            </div>
          ))}
          {text && (
            <div
              className="md-content"
              style={{ color: "#cbd5e1", marginTop: 4 }}
              dangerouslySetInnerHTML={{ __html: renderMarkdown(text) }}
            />
          )}
        </div>
      )}
    </div>
  );
}

// todo_write 的 live checklist。从 output（执行后回显）或 input（执行中预览）解析 {items}。
export function TodoBlockView({ input, output }: { input: string; output?: string }) {
  let items: Array<{ content: string; status: string }> = [];
  try {
    const src = output && output !== "{}" ? output : input;
    const parsed = JSON.parse(src);
    if (Array.isArray(parsed?.items)) items = parsed.items;
  } catch {
    /* malformed → 不渲染 */
  }
  if (items.length === 0) return null;
  const done = items.filter((i) => i.status === "completed").length;
  const mark = (s: string) => (s === "completed" ? "✔" : s === "in_progress" ? "▸" : "○");
  const color = (s: string) =>
    s === "completed" ? "#34d399" : s === "in_progress" ? "#fbbf24" : "#94a3b8";
  return (
    <div
      style={{
        border: "1px solid var(--border, #1e293b)",
        borderRadius: 8,
        padding: "8px 12px",
        margin: "6px 0",
        background: "var(--bg-raised, rgba(255,255,255,0.02))",
        fontSize: 13,
      }}
    >
      <div style={{ color: "var(--text-dim, #94a3b8)", fontSize: 12, marginBottom: 6 }}>
        📋 任务清单 · {done}/{items.length}
      </div>
      {items.map((it, i) => (
        <div key={i} style={{ display: "flex", gap: 8, alignItems: "baseline", padding: "1px 0" }}>
          <span style={{ color: color(it.status), width: 14, flexShrink: 0 }}>{mark(it.status)}</span>
          <span
            style={{
              color: it.status === "completed" ? "var(--text-dim, #94a3b8)" : "var(--text, #cbd5e1)",
              textDecoration: it.status === "completed" ? "line-through" : "none",
              fontWeight: it.status === "in_progress" ? 600 : 400,
            }}
          >
            {it.content}
          </span>
        </div>
      ))}
    </div>
  );
}

export function TextBlockView({ text }: { text: string }) {
  const cleaned = text
    .replace(/<use_tool[\s\S]*?<\/use_tool>/g, "")
    .replace(/<tool_result[\s\S]*?<\/tool_result>/g, "")
    .replace(/<tool_error[\s\S]*?<\/tool_error>/g, "")
    .trim();
  if (!cleaned) return null;
  return (
    <div
      className="chat-block-text md-content"
      dangerouslySetInnerHTML={{ __html: renderMarkdown(cleaned) }}
    />
  );
}

export function ThinkingBlockView({
  text,
  defaultCollapsed,
}: {
  text: string;
  defaultCollapsed: boolean;
}) {
  const [collapsed, setCollapsed] = useState(defaultCollapsed);
  return (
    <div
      className="chat-block-thinking"
      onClick={() => setCollapsed(!collapsed)}
    >
      <div className="chat-block-thinking-header">
        {collapsed ? <ChevronRight size={14} /> : <ChevronDown size={14} />}
        <span>思考过程</span>
      </div>
      {!collapsed && (
        <div className="chat-block-thinking-text">{text}</div>
      )}
    </div>
  );
}

export function ToolCallBlockView({
  name,
  input,
  output,
  isError,
  durationMs,
  status,
}: {
  name: string;
  input: string;
  output?: string;
  isError?: boolean;
  durationMs?: number;
  status: "running" | "done";
}) {
  const [expanded, setExpanded] = useState(false);
  return (
    <div className={`chat-block-tool${isError ? " is-error" : ""}`}>
      <div
        className="chat-block-tool-header"
        onClick={() => setExpanded(!expanded)}
      >
        {status === "running" ? (
          <Loader size={14} className="tool-spinner" />
        ) : (
          <Zap size={14} />
        )}
        <span className="tool-name">{name}</span>
        {status === "done" && durationMs != null && (
          <span className="tool-duration">{durationMs}ms</span>
        )}
        {status === "running" && (
          <span className="tool-duration">运行中...</span>
        )}
        <span className="tool-expand-hint">
          {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        </span>
      </div>
      {expanded && (
        <div className="chat-block-tool-body">
          <div className="tool-io-section">
            <span className="tool-io-label">Input</span>
            <pre>{truncate(input, 800)}</pre>
          </div>
          {output != null && (
            <div className="tool-io-section">
              <span className={`tool-io-label${isError ? " tool-io-error" : ""}`}>
                {isError ? "Error" : "Output"}
              </span>
              <pre>{truncate(output, 800)}</pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

export function UsageBlockView({
  input,
  output,
  cacheRead,
}: {
  input: number;
  output: number;
  cacheRead?: number;
}) {
  return (
    <div className="chat-block-usage">
      <Zap size={10} />
      {" "}
      {fmtTokens(input)} input · {fmtTokens(output)} output
      {cacheRead != null && cacheRead > 0 && (
        <> · {fmtTokens(cacheRead)} cache</>
      )}
    </div>
  );
}

