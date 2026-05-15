import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { ExternalLink, ImagePlus, Loader2, PinOff, Pin, Search, Send, Sparkles, Wrench, X } from "lucide-react";
import { Fragment, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useAgentEventStream } from "../hooks/useAgentEventStream";
import { formatDate } from "../lib/format";
import type {
  ArticleContent,
  ChatMessage,
  InvestorMemoryUpdate,
  NewsItem,
  StreamingRunState,
} from "../types";

const acceptedImageTypes = new Set(["image/png", "image/jpeg", "image/webp", "image/gif"]);
const maxImagesPerMessage = 4;
const maxImageBytes = 8 * 1024 * 1024;

type Props = {
  fetchArticle: (item: NewsItem) => Promise<ArticleContent | null>;
  hasMoreMessages: boolean;
  isChatting: boolean;
  loadMoreMessages: () => void;
  messages: ChatMessage[];
  searchMessages: (query: string) => void;
  sendChatMessage: (content: string, images?: string[]) => void;
};

export function ChatPage({
  fetchArticle,
  hasMoreMessages,
  isChatting,
  loadMoreMessages,
  messages,
  searchMessages,
  sendChatMessage,
}: Props) {
  const [draft, setDraft] = useState("");
  const [searchInput, setSearchInput] = useState("");
  const [openMessageId, setOpenMessageId] = useState<string | null>(null);
  const [railTab, setRailTab] = useState<"briefings" | "reviews">("briefings");
  const messageListRef = useRef<HTMLDivElement>(null);
  // 用户粘进/拖进来的图，base64 data URL。submit 时一并落盘 + 走 Block::Image 喂给 agent。
  const [pendingImages, setPendingImages] = useState<string[]>([]);
  const fileInputRef = useRef<HTMLInputElement>(null);

  // 实时 agent run 状态——按 run_id 索引；agent loop 每条 TextDelta / ToolStart /
  // ToolEnd 都进这里。只展示 pipeline=chat 的 run；briefing / review 在右侧分页里。
  const [streamingRuns, setStreamingRuns] = useState<Record<string, StreamingRunState>>({});
  useAgentEventStream({ enabled: true, setStreamingRuns });
  const activeChatRun = useMemo(() => {
    const runs = Object.values(streamingRuns).filter((r) => r.pipeline === "chat");
    // 一般同时只会有一条；多条的话取最后一条
    return runs[runs.length - 1] ?? null;
  }, [streamingRuns]);

  // Timeline 中间：用户对话 + Agent chat 回复 + highlight + 系统消息
  // briefing 和 review 都搬到右侧分页里——避免对话流被 Agent 自产消息淹没
  const timelineMessages = useMemo(
    () =>
      messages
        .filter((message) => message.kind !== "briefing" && message.kind !== "review")
        .sort((a, b) => Date.parse(a.createdAt) - Date.parse(b.createdAt)),
    [messages],
  );

  const briefings = useMemo(
    () =>
      messages
        .filter((message) => message.kind === "briefing")
        .sort((a, b) => Date.parse(b.createdAt) - Date.parse(a.createdAt)),
    [messages],
  );

  const reviews = useMemo(
    () =>
      messages
        .filter((message) => message.kind === "review")
        .sort((a, b) => Date.parse(b.createdAt) - Date.parse(a.createdAt)),
    [messages],
  );

  const openMessage = useMemo(
    () =>
      [...briefings, ...reviews].find((message) => message.id === openMessageId) ?? null,
    [briefings, reviews, openMessageId],
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
          <p className="muted">中间是对话流；主 Agent 自主发布的简报和复盘在右侧分页，点击查看详情。</p>
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
                <p>简报在右侧列表里。你可以直接提问，复盘消息也会出现在这里。</p>
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
                    {message.kind === "review" && <span className="kind-badge review">复盘</span>}
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

        <SideRail
          briefings={briefings}
          reviews={reviews}
          tab={railTab}
          onTab={setRailTab}
          onOpen={(id) => setOpenMessageId(id)}
          activeId={openMessageId}
        />
      </div>

      {openMessage && openMessage.kind === "briefing" && (
        <BriefingModal
          briefing={openMessage}
          fetchArticle={fetchArticle}
          onClose={() => setOpenMessageId(null)}
        />
      )}
      {openMessage && openMessage.kind === "review" && (
        <ReviewModal review={openMessage} onClose={() => setOpenMessageId(null)} />
      )}
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

// ---------- Side rail（右侧分页：简报 / 复盘） ----------

function SideRail({
  briefings,
  reviews,
  tab,
  onTab,
  onOpen,
  activeId,
}: {
  briefings: ChatMessage[];
  reviews: ChatMessage[];
  tab: "briefings" | "reviews";
  onTab: (next: "briefings" | "reviews") => void;
  onOpen: (id: string) => void;
  activeId: string | null;
}) {
  const [autoFollow, setAutoFollow] = useState(true);
  const listRef = useRef<HTMLDivElement>(null);
  const lastTopIdRef = useRef<string | null>(null);

  const list = tab === "briefings" ? briefings : reviews;

  // 当前分页 list 第 0 位 id 变化（新到达） → 滚到顶
  useEffect(() => {
    const topId = list[0]?.id ?? null;
    if (topId && topId !== lastTopIdRef.current && lastTopIdRef.current !== null && autoFollow) {
      listRef.current?.scrollTo({ top: 0, behavior: "smooth" });
    }
    lastTopIdRef.current = topId;
  }, [list, autoFollow]);

  // 切分页时重置滚动 + 跟随的"上一项 id" 锚点
  useEffect(() => {
    listRef.current?.scrollTo({ top: 0 });
    lastTopIdRef.current = null;
  }, [tab]);

  return (
    <aside className="news-rail briefing-rail">
      <div className="news-rail-head side-rail-head">
        <div className="side-rail-tabs" role="tablist">
          <button
            type="button"
            role="tab"
            aria-selected={tab === "briefings"}
            className={`side-rail-tab${tab === "briefings" ? " active" : ""}`}
            onClick={() => onTab("briefings")}
          >
            简报
            <span className="side-rail-count">{briefings.length}</span>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === "reviews"}
            className={`side-rail-tab${tab === "reviews" ? " active" : ""}`}
            onClick={() => onTab("reviews")}
          >
            复盘
            <span className="side-rail-count">{reviews.length}</span>
          </button>
        </div>
        <button
          type="button"
          className={`briefing-follow${autoFollow ? " active" : ""}`}
          onClick={() => setAutoFollow((v) => !v)}
          title={autoFollow ? "新条目到达时自动滚到顶" : "已关闭自动跟随"}
        >
          {autoFollow ? <Pin size={12} /> : <PinOff size={12} />}
          <span>{autoFollow ? "跟随" : "不跟随"}</span>
        </button>
      </div>
      <div className="news-rail-list briefing-list" ref={listRef}>
        {list.length === 0 ? (
          <p className="muted briefing-empty">
            {tab === "briefings"
              ? "主 Agent 还没发布简报。等待 buffer 满或定时触发。"
              : "尚无复盘。已开仓的交易假设到期后会自动复盘。"}
          </p>
        ) : (
          list.map((message) =>
            tab === "briefings" ? (
              <BriefingRow
                key={message.id}
                briefing={message}
                active={activeId === message.id}
                onOpen={onOpen}
              />
            ) : (
              <ReviewRow
                key={message.id}
                review={message}
                active={activeId === message.id}
                onOpen={onOpen}
              />
            ),
          )
        )}
      </div>
    </aside>
  );
}

function BriefingRow({
  briefing,
  active,
  onOpen,
}: {
  briefing: ChatMessage;
  active: boolean;
  onOpen: (id: string) => void;
}) {
  const coveredCount = briefing.sourceNewsIds?.length ?? 0;
  const tradeCount = briefing.contentJson?.briefing?.tradeCalls?.length ?? 0;
  const headline = briefingTitle(briefing);
  return (
    <button
      type="button"
      className={`briefing-row${active ? " active" : ""}`}
      onClick={() => onOpen(briefing.id)}
    >
      <time>{formatDate(briefing.createdAt)}</time>
      <strong>{headline}</strong>
      <span className="briefing-row-meta">
        覆盖 {coveredCount} 条资讯
        {tradeCount > 0 ? ` · ${tradeCount} 个交易假设` : ""}
      </span>
    </button>
  );
}

function ReviewRow({
  review,
  active,
  onOpen,
}: {
  review: ChatMessage;
  active: boolean;
  onOpen: (id: string) => void;
}) {
  const r = review.contentJson?.review;
  // 标题来源：从 contentMd 抽第一行的 "复盘｜<原假设标题>" 后缀；否则给个回退
  const headline = (() => {
    const firstLine = review.contentMd.split("\n", 1)[0] ?? "";
    const stripped = firstLine.replace(/^[*#\s]*复盘[｜|]/, "").replace(/[*\s]+$/, "");
    return (stripped || "复盘记录").slice(0, 28);
  })();
  return (
    <button
      type="button"
      className={`briefing-row${active ? " active" : ""}`}
      onClick={() => onOpen(review.id)}
    >
      <time>{formatDate(review.createdAt)}</time>
      <strong>{headline}</strong>
      <span className="briefing-row-meta">
        {r?.thesisStatus
          ? `状态：${reviewStatusLabel(r.thesisStatus)}`
          : "已复盘"}
        {typeof r?.confidence === "number" ? ` · 置信 ${(r.confidence * 100).toFixed(0)}%` : ""}
      </span>
    </button>
  );
}

function reviewStatusLabel(status: string): string {
  switch (status) {
    case "validated":
      return "✅ 验证";
    case "invalidated":
      return "❌ 证伪";
    case "watching":
      return "👀 观察";
    case "inconclusive":
      return "❓ 证据不足";
    default:
      return status;
  }
}

function briefingTitle(briefing: ChatMessage): string {
  const data = briefing.contentJson?.briefing;
  // 1) Agent 在 JSON 里给的 headline（新版本）
  if (data?.headline && data.headline.trim().length > 0) {
    return data.headline.slice(0, 24);
  }
  // 2) 第一个 signal 主题
  if (data?.signals?.[0]?.theme) return data.signals[0].theme.slice(0, 24);
  // 3) 第一个 trade call 标的
  if (data?.tradeCalls?.[0]?.name) return data.tradeCalls[0].name.slice(0, 24);
  // 4) 旧 briefing 没有结构化字段：直接给个通用标题，不再去 markdown 里碰运气
  return "Agent 简报";
}

// ---------- Briefing modal ----------

function BriefingModal({
  briefing,
  fetchArticle,
  onClose,
}: {
  briefing: ChatMessage;
  fetchArticle: (item: NewsItem) => Promise<ArticleContent | null>;
  onClose: () => void;
}) {
  const [selectedNewsId, setSelectedNewsId] = useState<string | null>(null);
  const [article, setArticle] = useState<ArticleContent | null>(null);
  const [loadingArticle, setLoadingArticle] = useState(false);
  const [coveredNews, setCoveredNews] = useState<NewsItem[]>([]);
  const [loadingNews, setLoadingNews] = useState(false);
  const [newsError, setNewsError] = useState<string | null>(null);

  // 按 sourceNewsIds 直接从 DB 查——避免依赖 items state（它只缓存最近 300 条，旧 briefing 的资讯查不到）
  useEffect(() => {
    const ids = briefing.sourceNewsIds ?? [];
    if (ids.length === 0) {
      setCoveredNews([]);
      setNewsError(null);
      return;
    }
    setLoadingNews(true);
    setNewsError(null);
    let cancelled = false;
    // SQLite 默认参数上限 999；按 500 一批分块以兜底超大 briefing
    const chunks: string[][] = [];
    for (let i = 0; i < ids.length; i += 500) {
      chunks.push(ids.slice(i, i + 500));
    }
    Promise.all(
      chunks.map((chunk) => invoke<NewsItem[]>("get_news_items_by_ids", { ids: chunk })),
    )
      .then((batches) => {
        if (cancelled) return;
        setCoveredNews(batches.flat());
      })
      .catch((err) => {
        if (cancelled) return;
        setCoveredNews([]);
        setNewsError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (!cancelled) setLoadingNews(false);
      });
    return () => {
      cancelled = true;
    };
  }, [briefing.sourceNewsIds]);

  const selectedNews = useMemo(
    () => coveredNews.find((item) => item.id === selectedNewsId) ?? null,
    [coveredNews, selectedNewsId],
  );

  // Esc 关闭
  useEffect(() => {
    function onKey(event: KeyboardEvent) {
      if (event.key === "Escape") onClose();
    }
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  // 选中资讯后取正文
  useEffect(() => {
    if (!selectedNews) {
      setArticle(null);
      return;
    }
    setLoadingArticle(true);
    let cancelled = false;
    fetchArticle(selectedNews)
      .then((result) => {
        if (!cancelled) setArticle(result);
      })
      .finally(() => {
        if (!cancelled) setLoadingArticle(false);
      });
    return () => {
      cancelled = true;
    };
  }, [selectedNews, fetchArticle]);

  return createPortal(
    <div className="modal-backdrop" onClick={onClose}>
      <div className="briefing-modal" onClick={(event) => event.stopPropagation()}>
        <header className="briefing-modal-head">
          <div>
            <strong>简报详情</strong>
            <time>{formatDate(briefing.createdAt)}</time>
          </div>
          <button className="modal-close" type="button" onClick={onClose} aria-label="关闭">
            <X size={16} />
          </button>
        </header>
        <div className="briefing-modal-body">
          <div className="briefing-modal-content">
            <MarkdownText content={briefing.contentMd} />
            <MemoryChips message={briefing} />
          </div>
          <aside className="briefing-modal-side">
            {selectedNews ? (
              <NewsDetailView
                news={selectedNews}
                article={article}
                loading={loadingArticle}
                onBack={() => setSelectedNewsId(null)}
              />
            ) : (
              <NewsListView
                covered={coveredNews}
                loading={loadingNews}
                error={newsError}
                onSelect={(id) => setSelectedNewsId(id)}
              />
            )}
          </aside>
        </div>
      </div>
    </div>,
    document.body,
  );
}

// ---------- Review modal ----------

function ReviewModal({
  review,
  onClose,
}: {
  review: ChatMessage;
  onClose: () => void;
}) {
  // Esc 关闭
  useEffect(() => {
    function onKey(event: KeyboardEvent) {
      if (event.key === "Escape") onClose();
    }
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [onClose]);

  const r = review.contentJson?.review;
  return createPortal(
    <div className="modal-backdrop" onClick={onClose}>
      <div className="briefing-modal review-modal" onClick={(event) => event.stopPropagation()}>
        <header className="briefing-modal-head">
          <div>
            <strong>复盘详情</strong>
            <time>{formatDate(review.createdAt)}</time>
          </div>
          <button className="modal-close" type="button" onClick={onClose} aria-label="关闭">
            <X size={16} />
          </button>
        </header>
        <div className="briefing-modal-body review-modal-body">
          <div className="briefing-modal-content">
            {r && (
              <div className="review-modal-meta">
                <span className={`review-status-pill review-status-${r.thesisStatus}`}>
                  {reviewStatusLabel(r.thesisStatus)}
                </span>
                {typeof r.confidence === "number" && (
                  <span className="review-confidence">置信 {(r.confidence * 100).toFixed(0)}%</span>
                )}
                {r.nextReviewAt && (
                  <span className="review-next">下次复盘：{formatDate(r.nextReviewAt)}</span>
                )}
              </div>
            )}
            <MarkdownText content={review.contentMd} />
            {r?.evidence && r.evidence.length > 0 && (
              <ReviewSection title="证据" items={r.evidence} />
            )}
            {r?.priceAction && r.priceAction.length > 0 && (
              <ReviewSection title="价格行为" items={r.priceAction} />
            )}
            {r?.newsFollowUp && r.newsFollowUp.length > 0 && (
              <ReviewSection title="后续消息面" items={r.newsFollowUp} />
            )}
            {r?.checklistReview && r.checklistReview.length > 0 && (
              <ReviewSection title="验证清单复查" items={r.checklistReview} />
            )}
            {r?.mistakes && r.mistakes.length > 0 && (
              <ReviewSection title="偏差识别" items={r.mistakes} />
            )}
            {r?.nextActions && r.nextActions.length > 0 && (
              <ReviewSection title="后续动作" items={r.nextActions} />
            )}
            {r?.learningUpdate && (
              <div className="review-section">
                <strong>学习沉淀</strong>
                <p>{r.learningUpdate}</p>
              </div>
            )}
          </div>
        </div>
      </div>
    </div>,
    document.body,
  );
}

function ReviewSection({ title, items }: { title: string; items: string[] }) {
  return (
    <div className="review-section">
      <strong>{title}</strong>
      <ul>
        {items.map((item, index) => (
          <li key={index}>{item}</li>
        ))}
      </ul>
    </div>
  );
}

function NewsListView({
  covered,
  loading,
  error,
  onSelect,
}: {
  covered: NewsItem[];
  loading: boolean;
  error: string | null;
  onSelect: (id: string) => void;
}) {
  return (
    <>
      <div className="modal-side-head">
        <strong>本简报覆盖的资讯</strong>
        <span>{loading ? "加载中…" : error ? "加载失败" : `${covered.length} 条`}</span>
      </div>
      <div className="modal-side-list">
        {loading ? (
          <div className="modal-news-loading">
            <Loader2 className="spin" size={14} /> 正在拉取资讯…
          </div>
        ) : error ? (
          <p className="muted">资讯加载失败：{error}</p>
        ) : covered.length === 0 ? (
          <p className="muted">该简报没有挂载资讯（可能是 Agent 总结性发布）。</p>
        ) : (
          covered.map((item) => (
            <button
              key={item.id}
              type="button"
              className="modal-news-row"
              onClick={() => onSelect(item.id)}
            >
              <div className="modal-news-meta">
                <span>{cleanSourceLabel(item.source)}</span>
                <time>{formatDate(item.published)}</time>
              </div>
              <strong>{item.title}</strong>
              {item.summary && <p>{item.summary.slice(0, 120)}</p>}
            </button>
          ))
        )}
      </div>
    </>
  );
}

function NewsDetailView({
  news,
  article,
  loading,
  onBack,
}: {
  news: NewsItem;
  article: ArticleContent | null;
  loading: boolean;
  onBack: () => void;
}) {
  const body = article?.paragraphs.filter(Boolean).join("\n") ?? news.summary ?? "";
  return (
    <>
      <div className="modal-side-head">
        <button className="modal-back" type="button" onClick={onBack}>
          ← 返回列表
        </button>
        {news.link && (
          <button
            type="button"
            className="modal-external"
            onClick={() => void invoke("open_external_url", { url: news.link! }).catch(() => undefined)}
            title="在浏览器打开原文"
          >
            <ExternalLink size={13} />
          </button>
        )}
      </div>
      <div className="modal-news-detail">
        <h4>{news.title}</h4>
        <div className="modal-news-detail-meta">
          <span>{cleanSourceLabel(news.source)}</span>
          {news.published && <time>{formatDate(news.published)}</time>}
        </div>
        {loading ? (
          <div className="modal-news-loading">
            <Loader2 className="spin" size={14} /> 正在抓取原文…
          </div>
        ) : body ? (
          <p>{body}</p>
        ) : (
          <p className="muted">未能抓取到正文，可点右上角图标在浏览器打开。</p>
        )}
      </div>
    </>
  );
}

function cleanSourceLabel(source: string) {
  return source.replace(/^RSS\s*备用[：:]\s*/, "").trim() || source;
}

// ---------- 消息内的图片附件 ----------
//
// chat_message.contentJson.images 存的是后端落盘后的绝对路径（在 app_data_dir/chat-images/）。
// 用 Tauri 的 convertFileSrc 把绝对路径转成 webview 能加载的 asset:// URL；
// CSP 已经允许 img-src 'self' asset: ...，tauri.conf.json assetProtocol.scope 限制只能读 chat-images/。
//
// 点缩略图打开 lightbox 看大图——chat 流里图都是缩略，原图在 modal 里看。

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
