import { invoke } from "@tauri-apps/api/core";
import {
  Check,
  ChevronDown,
  ChevronRight,
  Loader2,
  Plus,
  Save,
  Trash2,
  X as XIcon,
} from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";

// ============================================================================
// AgentConfig payload — 与后端 AgentConfig 一一对应
// ============================================================================

type ProviderKind = "anthropic" | "openai_responses" | "openai_chat_completions";
type ReasoningEffort = "minimal" | "low" | "medium" | "high";

const WIRE_FORMAT_LABELS: Record<ProviderKind, string> = {
  anthropic: "Anthropic",
  openai_responses: "OpenAI Responses",
  openai_chat_completions: "OpenAI Chat",
};

type ThinkingMode = "adaptive" | "enabled" | "disabled";
type ThinkingDisplay = "summarized" | "omitted";
type EffortLevel = "low" | "medium" | "high" | "xhigh" | "max";

type Channel = {
  id: string;
  name: string;
  wireFormat: ProviderKind;
  baseUrl: string;
  /** 后端 read 时返回 mask 占位（前 8 字符 + 中文省略号 + 长度）。
   *  后端 write 时若收到的值与 mask 一致或为空则保留已存 token。 */
  token: string;
  availableModels: string[];
  enableNativeWebSearch: boolean;
  /** Anthropic thinking 模式。adaptive=推荐 4.6+；enabled=老模型 manual budget；disabled=关闭。
   *  Haiku 上即使设了也会被 wire format 层 drop。 */
  thinkingMode: ThinkingMode;
  /** 仅 thinkingMode=enabled 时使用 */
  thinkingBudgetTokens: number;
  /** thinkingMode=adaptive 时 thinking 文本是否回流 UI。默认 summarized */
  thinkingDisplay: ThinkingDisplay;
  /** Anthropic effort（4.6+ 模型识别），独立于 thinking 也影响 tool call 数量 */
  defaultEffort?: EffortLevel | null;
  /** OpenAI 通道 reasoning effort（gpt-5/o3 系列识别）—— 和 Anthropic effort 是不同 API */
  reasoningEffort?: ReasoningEffort | null;
  enableWebSearch: boolean;
};

type ModelRef = { channelId: string; model: string };

type AgentConfigPayload = {
  channels: Channel[];
  assignments: {
    chat: ModelRef;
    compact: ModelRef;
  };
  agent: {
    maxTurnsPerRun: number;
    maxSearchCallsPerRun: number;
    contextSoftLimitTokens: number;
    contextHardLimitTokens: number;
    compactKeepLastNTurns: number;
    toolTimeoutSecs?: number;
    contextSummarizeThreshold?: number;
    summarizeMaxConsecutiveFailures?: number;
  };
};

type SlotKey = "chat" | "compact";
const SLOT_KEYS: SlotKey[] = ["chat", "compact"];
const SLOT_LABELS: Record<SlotKey, { title: string; hint: string }> = {
  chat: { title: "Chat 模型", hint: "Agent 主模型——对话 / reflection 都用它" },
  compact: { title: "Compact 模型", hint: "chat 上下文压缩用，一般选便宜款" },
};

type VerifyState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ok" }
  | { kind: "err"; message: string };

/** verify 状态键：`${channelId}::${model}` —— 同一 model 在多个渠道里要分开记。 */
const verifyKey = (channelId: string, model: string) => `${channelId}::${model}`;

// ============================================================================
// SettingsPage 主组件
// ============================================================================

type Props = {
  autoRefresh: boolean;
  databasePath: string | null;
  refreshInterval: number;
  setAutoRefresh: (value: boolean) => void;
  setRefreshInterval: (value: number) => void;
};

export function SettingsPage({
  autoRefresh,
  databasePath,
  refreshInterval,
  setAutoRefresh,
  setRefreshInterval,
}: Props) {
  return (
    <section className="page-shell settings-page">
      <header className="settings-head">
        <h2>设置</h2>
        <p>渠道管理、模型分配、Agent 节奏。</p>
      </header>

      <ChannelsAndAssignmentsBlock />

      <AgentBudgetBlock />

      <div className="settings-section">
        <div className="settings-section-head">
          <h3>资讯</h3>
          <p>NewsNow 财经源的拉取节奏。</p>
        </div>
        <div className="settings-rows">
          <Row title="自动刷新" hint="关闭后只在手动点击「刷新资讯」时才拉取">
            <Switch checked={autoRefresh} onChange={setAutoRefresh} />
          </Row>
          <Row title="刷新间隔" hint="自动刷新开启时生效；盘外自动 ×5 减少调用">
            <SegmentedControl
              value={refreshInterval}
              onChange={setRefreshInterval}
              options={[
                { label: "15 秒", value: 15000 },
                { label: "30 秒", value: 30000 },
                { label: "1 分钟", value: 60000 },
                { label: "5 分钟", value: 300000 },
              ]}
            />
          </Row>
        </div>
      </div>

      <DataSourceBlock />

      <NetworkBlock />

      <AgentHealthBlock />

      <div className="settings-section">
        <div className="settings-section-head">
          <h3>系统</h3>
          <p>运行环境信息（只读）。</p>
        </div>
        <div className="settings-rows">
          <Row title="本地数据库" hint="SQLite，全部学习记录与对话存储于此">
            <span className="settings-readonly settings-readonly-mono">{databasePath || "正在初始化…"}</span>
          </Row>
          <Row title="风险边界" hint="所有交易假设仅写入模拟账户">
            <span className="settings-readonly">仅做学习型分析，不接券商</span>
          </Row>
          <Row title="立即触发 reflection" hint="平常每交易日 15:30 自动触发一次；这里强制立即跑">
            <TriggerReflectionButton />
          </Row>
        </div>
      </div>
    </section>
  );
}

// ============================================================================
// Agent Health：自迭代健康度面板
//
// 直接对应 docs/architecture.md 的"5 秒确认自迭代在工作"几条审计 SQL。每个卡
// 片只盯一个关键信号——红色阈值是"环已经断了或没在跑"，黄色是"应注意"。
// ============================================================================

type HeartbeatRow = {
  loopName: string;
  lastOkAt: string | null;
  lastErrAt: string | null;
  lastErrMsg: string | null;
  consecutiveErr: number;
  updatedAt: string;
};

type AgentHealthDto = {
  expectationCompletenessRate: number | null;
  totalExpectations: number;
  totalClosedExpectations: number;
  reflectionEpisodeCount7d: number;
  scanTickCount7d: number;
  lessonsCount7d: number;
  heuristicCounts: { seed: number; userStated: number; agentInferred: number; retired: number };
  heuristicOriginShare: { seed: number; userStated: number; agentInferred: number; agentInferredShare: number | null };
  scanTicksToday: number;
  expectationsCreatedToday: number;
  lessonsCreatedToday: number;
  lessonsEmptyTakeaway7d: number;
  heuristicsEmerged7d: number;
  heartbeats: HeartbeatRow[];
};

function AgentHealthBlock() {
  const [data, setData] = useState<AgentHealthDto | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const refresh = async () => {
    setLoading(true);
    try {
      const res = await invoke<AgentHealthDto>("get_agent_health");
      setData(res);
      setErr(null);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void refresh();
  }, []);

  return (
    <div className="settings-section">
      <div className="settings-section-head">
        <h3>Agent 自迭代健康度</h3>
        <p>每项对应一条审计 SQL；点击「刷新」重拉。红字 = 环断了或 loop 卡死，橙字 = 需关注。</p>
      </div>
      <div className="settings-rows">
        <Row title="刷新" hint="拉取最新指标 + loop 心跳">
          <button
            type="button"
            className="settings-save-btn"
            onClick={() => void refresh()}
            disabled={loading}
          >
            {loading ? "刷新中…" : "刷新"}
          </button>
        </Row>
        {err && (
          <Row title="错误" hint="后端 get_agent_health 返回错误">
            <span className="settings-readonly health-stat-red">{err}</span>
          </Row>
        )}
        {data && (
          <>
            <HealthRow
              title="今日 scan ticks"
              hint="9 个时刻表，盘中应 ≥ 6；周末 / 盘外为 0"
              value={data.scanTicksToday}
              level={pickLevel(data.scanTicksToday, { red: -1, yellow: 3 })}
            />
            <HealthRow
              title="今日新建 expectation"
              hint="连续多日为 0 → agent 没在产出新预期"
              value={data.expectationsCreatedToday}
              level={pickLevel(data.expectationsCreatedToday, { red: -1, yellow: 0 })}
            />
            <HealthRow
              title="今日 lesson"
              hint="reflection 复盘产物——交易日盘后应有"
              value={data.lessonsCreatedToday}
            />
            <HealthRow
              title="近 7 天空 takeaway lesson"
              hint="> 0 说明 LLM provider 没接通 / takeaway fill 失败——自迭代环断点"
              value={data.lessonsEmptyTakeaway7d}
              level={data.lessonsEmptyTakeaway7d > 0 ? "red" : "ok"}
            />
            <HealthRow
              title="近 7 天新 emerge heuristic"
              hint="连续 2 周为 0 → emerge 链路死了"
              value={data.heuristicsEmerged7d}
              level={pickLevel(data.heuristicsEmerged7d, { red: -1, yellow: 0 })}
            />
            <HealthRow
              title="Heuristic 原创占比"
              hint={`agent ${data.heuristicOriginShare.agentInferred} · user ${data.heuristicOriginShare.userStated} · seed ${data.heuristicOriginShare.seed}`}
              value={
                data.heuristicOriginShare.agentInferredShare != null
                  ? `${Math.round(data.heuristicOriginShare.agentInferredShare * 100)}%`
                  : "—"
              }
            />
            <Row title="后台 loop 心跳" hint="每个 loop 的上次成功 / 连续失败计数">
              <div className="heartbeat-list">
                {data.heartbeats.length === 0 ? (
                  <span className="settings-readonly">暂无——loop 还没跑过 / schema 刚建</span>
                ) : (
                  data.heartbeats.map((h) => {
                    const isStale =
                      h.consecutiveErr >= 3 || (h.lastOkAt == null && h.lastErrAt != null);
                    return (
                      <div key={h.loopName} className="heartbeat-row">
                        <span className="heartbeat-name">{h.loopName}</span>
                        <span className={`heartbeat-ok ${isStale ? "health-stat-red" : ""}`}>
                          {relativeTime(h.lastOkAt)}
                        </span>
                        <span
                          className={`heartbeat-err-count ${
                            h.consecutiveErr > 0 ? "health-stat-red" : ""
                          }`}
                        >
                          {h.consecutiveErr > 0 ? `${h.consecutiveErr}× 连失` : "—"}
                        </span>
                        <span className="heartbeat-msg" title={h.lastErrMsg ?? ""}>
                          {h.lastErrMsg ?? "—"}
                        </span>
                      </div>
                    );
                  })
                )}
              </div>
            </Row>
          </>
        )}
      </div>
    </div>
  );
}

type HealthLevel = "ok" | "yellow" | "red";

/// value 落 (yellow, +∞] = ok；(red, yellow] = yellow；≤ red = red。
/// 用 -1 做"任何 0 都算 yellow / red"的语义。
function pickLevel(value: number, thresholds: { red: number; yellow: number }): HealthLevel {
  if (value <= thresholds.red) return "red";
  if (value <= thresholds.yellow) return "yellow";
  return "ok";
}

function HealthRow({
  title,
  hint,
  value,
  level = "ok",
}: {
  title: string;
  hint: string;
  value: number | string;
  level?: HealthLevel;
}) {
  const cls =
    level === "red"
      ? "settings-readonly health-stat health-stat-red"
      : level === "yellow"
        ? "settings-readonly health-stat health-stat-yellow"
        : "settings-readonly health-stat";
  return (
    <Row title={title} hint={hint}>
      <span className={cls}>{value}</span>
    </Row>
  );
}

function relativeTime(iso: string | null): string {
  if (!iso) return "—";
  const t = Date.parse(iso);
  if (!Number.isFinite(t)) return iso;
  const deltaSec = Math.floor((Date.now() - t) / 1000);
  if (deltaSec < 60) return `${deltaSec}s 前`;
  if (deltaSec < 3600) return `${Math.floor(deltaSec / 60)}min 前`;
  if (deltaSec < 86400) return `${Math.floor(deltaSec / 3600)}h 前`;
  return `${Math.floor(deltaSec / 86400)}d 前`;
}

function TriggerReflectionButton() {
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  return (
    <div>
      <button
        disabled={busy}
        onClick={() => {
          setBusy(true);
          setMsg("running…");
          invoke<{ runId: string; outcomeSummary: string; thesisCount: number }>(
            "trigger_reflection_now",
          )
            .then((r) => setMsg(`完成（${r.thesisCount} 个 thesis 被复盘）`))
            .catch((e) => setMsg(`失败: ${e}`))
            .finally(() => setBusy(false));
        }}
        style={{ padding: "4px 12px" }}
      >
        立即跑一次
      </button>
      {msg && (
        <span style={{ marginLeft: 12, fontSize: 12, color: "#64748b" }}>{msg}</span>
      )}
    </div>
  );
}

// ============================================================================
// 渠道管理 + 模型分配（两块各自独立保存）
// ============================================================================

// ============================================================================
// Agent 行为预算——上下文 / turn / web 搜索次数等运行时上限
//
// 这些之前都是 hard-coded 默认值，没 UI 调。结果用户灌一篇大文章 + agent 拉了
// 几十条 quote 就轻松撞 hard_limit_tokens=160k 报错。提到 UI 让用户能调，且
// 默认值已经升到 190k（Claude 4.x 都 200k context）。
// ============================================================================

function AgentBudgetBlock() {
  const [agent, setAgent] = useState<AgentConfigPayload["agent"] | null>(null);
  const [saving, setSaving] = useState(false);
  const [feedback, setFeedback] = useState<string | null>(null);

  const refresh = async () => {
    const cfg = await invoke<AgentConfigPayload>("get_agent_config").catch(() => null);
    setAgent(cfg?.agent ?? null);
  };

  useEffect(() => {
    void refresh();
  }, []);

  const updateField = <K extends keyof AgentConfigPayload["agent"]>(
    key: K,
    value: AgentConfigPayload["agent"][K],
  ) => {
    setAgent((prev) => (prev ? { ...prev, [key]: value } : prev));
    setFeedback(null);
  };

  const save = async () => {
    if (!agent) return;
    setSaving(true);
    setFeedback(null);
    try {
      const cfg = await invoke<AgentConfigPayload>("get_agent_config");
      const merged: AgentConfigPayload = { ...cfg, agent };
      await invoke("set_agent_config", { config: merged });
      setFeedback("已保存");
      window.setTimeout(() => setFeedback(null), 1800);
    } catch (e) {
      setFeedback(`保存失败：${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="settings-section">
      <div className="settings-section-head">
        <h3>Agent 行为预算</h3>
        <p>
          上下文窗口、单次 run 的轮数 / 搜索次数。默认值适合 Claude 4.x / GPT-5（200k 窗口）；
          换更小的模型时调低，免得撞模型自身上限。
        </p>
      </div>
      <div className="settings-rows">
        {!agent ? (
          <Row title="加载中" hint="">
            <span className="settings-readonly">读取 agent 配置…</span>
          </Row>
        ) : (
          <>
            <Row
              title="上下文 hard 限制"
              hint="超过此值压缩仍不够时 → agent 终止报错。建议留 ~10k 给模型响应。Claude 4.x / GPT-5 200k context → 设 190k"
            >
              <NumberInput
                value={agent.contextHardLimitTokens}
                min={20000}
                max={1000000}
                step={10000}
                suffix="tokens"
                onChange={(v) => updateField("contextHardLimitTokens", v)}
              />
            </Row>
            <Row
              title="上下文 soft 限制"
              hint="超过此值会触发 compact 工具压缩历史（保留最近 N 轮 + 摘要中段）。一般设 hard 的 70%"
            >
              <NumberInput
                value={agent.contextSoftLimitTokens}
                min={10000}
                max={agent.contextHardLimitTokens}
                step={10000}
                suffix="tokens"
                onChange={(v) => updateField("contextSoftLimitTokens", v)}
              />
            </Row>
            <Row
              title="单次 run 最大轮数"
              hint="一条用户消息触发 agent 最多 turn 数（每 turn = 一次 LLM 调用 + 工具调用）。过高浪费 token，过低 agent 跑不完任务"
            >
              <NumberInput
                value={agent.maxTurnsPerRun}
                min={3}
                max={50}
                step={1}
                onChange={(v) => updateField("maxTurnsPerRun", v)}
              />
            </Row>
            <Row
              title="单次 run 最大 web 搜索次数"
              hint="模型原生 web_search 次数上限（开关在每个渠道里）。每次搜索是真金白银"
            >
              <NumberInput
                value={agent.maxSearchCallsPerRun}
                min={0}
                max={20}
                step={1}
                onChange={(v) => updateField("maxSearchCallsPerRun", v)}
              />
            </Row>
            <Row
              title="工具调用超时"
              hint="单个本地工具（quote/news/account 等）超过此秒数视为失败"
            >
              <NumberInput
                value={agent.toolTimeoutSecs ?? 30}
                min={5}
                max={180}
                step={5}
                suffix="秒"
                onChange={(v) => updateField("toolTimeoutSecs", v)}
              />
            </Row>
            <Row title="" hint="">
              <button
                type="button"
                className="settings-save-btn"
                disabled={saving}
                onClick={() => void save()}
              >
                {saving ? <Loader2 size={14} className="spin" /> : <Save size={14} />}
                保存预算
              </button>
              {feedback && <span className="settings-feedback">{feedback}</span>}
            </Row>
          </>
        )}
      </div>
    </div>
  );
}

function ChannelsAndAssignmentsBlock() {
  const [config, setConfig] = useState<AgentConfigPayload | null>(null);
  const [draft, setDraft] = useState<AgentConfigPayload | null>(null);
  // 哪些渠道当前展开（默认折叠以省地方；点 chevron 切换）
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  // verify 状态：key = `${channelId}::${model}`，可独立追踪同名模型在不同渠道的状态
  const [verifyMap, setVerifyMap] = useState<Record<string, VerifyState>>({});
  // 已 verify 通过、但还没"保存渠道"持久化的 model：按 channelId 暂存，列表里以"待生效"形态展示。
  // 显式 save 时才合并进 draft.availableModels 并写库——避免点了"添加"就立刻进清单的体验。
  const [pendingModels, setPendingModels] = useState<Record<string, string[]>>({});
  // 保存按钮的 in-flight + feedback
  const [savingChannelIds, setSavingChannelIds] = useState<Set<string>>(new Set());
  const [channelFeedback, setChannelFeedback] = useState<Record<string, string>>({});
  const [savingAssignments, setSavingAssignments] = useState(false);
  const [assignmentFeedback, setAssignmentFeedback] = useState<string | null>(null);
  // 「新建渠道」inline form 的 state
  const [newChannelName, setNewChannelName] = useState("");
  const [newChannelWire, setNewChannelWire] = useState<ProviderKind>("anthropic");

  const refresh = async () => {
    const next = await invoke<AgentConfigPayload>("get_agent_config").catch(() => null);
    setConfig(next);
    setDraft(next);
    setVerifyMap({});
    setPendingModels({});
    setChannelFeedback({});
    setAssignmentFeedback(null);
    // 默认展开第一个渠道（如果有），让用户立刻看到它
    if (next && next.channels.length > 0 && next.channels.length <= 2) {
      setExpanded(new Set(next.channels.map((c) => c.id)));
    }
  };

  useEffect(() => {
    void refresh();
  }, []);

  /** 模型分配下拉的扁平选项：所有渠道 × 各自 availableModels。
   *  label 形如 "[渠道名] modelId" 用于消歧。value 用 `${channelId}::${model}`。
   *
   *  **重要**：这个 useMemo 必须在 `if (!draft) return ...` 早返回之前——React 要求
   *  hooks 在每次 render 调用顺序一致；放在 early return 之后会让初次渲染（draft=null）
   *  缺一个 hook，第二次渲染（draft=value）多一个 hook，触发"Rendered more hooks than
   *  during the previous render"错误，整个 App 卡白屏。 */
  const allModelOptions = useMemo(() => {
    if (!draft) return [];
    const opts: Array<{ value: string; label: string; ref: ModelRef }> = [];
    for (const chan of draft.channels) {
      for (const m of chan.availableModels) {
        opts.push({
          value: verifyKey(chan.id, m),
          label: `[${chan.name}] ${m}`,
          ref: { channelId: chan.id, model: m },
        });
      }
    }
    return opts;
  }, [draft]);

  if (!draft) {
    return (
      <div className="settings-section">
        <div className="settings-section-head">
          <h3>AI 配置</h3>
          <p>加载中…</p>
        </div>
      </div>
    );
  }

  const updateDraft = (updater: (d: AgentConfigPayload) => AgentConfigPayload) =>
    setDraft((d) => (d ? updater(d) : d));

  const updateChannel = (id: string, patch: Partial<Channel>) =>
    updateDraft((d) => ({
      ...d,
      channels: d.channels.map((c) => (c.id === id ? { ...c, ...patch } : c)),
    }));

  const updateChannelClearVerify = (id: string, patch: Partial<Channel>) => {
    updateChannel(id, patch);
    // base_url / token 改了 → 这个渠道下所有 model 的 verify 状态失效；
    // 待生效的 pending 模型也是基于旧 token/url 验过的，一并丢弃强制用户重 verify
    setVerifyMap((m) => {
      const next: typeof m = {};
      for (const [k, v] of Object.entries(m)) {
        if (!k.startsWith(`${id}::`)) next[k] = v;
      }
      return next;
    });
    setPendingModels((p) => {
      if (!p[id] || p[id].length === 0) return p;
      const next = { ...p };
      delete next[id];
      return next;
    });
  };

  /** 后端 verify_provider_model 调用——成功设 ok，失败设 err 带原始文本。 */
  const verifyModel = async (channel: Channel, model: string): Promise<boolean> => {
    const key = verifyKey(channel.id, model);
    setVerifyMap((m) => ({ ...m, [key]: { kind: "loading" } }));
    try {
      await invoke("verify_provider_model", {
        channelId: channel.id,
        baseUrl: channel.baseUrl,
        token: channel.token,
        model,
      });
      setVerifyMap((m) => ({ ...m, [key]: { kind: "ok" } }));
      return true;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setVerifyMap((m) => ({ ...m, [key]: { kind: "err", message } }));
      return false;
    }
  };

  /** 删除渠道——同时把任何指向该渠道的 assignment 清空。 */
  const removeChannel = (id: string) => {
    if (!confirm(`确认删除该渠道？引用此渠道的模型分配会被清空。`)) return;
    updateDraft((d) => {
      const cleared = (r: ModelRef): ModelRef =>
        r.channelId === id ? { channelId: "", model: "" } : r;
      return {
        ...d,
        channels: d.channels.filter((c) => c.id !== id),
        assignments: {
          chat: cleared(d.assignments.chat),
          compact: cleared(d.assignments.compact),
        },
      };
    });
    setExpanded((s) => {
      const n = new Set(s);
      n.delete(id);
      return n;
    });
    setPendingModels((p) => {
      if (!(id in p)) return p;
      const next = { ...p };
      delete next[id];
      return next;
    });
  };

  /** 添加新渠道——前端生成 uuid，立刻进入展开编辑。 */
  const addChannel = () => {
    const name = newChannelName.trim();
    if (!name) return;
    // 简易 uuid——36 字符够用，与后端 uuid v4 兼容
    const id = `ch_${Math.random().toString(36).slice(2, 10)}_${Date.now().toString(36)}`;
    const fresh: Channel = {
      id,
      name,
      wireFormat: newChannelWire,
      baseUrl: "",
      token: "",
      availableModels: [],
      enableNativeWebSearch: false,
      thinkingMode: "adaptive",
      thinkingBudgetTokens: 4000,
      thinkingDisplay: "summarized",
      defaultEffort: null,
      reasoningEffort: null,
      enableWebSearch: false,
    };
    updateDraft((d) => ({ ...d, channels: [...d.channels, fresh] }));
    setExpanded((s) => new Set(s).add(id));
    setNewChannelName("");
  };

  /** 保存某个渠道——把 pendingModels[id] 合并进 availableModels，再 verify 全部，最后写库。
   *  pending 模型在 addModelToChannel 时已 verify 过一次，但这里仍然走 verify 一遍以保证
   *  当前 base_url/token 下确实可用（用户可能在 add 后又改过 URL 又点了 save）。 */
  const saveChannel = async (id: string) => {
    if (!draft || !config) return;
    const chan = draft.channels.find((c) => c.id === id);
    if (!chan) return;
    setSavingChannelIds((s) => new Set(s).add(id));
    setChannelFeedback((f) => ({ ...f, [id]: "校验可用模型…" }));
    try {
      // 合并 pending 进 effectiveModels——这是真正要写库的清单
      const pending = pendingModels[id] ?? [];
      const effectiveModels = [...chan.availableModels, ...pending];
      const channelToSave: Channel = { ...chan, availableModels: effectiveModels };

      if (effectiveModels.length > 0) {
        // 后端 verify 需要 stored 里能查到 channel（含最新 base_url/token）。
        // 新建未保存 或 已存在但字段改了 → 先把 channelToSave 写库一遍，
        // 再 verify。verify 通过后会再写一遍以确认；失败也无害——
        // availableModels 多一条 pending 不影响功能。
        const isNew = !config.channels.some((c) => c.id === id);
        const stored = isNew ? null : config.channels.find((c) => c.id === id);
        const channelFieldsChanged = stored
          ? stored.baseUrl !== channelToSave.baseUrl || stored.token !== channelToSave.token
          : true;
        if (isNew || channelFieldsChanged) {
          const merged = mergeOneChannel(config, channelToSave);
          await invoke("set_agent_config", { config: merged });
          await refresh();
        }
        const results = await Promise.all(
          effectiveModels.map((m) => verifyModel(channelToSave, m)),
        );
        const failed = effectiveModels.filter((_, i) => !results[i]);
        if (failed.length > 0) {
          setChannelFeedback((f) => ({
            ...f,
            [id]: `校验未通过：${failed.join(", ")}（点对应「验证」看错误，或先移除再保存）`,
          }));
          return;
        }
      }
      // verify 通过 → 把 pending 合并进 draft.availableModels 并写库
      updateChannel(id, { availableModels: effectiveModels });
      const merged = mergeOneChannel(config, channelToSave);
      await invoke("set_agent_config", { config: merged });
      setPendingModels((p) => {
        if (!(id in p)) return p;
        const next = { ...p };
        delete next[id];
        return next;
      });
      setChannelFeedback((f) => ({ ...f, [id]: "已保存" }));
      await refresh();
    } catch (err) {
      setChannelFeedback((f) => ({
        ...f,
        [id]: `保存失败：${err instanceof Error ? err.message : String(err)}`,
      }));
    } finally {
      setSavingChannelIds((s) => {
        const n = new Set(s);
        n.delete(id);
        return n;
      });
    }
  };

  /** 添加 model 到某渠道——先 verify，通过后放入 pendingModels[id]（不立刻入清单）。
   *  待用户点"保存渠道"时 saveChannel 会把 pending 合并到 availableModels 并写库。 */
  const addModelToChannel = async (id: string, model: string): Promise<string | null> => {
    if (!draft) return "draft 未加载";
    const chan = draft.channels.find((c) => c.id === id);
    if (!chan) return "渠道不存在";
    const trimmed = model.trim();
    if (!trimmed) return "模型 ID 为空";
    if (chan.availableModels.includes(trimmed)) return "已在清单里";
    if ((pendingModels[id] ?? []).includes(trimmed)) return "已在待生效列表里";
    // 先把渠道写库（如果是新建未保存的）让后端能找到——verify 后端通过 channelId 查 stored
    if (config && !config.channels.some((c) => c.id === id)) {
      try {
        const merged = mergeOneChannel(config, chan);
        await invoke("set_agent_config", { config: merged });
        await refresh();
      } catch (err) {
        return `保存渠道失败：${err instanceof Error ? err.message : String(err)}`;
      }
    }
    // verify
    const ok = await verifyModel(chan, trimmed);
    if (!ok) {
      const state = verifyMap[verifyKey(id, trimmed)];
      return state?.kind === "err" ? `验证失败：${state.message}` : "验证失败";
    }
    // 入待生效暂存——不动 draft.availableModels
    setPendingModels((p) => ({
      ...p,
      [id]: [...(p[id] ?? []), trimmed],
    }));
    return null;
  };

  /** 从待生效列表移除（用户没保存就想撤销）——只清 pending 和 verify 状态。 */
  const removePendingModel = (id: string, model: string) => {
    setPendingModels((p) => {
      const list = (p[id] ?? []).filter((m) => m !== model);
      if (list.length === (p[id]?.length ?? 0)) return p;
      const next = { ...p };
      if (list.length === 0) delete next[id];
      else next[id] = list;
      return next;
    });
    setVerifyMap((m) => {
      const key = verifyKey(id, model);
      if (!(key in m)) return m;
      const next = { ...m };
      delete next[key];
      return next;
    });
  };

  /** 移除模型——同时把指向 (id, model) 的 assignments 清空。 */
  const removeModelFromChannel = (id: string, model: string) => {
    updateDraft((d) => {
      const cleared = (r: ModelRef): ModelRef =>
        r.channelId === id && r.model === model ? { channelId: "", model: "" } : r;
      return {
        ...d,
        channels: d.channels.map((c) =>
          c.id === id ? { ...c, availableModels: c.availableModels.filter((m) => m !== model) } : c,
        ),
        assignments: {
          chat: cleared(d.assignments.chat),
          compact: cleared(d.assignments.compact),
        },
      };
    });
    setVerifyMap((m) => {
      const next = { ...m };
      delete next[verifyKey(id, model)];
      return next;
    });
  };

  const setAssignment = (slot: SlotKey, ref: ModelRef) =>
    updateDraft((d) => ({ ...d, assignments: { ...d.assignments, [slot]: ref } }));

  /** 保存模型分配——只写 assignments 字段，channels 保持 stored 值。 */
  const saveAssignments = async () => {
    if (!draft || !config) return;
    setSavingAssignments(true);
    setAssignmentFeedback(null);
    try {
      // 校验：每个 slot 必须指向真实存在的 (channel, model)
      const refsValid = SLOT_KEYS.every((s) => {
        const r = draft.assignments[s];
        if (!r.channelId || !r.model) return false;
        const chan = draft.channels.find((c) => c.id === r.channelId);
        return !!chan && chan.availableModels.includes(r.model);
      });
      if (!refsValid) {
        setAssignmentFeedback("有 pipeline 未分配或指向不存在的 (渠道, 模型) 组合");
        return;
      }
      const merged: AgentConfigPayload = {
        ...config,
        assignments: draft.assignments,
      };
      await invoke("set_agent_config", { config: merged });
      setAssignmentFeedback("已保存");
      await refresh();
    } catch (err) {
      setAssignmentFeedback(`保存失败：${err instanceof Error ? err.message : String(err)}`);
    } finally {
      setSavingAssignments(false);
    }
  };

  // ----- 渲染 -----
  return (
    <>
      <div className="settings-section">
        <div className="settings-section-head">
          <h3>渠道管理</h3>
          <p>
            一个渠道 = (wire format + base_url + token + 该渠道下的可用模型)。
            可以加任意多个相同 wire format 的渠道（不同 base_url），每个独立保存。
          </p>
        </div>
        <div className="settings-rows">
          {draft.channels.length === 0 && (
            <Row title="暂无渠道" hint="">
              <span className="settings-readonly">下面新建一个渠道开始</span>
            </Row>
          )}
          {draft.channels.map((chan) => (
            <ChannelCard
              key={chan.id}
              channel={chan}
              expanded={expanded.has(chan.id)}
              onToggleExpand={() =>
                setExpanded((s) => {
                  const n = new Set(s);
                  if (n.has(chan.id)) n.delete(chan.id);
                  else n.add(chan.id);
                  return n;
                })
              }
              onUpdate={(patch) => updateChannel(chan.id, patch)}
              onUpdateClearVerify={(patch) => updateChannelClearVerify(chan.id, patch)}
              onRemove={() => removeChannel(chan.id)}
              onAddModel={(m) => addModelToChannel(chan.id, m)}
              onRemoveModel={(m) => removeModelFromChannel(chan.id, m)}
              onVerifyModel={(m) => verifyModel(chan, m)}
              verifyMap={verifyMap}
              pendingModels={pendingModels[chan.id] ?? []}
              onRemovePending={(m) => removePendingModel(chan.id, m)}
              isSaving={savingChannelIds.has(chan.id)}
              feedback={channelFeedback[chan.id] ?? null}
              onSave={() => saveChannel(chan.id)}
              isDirty={
                (pendingModels[chan.id]?.length ?? 0) > 0 ||
                !config ||
                JSON.stringify(config.channels.find((c) => c.id === chan.id)) !== JSON.stringify(chan)
              }
              isNew={!config?.channels.some((c) => c.id === chan.id)}
            />
          ))}
          <div className="settings-row available-model-row">
            <div className="settings-row-label">
              <strong>新建渠道</strong>
              <span>起个名字（如"DeepSeek 个人"）+ 选 wire format → 添加</span>
            </div>
            <div className="settings-row-control add-model-row">
              <input
                className="settings-input"
                type="text"
                value={newChannelName}
                onChange={(e) => setNewChannelName(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") addChannel();
                }}
                placeholder="渠道名称"
                spellCheck={false}
                autoComplete="off"
              />
              <select
                className="settings-input"
                value={newChannelWire}
                onChange={(e) => setNewChannelWire(e.target.value as ProviderKind)}
              >
                <option value="anthropic">Anthropic</option>
                <option value="openai_responses">OpenAI Responses</option>
                <option value="openai_chat_completions">OpenAI Chat</option>
              </select>
              <button
                type="button"
                className="model-verify-btn"
                onClick={addChannel}
                disabled={!newChannelName.trim()}
              >
                <Plus size={14} />
                <span>新建</span>
              </button>
            </div>
          </div>
        </div>
      </div>

      <div className="settings-section">
        <div className="settings-section-head">
          <h3>模型分配</h3>
          <p>
            每条 pipeline 用哪个 (渠道, 模型)——下拉列出所有渠道的可用模型，
            label 形如 [渠道名] modelId 区分。
          </p>
        </div>
        <div className="settings-rows">
          {SLOT_KEYS.map((slot) => {
            const cur = draft.assignments[slot];
            const curValue = cur.channelId && cur.model ? verifyKey(cur.channelId, cur.model) : "";
            // 当前值在选项里就 OK；否则 prepend 一个 ⚠ 选项保留显示
            const curMissing = !!curValue && !allModelOptions.some((o) => o.value === curValue);
            return (
              <Row title={SLOT_LABELS[slot].title} hint={SLOT_LABELS[slot].hint} key={slot}>
                <select
                  className="settings-input"
                  value={curValue}
                  onChange={(e) => {
                    const opt = allModelOptions.find((o) => o.value === e.target.value);
                    setAssignment(slot, opt?.ref ?? { channelId: "", model: "" });
                  }}
                  disabled={allModelOptions.length === 0}
                >
                  {!curValue && (
                    <option value="" disabled>
                      {allModelOptions.length === 0
                        ? "（请先在上面新建渠道并添加模型）"
                        : "请选择…"}
                    </option>
                  )}
                  {curMissing && (
                    <option value={curValue}>
                      ⚠ {cur.channelId} / {cur.model}（已不存在）
                    </option>
                  )}
                  {allModelOptions.map((o) => (
                    <option key={o.value} value={o.value}>
                      {o.label}
                    </option>
                  ))}
                </select>
              </Row>
            );
          })}
          <Row title="" hint="">
            <button
              type="button"
              className="settings-save-btn"
              disabled={savingAssignments}
              onClick={saveAssignments}
            >
              {savingAssignments ? <Loader2 size={14} className="spin" /> : <Save size={14} />}
              保存模型分配
            </button>
            {assignmentFeedback && <span className="settings-feedback">{assignmentFeedback}</span>}
          </Row>
        </div>
      </div>
    </>
  );
}

/** 把单个 channel patch 进 stored config——保留其他渠道和 assignments 不变。
 *  新渠道（不在 stored.channels 里）→ 追加；已存在 → 替换。 */
function mergeOneChannel(stored: AgentConfigPayload, chan: Channel): AgentConfigPayload {
  const exists = stored.channels.some((c) => c.id === chan.id);
  return {
    ...stored,
    channels: exists
      ? stored.channels.map((c) => (c.id === chan.id ? chan : c))
      : [...stored.channels, chan],
  };
}

// ============================================================================
// 单个渠道卡片：展开后可改 URL/token/开关 + 管理可用模型 + 保存
// ============================================================================

function ChannelCard({
  channel,
  expanded,
  onToggleExpand,
  onUpdate,
  onUpdateClearVerify,
  onRemove,
  onAddModel,
  onRemoveModel,
  onVerifyModel,
  verifyMap,
  pendingModels,
  onRemovePending,
  isSaving,
  feedback,
  onSave,
  isDirty,
  isNew,
}: {
  channel: Channel;
  expanded: boolean;
  onToggleExpand: () => void;
  onUpdate: (patch: Partial<Channel>) => void;
  onUpdateClearVerify: (patch: Partial<Channel>) => void;
  onRemove: () => void;
  onAddModel: (model: string) => Promise<string | null>;
  onRemoveModel: (model: string) => void;
  onVerifyModel: (model: string) => Promise<boolean>;
  verifyMap: Record<string, VerifyState>;
  pendingModels: string[];
  onRemovePending: (model: string) => void;
  isSaving: boolean;
  feedback: string | null;
  onSave: () => void;
  isDirty: boolean;
  isNew: boolean;
}) {
  const [newModel, setNewModel] = useState("");
  const [adding, setAdding] = useState(false);
  const [addError, setAddError] = useState<string | null>(null);

  const handleAdd = async () => {
    if (!newModel.trim() || adding) return;
    setAdding(true);
    setAddError(null);
    try {
      const err = await onAddModel(newModel.trim());
      if (err) setAddError(err);
      else setNewModel("");
    } finally {
      setAdding(false);
    }
  };

  return (
    <div className="channel-card">
      {/* 头部：折叠/展开切换 + 名称 + wire format + 删除按钮 */}
      <div className="channel-card-head">
        <button
          type="button"
          className="channel-card-toggle"
          onClick={onToggleExpand}
          title={expanded ? "折叠" : "展开"}
        >
          {expanded ? <ChevronDown size={16} /> : <ChevronRight size={16} />}
          <strong>{channel.name || "（未命名）"}</strong>
          <span className="channel-card-wire">{WIRE_FORMAT_LABELS[channel.wireFormat]}</span>
          {isNew && <span className="channel-card-badge">新建未保存</span>}
          {isDirty && !isNew && <span className="channel-card-badge">未保存改动</span>}
          <span className="channel-card-models">{channel.availableModels.length} 模型</span>
        </button>
        <button
          type="button"
          className="ghost compact"
          onClick={onRemove}
          title="删除此渠道"
        >
          <Trash2 size={12} /> 删除
        </button>
      </div>

      {expanded && (
        <div className="channel-card-body">
          <Row title="渠道名称" hint="给它一个能记得的别名">
            <input
              className="settings-input"
              type="text"
              value={channel.name}
              onChange={(e) => onUpdate({ name: e.target.value })}
              spellCheck={false}
              autoComplete="off"
            />
          </Row>
          <Row title="Wire format" hint="改 wire format 通常意味着另起一个渠道——这里允许改但请慎重">
            <SegmentedControl
              value={channel.wireFormat}
              onChange={(v) => onUpdateClearVerify({ wireFormat: v })}
              options={[
                { label: "Anthropic", value: "anthropic" },
                { label: "OpenAI Responses", value: "openai_responses" },
                { label: "OpenAI Chat", value: "openai_chat_completions" },
              ]}
            />
          </Row>
          <Row
            title="Base URL"
            hint={
              channel.wireFormat === "anthropic"
                ? "https://api.anthropic.com 或 claude-relay 代理地址（去尾斜杠）"
                : "https://api.openai.com 或任意 OpenAI-兼容 relay"
            }
          >
            <input
              className="settings-input"
              type="text"
              value={channel.baseUrl}
              onChange={(e) => onUpdateClearVerify({ baseUrl: e.target.value })}
              placeholder={
                channel.wireFormat === "anthropic"
                  ? "https://api.anthropic.com"
                  : "https://api.openai.com"
              }
              spellCheck={false}
              autoComplete="off"
            />
          </Row>
          <Row
            title="Token"
            hint={
              channel.wireFormat === "anthropic"
                ? "cr_xxx 或官方 sk-ant-xxx；显示时仅前 8 位 + 长度"
                : "sk-xxx；显示时仅前 8 位 + 长度"
            }
          >
            <input
              className="settings-input"
              type="text"
              value={channel.token}
              onChange={(e) => onUpdateClearVerify({ token: e.target.value })}
              placeholder={channel.wireFormat === "anthropic" ? "cr_..." : "sk-..."}
              spellCheck={false}
              autoComplete="off"
            />
          </Row>

          {/* wire-format 专属开关 */}
          {channel.wireFormat === "anthropic" && (
            <>
              <Row title="原生 web_search" hint="Anthropic 内置搜索（多数 relay 不支持）">
                <Switch
                  checked={channel.enableNativeWebSearch}
                  onChange={(v) => onUpdate({ enableNativeWebSearch: v })}
                />
              </Row>
              <Row
                title="Thinking 模式"
                hint="adaptive=4.6+ 推荐（模型自决深度）；enabled=老模型 manual budget；disabled=关。Haiku 即使设了也会被 drop。"
              >
                <SegmentedControl
                  value={channel.thinkingMode}
                  onChange={(v) => onUpdate({ thinkingMode: v as ThinkingMode })}
                  options={[
                    { label: "Adaptive", value: "adaptive" },
                    { label: "Manual", value: "enabled" },
                    { label: "关", value: "disabled" },
                  ]}
                />
              </Row>
              {channel.thinkingMode === "adaptive" && (
                <Row
                  title="Thinking 显示"
                  hint="summarized=UI 看到思考摘要（推荐）；omitted=只回 signature 不显示，首 token 更快"
                >
                  <SegmentedControl
                    value={channel.thinkingDisplay}
                    onChange={(v) => onUpdate({ thinkingDisplay: v as ThinkingDisplay })}
                    options={[
                      { label: "Summarized", value: "summarized" },
                      { label: "Omitted", value: "omitted" },
                    ]}
                  />
                </Row>
              )}
              {channel.thinkingMode === "enabled" && (
                <Row
                  title="Thinking 预算"
                  hint="manual budget_tokens——Opus 4.7 上会被自动转 adaptive"
                >
                  <NumberInput
                    value={channel.thinkingBudgetTokens}
                    min={1024}
                    max={32000}
                    step={500}
                    suffix="tokens"
                    onChange={(v) => onUpdate({ thinkingBudgetTokens: v })}
                  />
                </Row>
              )}
              <Row
                title="Effort"
                hint="Anthropic 4.6+ 识别。影响 thinking 深度 + tool call 数 + 文本长度。high=默认，xhigh=agentic 任务推荐"
              >
                <SegmentedControl
                  value={channel.defaultEffort ?? "off"}
                  onChange={(v) =>
                    onUpdate({ defaultEffort: v === "off" ? null : (v as EffortLevel) })
                  }
                  options={[
                    { label: "默认", value: "off" },
                    { label: "low", value: "low" },
                    { label: "medium", value: "medium" },
                    { label: "high", value: "high" },
                    { label: "xhigh", value: "xhigh" },
                    { label: "max", value: "max" },
                  ]}
                />
              </Row>
            </>
          )}
          {(channel.wireFormat === "openai_responses" ||
            channel.wireFormat === "openai_chat_completions") && (
            <Row title="Reasoning effort" hint="gpt-5/o3 系列识别；gpt-4 / DeepSeek 等忽略">
              <SegmentedControl
                value={channel.reasoningEffort ?? "off"}
                onChange={(v) =>
                  onUpdate({ reasoningEffort: v === "off" ? null : (v as ReasoningEffort) })
                }
                options={[
                  { label: "关", value: "off" },
                  { label: "low", value: "low" },
                  { label: "medium", value: "medium" },
                  { label: "high", value: "high" },
                ]}
              />
            </Row>
          )}
          {channel.wireFormat === "openai_responses" && (
            <Row title="原生 web_search" hint="Responses API 内置搜索工具">
              <Switch
                checked={channel.enableWebSearch}
                onChange={(v) => onUpdate({ enableWebSearch: v })}
              />
            </Row>
          )}

          {/* 添加模型 + 已加列表 */}
          <Row
            title="添加模型"
            hint="输入模型 ID + 回车 / 点添加 → 自动 verify → 通过后进入「待生效」，点保存渠道才正式入清单"
          >
            <div className="add-model-row">
              <input
                className="settings-input"
                type="text"
                value={newModel}
                onChange={(e) => {
                  setNewModel(e.target.value);
                  setAddError(null);
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && !adding) void handleAdd();
                }}
                placeholder={
                  channel.wireFormat === "anthropic"
                    ? "例 claude-opus-4-7"
                    : "例 gpt-5.5-instant / deepseek-chat"
                }
                spellCheck={false}
                autoComplete="off"
                disabled={adding}
              />
              <button
                type="button"
                className="model-verify-btn"
                onClick={() => void handleAdd()}
                disabled={adding || !newModel.trim()}
              >
                {adding ? <Loader2 size={14} className="spin" /> : <Plus size={14} />}
                <span>{adding ? "验证中" : "添加"}</span>
              </button>
              {addError && <div className="model-verify-err">{addError}</div>}
            </div>
          </Row>

          {pendingModels.length > 0 && (
            <Row title="待生效" hint="已 verify 通过，点下方「保存此渠道」后正式入清单">
              <div className="pending-model-list">
                {pendingModels.map((id) => (
                  <div className="pending-model-chip" key={id}>
                    <Check size={12} />
                    <span className="settings-readonly-mono">{id}</span>
                    <button
                      type="button"
                      className="ghost compact"
                      onClick={() => onRemovePending(id)}
                      title="撤销"
                    >
                      <XIcon size={12} />
                    </button>
                  </div>
                ))}
              </div>
            </Row>
          )}

          {channel.availableModels.length === 0 ? (
            <Row title="可用模型" hint="">
              <span className="settings-readonly">
                {pendingModels.length > 0 ? "暂无——保存后上方待生效模型会入此清单" : "暂无——上面输入 ID 添加"}
              </span>
            </Row>
          ) : (
            channel.availableModels.map((id) => {
              const state = verifyMap[verifyKey(channel.id, id)] ?? { kind: "idle" as const };
              return (
                <div className="settings-row available-model-row" key={id}>
                  <div className="settings-row-label">
                    <strong className="settings-readonly-mono">{id}</strong>
                  </div>
                  <div className="settings-row-control available-model-actions">
                    <button
                      type="button"
                      className={`model-verify-btn model-verify-btn-${state.kind}`}
                      onClick={() => void onVerifyModel(id)}
                      disabled={state.kind === "loading"}
                      title={state.kind === "err" ? `校验失败：${state.message}` : "1-token 探针"}
                    >
                      {state.kind === "loading" ? (
                        <Loader2 size={14} className="spin" />
                      ) : state.kind === "ok" ? (
                        <Check size={14} />
                      ) : state.kind === "err" ? (
                        <XIcon size={14} />
                      ) : (
                        <Check size={14} />
                      )}
                      <span>
                        {state.kind === "loading"
                          ? "校验中"
                          : state.kind === "ok"
                            ? "可用"
                            : state.kind === "err"
                              ? "失败"
                              : "验证"}
                      </span>
                    </button>
                    <button
                      type="button"
                      className="ghost compact"
                      onClick={() => onRemoveModel(id)}
                    >
                      <XIcon size={12} /> 移除
                    </button>
                    {state.kind === "err" && (
                      <div className="model-verify-err">{truncateForDisplay(state.message)}</div>
                    )}
                  </div>
                </div>
              );
            })
          )}

          <Row title="" hint="">
            <button
              type="button"
              className="settings-save-btn"
              disabled={!isDirty || isSaving}
              onClick={onSave}
            >
              {isSaving ? <Loader2 size={14} className="spin" /> : <Save size={14} />}
              {isDirty ? "保存此渠道" : "已是最新"}
            </button>
            {feedback && <span className="settings-feedback">{feedback}</span>}
          </Row>
        </div>
      )}
    </div>
  );
}

// ============================================================================
// 通用小组件
// ============================================================================

function Row({ title, hint, children }: { title: string; hint?: string; children: ReactNode }) {
  return (
    <div className="settings-row">
      <div className="settings-row-label">
        <strong>{title}</strong>
        {hint && <span>{hint}</span>}
      </div>
      <div className="settings-row-control">{children}</div>
    </div>
  );
}

function Switch({ checked, onChange }: { checked: boolean; onChange: (value: boolean) => void }) {
  return (
    <button
      type="button"
      className={`switch${checked ? " on" : ""}`}
      role="switch"
      aria-checked={checked}
      onClick={() => onChange(!checked)}
    >
      <span className="switch-thumb" />
    </button>
  );
}

function truncateForDisplay(s: string): string {
  const max = 200;
  return s.length <= max ? s : s.slice(0, max) + "…";
}

function SegmentedControl<T extends string | number>({
  value,
  onChange,
  options,
}: {
  value: T;
  onChange: (value: T) => void;
  options: Array<{ label: string; value: T }>;
}) {
  return (
    <div className="segmented" role="radiogroup">
      {options.map((option) => (
        <button
          key={String(option.value)}
          type="button"
          role="radio"
          aria-checked={option.value === value}
          className={`segmented-option${option.value === value ? " active" : ""}`}
          onClick={() => onChange(option.value)}
        >
          {option.label}
        </button>
      ))}
    </div>
  );
}

function NumberInput({
  value,
  min,
  max,
  step,
  suffix,
  onChange,
}: {
  value: number;
  min: number;
  max: number;
  step?: number;
  suffix?: string;
  onChange: (value: number) => void;
}) {
  const stride = step ?? 1;
  function clamp(v: number) {
    return Math.max(min, Math.min(max, Math.round(v)));
  }
  return (
    <div className="number-stepper">
      <button
        type="button"
        className="stepper-btn"
        onClick={() => onChange(clamp(value - stride))}
        disabled={value <= min}
      >
        −
      </button>
      <input
        type="number"
        value={value}
        min={min}
        max={max}
        step={stride}
        onChange={(event) => {
          const next = Number(event.target.value);
          if (Number.isFinite(next)) onChange(clamp(next));
        }}
      />
      {suffix && <span className="stepper-suffix">{suffix}</span>}
      <button
        type="button"
        className="stepper-btn"
        onClick={() => onChange(clamp(value + stride))}
        disabled={value >= max}
      >
        +
      </button>
    </div>
  );
}

// ============================================================================
// 数据源配置——TuShare token（用于历史 / 财务 / 大盘 / 板块数据）
// ============================================================================

const TUSHARE_TOKEN_KEY = "gangzi-terminal.tushare-token";

function DataSourceBlock() {
  const [token, setToken] = useState<string>("");
  const [stored, setStored] = useState<string>("");
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [savedAt, setSavedAt] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  // 初始 hydrate：从 app_state 拉已存 token
  useEffect(() => {
    let cancelled = false;
    invoke<unknown>("load_app_state", { key: TUSHARE_TOKEN_KEY })
      .then((v) => {
        if (cancelled) return;
        const value = typeof v === "string" ? v : "";
        setToken(value);
        setStored(value);
      })
      .catch(() => undefined)
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const dirty = token.trim() !== stored.trim();
  const handleSave = async () => {
    setSaving(true);
    setError(null);
    try {
      const value = token.trim();
      await invoke("save_tushare_token", { token: value });
      setStored(value);
      setSavedAt(Date.now());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };

  // saved 提示淡出
  useEffect(() => {
    if (savedAt === null) return;
    const t = window.setTimeout(() => setSavedAt(null), 2500);
    return () => window.clearTimeout(t);
  }, [savedAt]);

  return (
    <div className="settings-section">
      <div className="settings-section-head">
        <h3>数据源</h3>
        <p>
          TuShare Pro token——历史 K 线 / 财务 / 大盘 / 板块 / 资金面的主数据源。
          没填时 stocks 表填不进、scanner / 大盘日级数据不可用。
          注册 + 拿 token 见{" "}
          <a href="https://tushare.pro" target="_blank" rel="noreferrer">
            tushare.pro
          </a>
          ；个人 2000 积分档约 ¥120/年。
        </p>
      </div>
      <div className="settings-rows">
        <Row title="TuShare Token" hint={stored ? "已配置（保留隐私，不在 UI 上回显完整值）" : "未配置"}>
          <div className="settings-row-inline" style={{ gap: 8, flexWrap: "wrap" }}>
            <input
              type="password"
              placeholder={loading ? "加载中…" : "粘贴 token"}
              value={token}
              onChange={(e) => setToken(e.target.value)}
              disabled={loading || saving}
              style={{ minWidth: 320, padding: "6px 10px", borderRadius: 6, border: "1px solid #d0d7de" }}
              autoComplete="off"
              spellCheck={false}
            />
            <button
              type="button"
              className="primary"
              disabled={!dirty || saving || loading}
              onClick={handleSave}
            >
              {saving ? <Loader2 className="spin" size={14} /> : <Save size={14} />}
              保存
            </button>
            {savedAt !== null && <span className="muted">已保存</span>}
            {error && <span style={{ color: "#b14444" }}>{error}</span>}
          </div>
        </Row>
      </div>
    </div>
  );
}

// ============================================================================
// 网络 / 代理 IP 池（实时报价用）
// ============================================================================

type ProxyHealth = {
  label: string;
  health: number;
  blocked: boolean;
  blockedRemainingSecs: number;
};
type ProxyPoolDto = { urls: string[]; health: ProxyHealth[] };
type SourceHealth = { name: string; health: number };

function NetworkBlock() {
  const [text, setText] = useState("");
  const [health, setHealth] = useState<ProxyHealth[]>([]);
  const [sourceHealth, setSourceHealth] = useState<SourceHealth[]>([]);
  const [saving, setSaving] = useState(false);
  const [savedAt, setSavedAt] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = async () => {
    try {
      const pool = await invoke<ProxyPoolDto>("get_proxy_pool");
      setText(pool.urls.join("\n"));
      setHealth(pool.health);
      const sh = await invoke<SourceHealth[]>("get_realtime_health");
      setSourceHealth(sh);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  useEffect(() => {
    void refresh();
    const t = setInterval(() => void refresh(), 10_000); // 10s 刷一次健康度
    return () => clearInterval(t);
  }, []);

  const handleSave = async () => {
    setSaving(true);
    setError(null);
    try {
      const urls = text
        .split(/\r?\n/)
        .map((s) => s.trim())
        .filter((s) => s.length > 0);
      await invoke("set_proxy_pool", { args: { urls } });
      setSavedAt(Date.now());
      await refresh();
      setTimeout(() => setSavedAt(null), 2000);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  };

  const healthBar = (v: number) => {
    const pct = Math.max(0, Math.min(100, Math.round(v * 100)));
    return (
      <div
        style={{
          display: "inline-block",
          width: 80,
          height: 8,
          background: "#eee",
          borderRadius: 4,
          overflow: "hidden",
          verticalAlign: "middle",
        }}
      >
        <div
          style={{
            width: `${pct}%`,
            height: "100%",
            background: pct > 70 ? "#3a9b53" : pct > 30 ? "#c7a23a" : "#b14444",
          }}
        />
      </div>
    );
  };

  return (
    <div className="settings-section">
      <div className="settings-section-head">
        <h3>网络</h3>
        <p>
          实时报价多源（EM / 腾讯 / 新浪）+ 代理 IP 池。代理列表每行一个，留空 = 直连。
          支持 <code>http://</code> / <code>https://</code> / <code>socks5://</code>。
        </p>
      </div>
      <div className="settings-rows">
        <Row title="代理 IP 列表" hint="按健康度排序自动轮换；失败连续后自动拉黑 5 分钟">
          <div style={{ display: "flex", flexDirection: "column", gap: 8, width: "100%" }}>
            <textarea
              value={text}
              onChange={(e) => setText(e.target.value)}
              placeholder="socks5://127.0.0.1:7890&#10;http://user:pass@1.2.3.4:8080"
              rows={4}
              spellCheck={false}
              style={{
                width: "100%",
                fontFamily: "monospace",
                fontSize: 12,
                padding: "6px 10px",
                borderRadius: 6,
                border: "1px solid #d0d7de",
                resize: "vertical",
              }}
            />
            <div className="settings-row-inline" style={{ gap: 8 }}>
              <button
                type="button"
                className="primary"
                disabled={saving}
                onClick={handleSave}
              >
                {saving ? <Loader2 className="spin" size={14} /> : <Save size={14} />}
                保存代理列表
              </button>
              {savedAt !== null && <span className="muted">已生效</span>}
              {error && <span style={{ color: "#b14444" }}>{error}</span>}
            </div>
          </div>
        </Row>
        <Row title="代理健康度" hint="每 10s 刷新；连续失败 → EMA < 0.2 拉黑">
          <div style={{ display: "flex", flexDirection: "column", gap: 4, fontSize: 12 }}>
            {health.length === 0 && <span className="muted">无</span>}
            {health.map((h) => (
              <div key={h.label} style={{ display: "flex", alignItems: "center", gap: 8 }}>
                <code style={{ minWidth: 220 }}>{h.label}</code>
                {healthBar(h.health)}
                <span style={{ minWidth: 36, textAlign: "right" }}>
                  {Math.round(h.health * 100)}%
                </span>
                {h.blocked && (
                  <span style={{ color: "#b14444" }}>
                    拉黑 {h.blockedRemainingSecs}s
                  </span>
                )}
              </div>
            ))}
          </div>
        </Row>
        <Row title="实时报价源健康度" hint="EM > 腾讯 > 新浪 链式 fallback（dispatch）">
          <div style={{ display: "flex", flexDirection: "column", gap: 4, fontSize: 12 }}>
            {sourceHealth.map((s) => (
              <div key={s.name} style={{ display: "flex", alignItems: "center", gap: 8 }}>
                <code style={{ minWidth: 80 }}>{s.name}</code>
                {healthBar(s.health)}
                <span style={{ minWidth: 36, textAlign: "right" }}>
                  {Math.round(s.health * 100)}%
                </span>
              </div>
            ))}
          </div>
        </Row>
      </div>
    </div>
  );
}
