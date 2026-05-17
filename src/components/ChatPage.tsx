import { convertFileSrc } from "@tauri-apps/api/core";
import { ImagePlus, Loader2, Search, Send, Sparkles, Wrench, X } from "lucide-react";
import { Fragment, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useAgentEventStream } from "../hooks/useAgentEventStream";
import { formatDate } from "../lib/format";
import type { ChatMessage, InvestorMemoryUpdate, StreamingRunState } from "../types";

const acceptedImageTypes = new Set(["image/png", "image/jpeg", "image/webp", "image/gif"]);
const maxImagesPerMessage = 4;
const maxImageBytes = 8 * 1024 * 1024;

type Props = {
  hasMoreMessages: boolean;
  isChatting: boolean;
  loadMoreMessages: () => void;
  messages: ChatMessage[];
  searchMessages: (query: string) => void;
  sendChatMessage: (content: string, images?: string[]) => void;
};

export function ChatPage({
  hasMoreMessages,
  isChatting,
  loadMoreMessages,
  messages,
  searchMessages,
  sendChatMessage,
}: Props) {
  const [draft, setDraft] = useState("");
  const [searchInput, setSearchInput] = useState("");
  const messageListRef = useRef<HTMLDivElement>(null);
  // 用户粘进/拖进来的图，base64 data URL。submit 时一并落盘 + 走 Block::Image 喂给 agent。
  const [pendingImages, setPendingImages] = useState<string[]>([]);
  const fileInputRef = useRef<HTMLInputElement>(null);

  // 实时 agent run 状态——按 run_id 索引；agent loop 每条 TextDelta / ToolStart /
  // ToolEnd 都进这里。只展示 pipeline=chat 的 run。
  const [streamingRuns, setStreamingRuns] = useState<Record<string, StreamingRunState>>({});
  useAgentEventStream({ enabled: true, setStreamingRuns });
  const activeChatRun = useMemo(() => {
    const runs = Object.values(streamingRuns).filter((r) => r.pipeline === "chat");
    return runs[runs.length - 1] ?? null;
  }, [streamingRuns]);

  // Timeline：用户对话 + Agent chat 回复 + highlight 类消息。
  // briefing / review 已彻底下线（DB CHECK 不再允许这些 kind）。
  const timelineMessages = useMemo(
    () => [...messages].sort((a, b) => Date.parse(a.createdAt) - Date.parse(b.createdAt)),
    [messages],
  );

  function submit() {
    const content = draft.trim();
    const hasImages = pendingImages.length > 0;
    if ((!content && !hasImages) || isChatting) return;
    setDraft("");
    const images = pendingImages;
    setPendingImages([]);
    sendChatMessage(content, images);
  }

  function blobToDataUrl(blob: Blob): Promise<string> {
    return new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onerror = () => reject(reader.error ?? new Error("FileReader 失败"));
      reader.onload = () => resolve(reader.result as string);
      reader.readAsDataURL(blob);
    });
  }

  // 粘贴 / 拖拽 / 选文件——三个入口共用一段逻辑
  async function ingestFiles(files: FileList | null) {
    if (!files) return;
    const tasks: Promise<string | null>[] = [];
    for (const file of Array.from(files)) {
      if (!acceptedImageTypes.has(file.type) || file.size > maxImageBytes) continue;
      if (pendingImages.length + tasks.length >= maxImagesPerMessage) break;
      tasks.push(blobToDataUrl(file).catch(() => null));
    }
    const urls = (await Promise.all(tasks)).filter((u): u is string => !!u);
    if (urls.length) setPendingImages((cur) => [...cur, ...urls]);
  }

  async function onPaste(event: React.ClipboardEvent<HTMLTextAreaElement>) {
    const items = event.clipboardData?.items;
    if (!items) return;
    const files: File[] = [];
    for (const item of Array.from(items)) {
      if (item.kind === "file" && item.type.startsWith("image/")) {
        const f = item.getAsFile();
        if (f) files.push(f);
      }
    }
    if (files.length === 0) return; // 让原生粘贴文本走默认路径
    event.preventDefault();
    const dataTransfer = new DataTransfer();
    files.forEach((f) => dataTransfer.items.add(f));
    await ingestFiles(dataTransfer.files);
  }

  function removePendingImage(index: number) {
    setPendingImages((cur) => cur.filter((_, i) => i !== index));
  }

  // 只在"末尾真的有新消息"或"thinking 指示器切换"时滚到底。
  // 加载更早是头部 prepend——末尾 id 不变，不应该滚到底（否则用户点完按钮反而被弹到最新）。
  const lastBottomIdRef = useRef<string | null>(null);
  useEffect(() => {
    const list = messageListRef.current;
    if (!list) return;
    const lastId = timelineMessages[timelineMessages.length - 1]?.id ?? null;
    const tailChanged = lastId !== lastBottomIdRef.current;
    if (!tailChanged && !isChatting) return;
    lastBottomIdRef.current = lastId;
    list.scrollTo({ top: list.scrollHeight, behavior: "smooth" });
  }, [timelineMessages, isChatting]);

  // 搜索：输入防抖
  // 注意：只在 searchInput 真的变化时才查——searchMessages 是 App.tsx 内联箭头函数，
  // 每次 App 渲染都是新引用；如果 effect deps 包含 searchMessages，任何无关的 App
  // re-render（比如刚刚 setInvestorMemory）都会让 effect 重跑、触发 searchMessages("")
  // 把 list_chat_messages 的最新 50 条覆盖回去——用户点完"加载更早"看到的旧消息瞬间
  // 又消失就是这个原因。
  const prevSearchRef = useRef("");
  useEffect(() => {
    const trimmed = searchInput.trim();
    if (trimmed === prevSearchRef.current.trim()) return;
    prevSearchRef.current = searchInput;
    const timer = window.setTimeout(() => searchMessages(searchInput), 300);
    return () => window.clearTimeout(timer);
  }, [searchInput, searchMessages]);

  return (
    <section className="page-shell chat-page">
      <div className="section-head">
        <div>
          <h2>Agent 对话</h2>
          <p className="muted">和 Agent 实时对话。决策、开仓、调止损都在对话流里完成。</p>
        </div>
        <div className="chat-search-wrap">
          <Search size={14} />
          <input
            placeholder="搜索消息内容..."
            value={searchInput}
            onChange={(event) => setSearchInput(event.target.value)}
          />
        </div>
      </div>

      <div className="chat-layout single-stream">
        <div className="chat-main">
          <div className="chat-message-list" ref={messageListRef}>
            {hasMoreMessages && !searchInput && (
              <button className="ghost chat-load-more" type="button" onClick={loadMoreMessages}>
                加载更早消息
              </button>
            )}
            {timelineMessages.length === 0 ? (
              <div className="chat-welcome">
                <strong>对话流为空</strong>
                <p>直接提问。Agent 看到机会会主动开仓 / 调止损，过程会用自然语言汇报。</p>
              </div>
            ) : (
              timelineMessages.map((message) => (
                <article
                  className={`chat-message ${message.role} kind-${message.kind}`}
                  key={message.id}
                  data-message-id={message.id}
                >
                  <header>
                    {message.kind === "highlight" && (
                      <span className="kind-badge highlight">
                        <Sparkles size={11} />
                        Agent 划重点
                      </span>
                    )}
                    <time>{formatDate(message.createdAt)}</time>
                  </header>
                  {message.contentMd && (
                    <MarkdownText content={message.contentMd} highlight={searchInput.trim()} />
                  )}
                  <MessageImages images={message.contentJson?.images} />
                  {message.role === "assistant" && <MemoryChips message={message} />}
                </article>
              ))
            )}
            {(isChatting || activeChatRun) && (
              <StreamingRunCard
                run={activeChatRun}
                fallback={!activeChatRun}
              />
            )}
          </div>

          <div
            className="chat-composer"
            onDragOver={(e) => {
              if (e.dataTransfer.types.includes("Files")) e.preventDefault();
            }}
            onDrop={(e) => {
              if (!e.dataTransfer.files.length) return;
              e.preventDefault();
              void ingestFiles(e.dataTransfer.files);
            }}
          >
            {pendingImages.length > 0 && (
              <div className="chat-composer-images">
                {pendingImages.map((dataUrl, idx) => (
                  <div className="chat-composer-image" key={idx}>
                    <img src={dataUrl} alt={`待发送图片 ${idx + 1}`} />
                    <button
                      type="button"
                      className="chat-composer-image-remove"
                      onClick={() => removePendingImage(idx)}
                      aria-label="删除这张图"
                    >
                      <X size={12} />
                    </button>
                  </div>
                ))}
              </div>
            )}
            <textarea
              onChange={(event) => setDraft(event.target.value)}
              onKeyDown={(event) => {
                if ((event.metaKey || event.ctrlKey) && event.key === "Enter") submit();
              }}
              onPaste={(event) => void onPaste(event)}
              placeholder="问市场、聊判断、复盘想法...（支持粘贴 / 拖拽图片）"
              value={draft}
            />
            <div className="chat-composer-actions">
              <input
                ref={fileInputRef}
                type="file"
                accept="image/png,image/jpeg,image/webp,image/gif"
                multiple
                style={{ display: "none" }}
                onChange={(e) => {
                  void ingestFiles(e.target.files);
                  if (e.target) e.target.value = "";
                }}
              />
              <button
                type="button"
                className="chat-composer-image-btn"
                title="选择图片附件"
                onClick={() => fileInputRef.current?.click()}
              >
                <ImagePlus size={16} />
              </button>
              <button
                disabled={(!draft.trim() && pendingImages.length === 0) || isChatting}
                onClick={submit}
              >
                <Send size={16} />
                {isChatting ? "思考中" : "发送"}
              </button>
            </div>
          </div>
        </div>

      </div>
    </section>
  );
}

// ---------- 流式 in-progress run 卡片 ----------

/**
 * 实时展示一条正在进行的 agent run：
 * - 文本增量边到边渲（TextDelta 累积进 run.text）
 * - 每个 ToolStart/ToolEnd emit 一张工具卡（loading → done/error）
 * - run 收到 done/error 后被 hook 从 streamingRuns 删除，本卡片消失，最终
 *   消息会通过 chat-message-appended 落到 messages 列表。
 *
 * `fallback=true`：用户刚点 send，run 还没启动（run_start 没到），显示占位 dots。
 */
function StreamingRunCard({
  run,
  fallback,
}: {
  run: StreamingRunState | null;
  fallback: boolean;
}) {
  if (!run) {
    return (
      <article className="chat-message assistant thinking">
        <header>
          <time>{fallback ? "整理中" : "进行中"}</time>
        </header>
        <div className="thinking-line">
          <span />
          <span />
          <span />
        </div>
      </article>
    );
  }
  const hasText = run.text.length > 0;
  const hasTools = run.toolCalls.length > 0;
  return (
    <article className="chat-message assistant streaming">
      <header>
        <time>进行中 · {run.model}</time>
      </header>
      {hasTools && (
        <div className="streaming-tool-calls">
          {run.toolCalls.map((tc) => (
            <ToolCallChip key={tc.id} tc={tc} />
          ))}
        </div>
      )}
      {hasText ? (
        <div className="streaming-text">{run.text}</div>
      ) : (
        !hasTools && (
          <div className="thinking-line">
            <span />
            <span />
            <span />
          </div>
        )
      )}
    </article>
  );
}

function ToolCallChip({
  tc,
}: {
  tc: StreamingRunState["toolCalls"][number];
}) {
  const inputBrief = useMemo(() => briefInput(tc.input), [tc.input]);
  const statusClass =
    tc.status === "running" ? "running" : tc.status === "error" ? "error" : "done";
  return (
    <span className={`tool-call-chip ${statusClass}`}>
      {tc.status === "running" ? (
        <Loader2 size={11} className="spin" />
      ) : (
        <Wrench size={11} />
      )}
      <span className="tool-call-name">{tc.name}</span>
      {inputBrief ? <span className="tool-call-arg">{inputBrief}</span> : null}
      {tc.serverSide ? <span className="tool-call-tag">server</span> : null}
      {tc.durationMs !== undefined ? (
        <span className="tool-call-duration">{Math.round(tc.durationMs)}ms</span>
      ) : null}
    </span>
  );
}

/// 把 tool input 缩成一行可读摘要——常见字段优先（code / query / positionId），
/// 否则取头几个字段拼一下，最长 32 字符。
function briefInput(input: unknown): string {
  if (input === null || input === undefined) return "";
  if (typeof input !== "object") return String(input).slice(0, 32);
  const obj = input as Record<string, unknown>;
  for (const key of ["code", "query", "positionId", "ticker"]) {
    if (key in obj && obj[key] != null) {
      return String(obj[key]).slice(0, 32);
    }
  }
  const keys = Object.keys(obj).slice(0, 2);
  if (keys.length === 0) return "";
  const parts = keys.map((k) => `${k}=${String(obj[k]).slice(0, 16)}`);
  const out = parts.join(",");
  return out.length > 32 ? out.slice(0, 32) + "…" : out;
}


function MessageImages({ images }: { images?: string[] }) {
  const [openIdx, setOpenIdx] = useState<number | null>(null);
  if (!images || images.length === 0) return null;
  return (
    <>
      <div className="chat-message-images">
        {images.map((path, idx) => {
          // 兼容历史路径（如果以前临时存的是 data URL，原样用；新的都是绝对路径）
          const src = path.startsWith("data:") ? path : convertFileSrc(path);
          return (
            <button
              type="button"
              key={`${path}-${idx}`}
              className="chat-message-image-thumb"
              onClick={() => setOpenIdx(idx)}
              aria-label="点击查看大图"
            >
              <img src={src} alt={`附件 ${idx + 1}`} loading="lazy" />
            </button>
          );
        })}
      </div>
      {openIdx !== null && images[openIdx] && (
        <ImageLightbox
          src={images[openIdx].startsWith("data:") ? images[openIdx] : convertFileSrc(images[openIdx])}
          onClose={() => setOpenIdx(null)}
        />
      )}
    </>
  );
}

function ImageLightbox({ src, onClose }: { src: string; onClose: () => void }) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return createPortal(
    <div className="image-lightbox-overlay" onClick={onClose} role="presentation">
      <img src={src} alt="" onClick={(e) => e.stopPropagation()} />
      <button type="button" className="image-lightbox-close" onClick={onClose} aria-label="关闭">
        <X size={18} />
      </button>
    </div>,
    document.body,
  );
}

// ---------- Memory chips ----------

function MemoryChips({ message }: { message: ChatMessage }) {
  const adds = collectMemoryUpdateChips(message.contentJson?.memoryUpdates);
  const removes = collectMemoryUpdateChips(message.contentJson?.memoryRemovals);
  if (!adds.length && !removes.length) return null;
  return (
    <div className="chat-memory-footer">
      {adds.length > 0 && (
        <>
          <strong>已沉淀新记忆</strong>
          {adds.slice(0, 6).map((chip) => (
            <span key={`add-${chip}`}>{chip}</span>
          ))}
        </>
      )}
      {removes.length > 0 && (
        <>
          <strong className="memory-strike">已移除旧记忆</strong>
          {removes.slice(0, 6).map((chip) => (
            <span className="memory-strike" key={`rm-${chip}`}>{chip}</span>
          ))}
        </>
      )}
    </div>
  );
}

function collectMemoryUpdateChips(update?: InvestorMemoryUpdate) {
  if (!update) return [];
  const items: string[] = [];
  if (update.riskPreference) items.push(update.riskPreference);
  for (const value of update.focusThemes ?? []) items.push(value);
  for (const value of update.preferredMarkets ?? []) items.push(value);
  for (const value of update.learningGoals ?? []) items.push(value);
  for (const value of update.knownBiases ?? []) items.push(value);
  for (const value of update.investmentPrinciples ?? []) items.push(value);
  for (const value of update.watchQuestions ?? []) items.push(value);
  for (const value of update.recentInsights ?? []) items.push(value);
  return Array.from(new Set(items));
}

// ---------- Markdown ----------

function MarkdownText({ content, highlight }: { content: string; highlight?: string }) {
  const blocks = parseMarkdownBlocks(content);
  const render = (text: string) => renderInlineMarkdown(text, highlight);
  return (
    <div className="markdown-body">
      {blocks.map((block, index) => {
        if (block.type === "heading") return <h4 key={index}>{render(block.text)}</h4>;
        if (block.type === "quote") return <blockquote key={index}>{render(block.text)}</blockquote>;
        if (block.type === "code") return <pre key={index}><code>{withHighlight(block.text, highlight)}</code></pre>;
        if (block.type === "list") {
          return (
            <ul key={index}>
              {block.items?.map((item, itemIndex) => (
                <li key={itemIndex}>{render(item)}</li>
              ))}
            </ul>
          );
        }
        if (block.type === "ordered-list") {
          return (
            <ol key={index}>
              {block.items?.map((item, itemIndex) => (
                <li key={itemIndex}>{render(item)}</li>
              ))}
            </ol>
          );
        }
        if (block.type === "table" && block.table) {
          return (
            <table key={index}>
              <thead>
                <tr>
                  {block.table.header.map((cell, cellIndex) => (
                    <th key={cellIndex}>{render(cell)}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {block.table.rows.map((row, rowIndex) => (
                  <tr key={rowIndex}>
                    {row.map((cell, cellIndex) => (
                      <td key={cellIndex}>{render(cell)}</td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          );
        }
        return <p key={index}>{render(block.text)}</p>;
      })}
    </div>
  );
}

type MarkdownBlock = {
  type: "paragraph" | "heading" | "quote" | "list" | "ordered-list" | "code" | "table";
  text: string;
  items?: string[];
  table?: { header: string[]; rows: string[][] };
};

function parseMarkdownBlocks(content: string): MarkdownBlock[] {
  const lines = content.replace(/\r\n/g, "\n").split("\n");
  const blocks: MarkdownBlock[] = [];
  let paragraph: string[] = [];
  let code: string[] | null = null;

  const flushParagraph = () => {
    if (!paragraph.length) return;
    blocks.push({ type: "paragraph", text: paragraph.join(" ") });
    paragraph = [];
  };

  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (line.trim().startsWith("```")) {
      if (code) {
        blocks.push({ type: "code", text: code.join("\n") });
        code = null;
      } else {
        flushParagraph();
        code = [];
      }
      continue;
    }
    if (code) {
      code.push(line);
      continue;
    }
    const trimmed = line.trim();
    if (!trimmed) {
      flushParagraph();
      continue;
    }
    if (trimmed.startsWith("|") && trimmed.endsWith("|")) {
      const next = (lines[i + 1] ?? "").trim();
      if (/^\|\s*:?-{3,}.*\|$/.test(next)) {
        flushParagraph();
        const header = splitTableRow(trimmed);
        i += 1;
        const rows: string[][] = [];
        while (i + 1 < lines.length) {
          const candidate = lines[i + 1].trim();
          if (!candidate.startsWith("|") || !candidate.endsWith("|")) break;
          rows.push(splitTableRow(candidate));
          i += 1;
        }
        blocks.push({ type: "table", text: "", table: { header, rows } });
        continue;
      }
    }
    const heading = trimmed.match(/^#{1,4}\s+(.+)$/);
    if (heading) {
      flushParagraph();
      blocks.push({ type: "heading", text: heading[1] });
      continue;
    }
    if (trimmed.startsWith(">")) {
      flushParagraph();
      blocks.push({ type: "quote", text: trimmed.replace(/^>\s?/, "") });
      continue;
    }
    const ordered = trimmed.match(/^\d+[.)]\s+(.+)$/);
    if (ordered) {
      flushParagraph();
      const previous = blocks.at(-1);
      if (previous?.type === "ordered-list") previous.items!.push(ordered[1]);
      else blocks.push({ type: "ordered-list", text: "", items: [ordered[1]] });
      continue;
    }
    const bullet = trimmed.match(/^[-*]\s+(.+)$/);
    if (bullet) {
      flushParagraph();
      const previous = blocks.at(-1);
      if (previous?.type === "list") previous.items!.push(bullet[1]);
      else blocks.push({ type: "list", text: "", items: [bullet[1]] });
      continue;
    }
    paragraph.push(trimmed);
  }
  flushParagraph();
  if (code) blocks.push({ type: "code", text: code.join("\n") });
  return blocks;
}

function splitTableRow(line: string): string[] {
  return line
    .replace(/^\|/, "")
    .replace(/\|$/, "")
    .split("|")
    .map((cell) => cell.trim());
}

function renderInlineMarkdown(text: string, highlight?: string) {
  // bold 正则用 `(?:\\\*|[^*])+` 允许内部出现转义的星号（agent 写 "**\*ST：...**" 表示字面 *ST 时
  // 中间含 *，原 `[^*]+` 会让整个 bold 匹配失败、星号/反斜杠裸露在 UI 上）。
  // strip 时再把 `\*` 还原成 `*`。
  const tokens = text
    .split(/(\*\*(?:\\\*|[^*])+\*\*|\[[^\]]+\]\([^)]+\))/g)
    .filter(Boolean);
  return tokens.map((token, index) => {
    const bold = token.match(/^\*\*((?:\\\*|[^*])+)\*\*$/);
    if (bold) {
      const inner = bold[1].replace(/\\\*/g, "*");
      return <strong key={index}>{withHighlight(inner, highlight)}</strong>;
    }
    const link = token.match(/^\[([^\]]+)\]\(([^)]+)\)$/);
    if (link) {
      const [, label, url] = link;
      // 协议白名单：只允许 http/https、相对路径、锚点。其它（javascript:/data:/file:/...）
      // 退化为纯文本，防止 AI 输出或历史数据夹带恶意链接
      if (!isSafeLinkUrl(url)) {
        return <Fragment key={index}>{withHighlight(`[${label}](${url})`, highlight)}</Fragment>;
      }
      return (
        <a href={url} key={index} rel="noreferrer noopener" target="_blank">
          {withHighlight(label, highlight)}
        </a>
      );
    }
    // 非 bold / 非 link 的纯文本片段也可能含转义（如 "ST 股的 \*ST 标记"），同样还原 `\*` → `*`、`\_` → `_`。
    return <Fragment key={index}>{withHighlight(stripMdEscapes(token), highlight)}</Fragment>;
  });
}

function stripMdEscapes(text: string): string {
  return text.replace(/\\([*_])/g, "$1");
}

function isSafeLinkUrl(url: string): boolean {
  if (!url) return false;
  // 锚点 / 站内相对路径
  if (url.startsWith("#") || url.startsWith("/")) return true;
  // 仅放行 http(s)
  return /^https?:\/\//i.test(url);
}

function withHighlight(text: string, query?: string) {
  const trimmed = (query ?? "").trim();
  if (!trimmed) return text;
  const escaped = trimmed.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const parts = text.split(new RegExp(`(${escaped})`, "gi"));
  if (parts.length === 1) return text;
  return parts.map((part, index) =>
    part.toLowerCase() === trimmed.toLowerCase() ? <mark key={index}>{part}</mark> : <Fragment key={index}>{part}</Fragment>,
  );
}
