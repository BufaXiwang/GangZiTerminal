// AddChannelForm — 添加渠道（快速预设 / 自定义 两个 tab）+ 模型发现 + 手动降级。
//
// Spec: docs/design/frontend-design.md §设置页 — 添加渠道
//   快速预设：选官方 → 只填 API Key（host + wireFormat 来自预设，只读）
//   自定义：渠道名 + 消息格式 + Host + API Key
//   → 发现模型：成功 = 多选勾选保留；失败 = 降级手动输入模型名（多个）
//   → 每个确认保留的模型各调一次 agentAddChannel
//
// API Key 写入即提交，永不回显（type="password"，不预填）。

import { AlertCircle, Check, Loader2, Plus } from "lucide-react";
import { useCallback, useMemo, useState } from "react";
import {
  commands,
  type AddChannelInput,
  type ChannelPresetView,
  type DiscoveredModel,
  type WireFormat,
} from "../../bindings";
import {
  WIRE_FORMAT_LABEL,
  WIRE_FORMAT_OPTIONS,
  formatError,
  providerInitial,
} from "./types";

type Tab = "preset" | "custom";

interface AddChannelFormProps {
  presets: ChannelPresetView[];
  /** 全部模型保存成功后回调：父组件 refetch 列表。 */
  onSaved: () => void;
}

/** 发现阶段：先填连接信息，再发现/手填模型。 */
interface ConnDraft {
  provider: string;
  wireFormat: WireFormat;
  baseUrl: string;
  apiKey: string;
}

export function AddChannelForm({ presets, onSaved }: AddChannelFormProps) {
  const [tab, setTab] = useState<Tab>("preset");

  // === preset tab ===
  const [presetKey, setPresetKey] = useState<string | null>(
    presets[0]?.key ?? null,
  );
  const [presetApiKey, setPresetApiKey] = useState("");
  const selectedPreset = useMemo(
    () => presets.find((p) => p.key === presetKey) ?? null,
    [presets, presetKey],
  );

  // === custom tab ===
  const [customProvider, setCustomProvider] = useState("");
  const [customWire, setCustomWire] = useState<WireFormat>("messages");
  const [customBaseUrl, setCustomBaseUrl] = useState("");
  const [customApiKey, setCustomApiKey] = useState("");

  // === discovery / save state ===
  const [discovering, setDiscovering] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // 已锁定的连接信息（发现/手填阶段一直用它建渠道）。
  const [conn, setConn] = useState<ConnDraft | null>(null);
  // 发现到的模型（成功路径）。
  const [discovered, setDiscovered] = useState<DiscoveredModel[] | null>(null);
  const [selectedModels, setSelectedModels] = useState<Set<string>>(new Set());
  // 发现失败 → 手动输入模型名（逗号 / 换行分隔）。
  const [manualMode, setManualMode] = useState(false);
  const [manualText, setManualText] = useState("");

  const resetDiscovery = useCallback(() => {
    setConn(null);
    setDiscovered(null);
    setSelectedModels(new Set());
    setManualMode(false);
    setManualText("");
    setError(null);
  }, []);

  const resetAll = useCallback(() => {
    resetDiscovery();
    setPresetApiKey("");
    setCustomProvider("");
    setCustomBaseUrl("");
    setCustomApiKey("");
    setCustomWire("messages");
  }, [resetDiscovery]);

  /** 从当前 tab 收集连接信息（校验必填）。 */
  const collectConn = useCallback((): ConnDraft | string => {
    if (tab === "preset") {
      if (!selectedPreset) return "请先选择一个预设服务商";
      if (!presetApiKey.trim()) return "请填写 API Key";
      return {
        provider: selectedPreset.provider,
        wireFormat: selectedPreset.wireFormat,
        baseUrl: selectedPreset.baseUrl,
        apiKey: presetApiKey.trim(),
      };
    }
    if (!customProvider.trim()) return "请填写渠道名";
    if (!customBaseUrl.trim()) return "请填写 Host";
    if (!customApiKey.trim()) return "请填写 API Key";
    return {
      provider: customProvider.trim(),
      wireFormat: customWire,
      baseUrl: customBaseUrl.trim(),
      apiKey: customApiKey.trim(),
    };
  }, [
    tab,
    selectedPreset,
    presetApiKey,
    customProvider,
    customBaseUrl,
    customApiKey,
    customWire,
  ]);

  const handleDiscover = useCallback(async () => {
    const c = collectConn();
    if (typeof c === "string") {
      setError(c);
      return;
    }
    setError(null);
    setDiscovering(true);
    setConn(c);
    const res = await commands.agentDiscoverModels(
      c.wireFormat,
      c.baseUrl,
      c.apiKey,
    );
    setDiscovering(false);
    if (res.status === "ok") {
      const sorted = [...res.data].sort((a, b) => b.id.localeCompare(a.id));
      setDiscovered(sorted);
      setManualMode(false);
      // 默认全选发现到的模型，方便一键保存。
      setSelectedModels(new Set(sorted.map((m) => m.id)));
      if (res.data.length === 0) {
        // 没发现到任何模型 → 也允许手填。
        setManualMode(true);
        setError("未发现可用模型，可手动输入模型名后保存。");
      }
    } else {
      // 发现失败 → 降级手动输入，不阻塞配置。
      setDiscovered(null);
      setManualMode(true);
      setError(`模型发现失败（${formatError(res.error)}）。可手动输入模型名后保存。`);
    }
  }, [collectConn]);

  const toggleModel = useCallback((id: string) => {
    setSelectedModels((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  /** 解析手动输入：逗号 / 换行 / 空白分隔，去重去空。 */
  const parseManual = useCallback((text: string): string[] => {
    const seen = new Set<string>();
    const out: string[] = [];
    for (const raw of text.split(/[\n,]/)) {
      const m = raw.trim();
      if (m && !seen.has(m)) {
        seen.add(m);
        out.push(m);
      }
    }
    return out;
  }, []);

  const handleSave = useCallback(async () => {
    if (!conn) return;
    const models = manualMode
      ? parseManual(manualText)
      : Array.from(selectedModels);
    if (models.length === 0) {
      setError(manualMode ? "请至少输入一个模型名" : "请至少勾选一个模型");
      return;
    }
    setError(null);
    setSaving(true);
    // 每个确认保留的模型各成一条渠道，复用同一连接信息。
    for (const model of models) {
      const input: AddChannelInput = {
        provider: conn.provider,
        wireFormat: conn.wireFormat,
        baseUrl: conn.baseUrl,
        apiKey: conn.apiKey,
        model,
      };
      const res = await commands.agentAddChannel(input);
      if (res.status === "error") {
        setSaving(false);
        setError(`保存「${model}」失败：${formatError(res.error)}`);
        return;
      }
    }
    setSaving(false);
    resetAll();
    onSaved();
  }, [
    conn,
    manualMode,
    manualText,
    selectedModels,
    parseManual,
    resetAll,
    onSaved,
  ]);

  const busy = discovering || saving;
  const inDiscovery = conn !== null;
  const manualCount = manualMode ? parseManual(manualText).length : 0;

  return (
    <div className="settings-add-form">
      {/* tabs */}
      <div className="segmented settings-add-tabs" role="tablist">
        <button
          type="button"
          role="tab"
          aria-selected={tab === "preset"}
          className={`segmented-item${tab === "preset" ? " active" : ""}`}
          onClick={() => {
            setTab("preset");
            resetDiscovery();
          }}
          disabled={busy}
        >
          快速预设
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={tab === "custom"}
          className={`segmented-item${tab === "custom" ? " active" : ""}`}
          onClick={() => {
            setTab("custom");
            resetDiscovery();
          }}
          disabled={busy}
        >
          自定义
        </button>
      </div>

      {/* === step 1: connection === */}
      {!inDiscovery && tab === "preset" && (
        <div className="settings-form-body">
          <div className="settings-preset-cards">
            {presets.length === 0 ? (
              <span className="muted">无可用预设。</span>
            ) : (
              presets.map((p) => {
                const active = p.key === presetKey;
                return (
                  <button
                    key={p.key}
                    type="button"
                    className={`settings-preset-card${active ? " active" : ""}`}
                    onClick={() => setPresetKey(p.key)}
                    aria-pressed={active}
                  >
                    <span className="settings-avatar" aria-hidden>
                      {providerInitial(p.provider)}
                    </span>
                    <span className="settings-preset-text">
                      <span className="settings-preset-name">
                        {p.provider}
                      </span>
                      <span className="settings-preset-sub muted">
                        {WIRE_FORMAT_LABEL[p.wireFormat]}
                      </span>
                    </span>
                    {active && (
                      <Check
                        size={14}
                        strokeWidth={2.5}
                        className="settings-preset-check"
                        aria-hidden
                      />
                    )}
                  </button>
                );
              })
            )}
          </div>

          {selectedPreset && (
            <div className="settings-field-grid">
              <label className="settings-field">
                <span className="settings-field-label">Host</span>
                <input
                  className="settings-input"
                  value={selectedPreset.baseUrl}
                  readOnly
                  tabIndex={-1}
                />
              </label>
              <label className="settings-field">
                <span className="settings-field-label">消息格式</span>
                <input
                  className="settings-input"
                  value={WIRE_FORMAT_LABEL[selectedPreset.wireFormat]}
                  readOnly
                  tabIndex={-1}
                />
              </label>
              <label className="settings-field settings-field-wide">
                <span className="settings-field-label">API Key</span>
                <input
                  className="settings-input"
                  type="password"
                  autoComplete="off"
                  placeholder="只提交不回显"
                  value={presetApiKey}
                  onChange={(e) => setPresetApiKey(e.target.value)}
                />
              </label>
            </div>
          )}
        </div>
      )}

      {!inDiscovery && tab === "custom" && (
        <div className="settings-form-body">
          <div className="settings-field-grid">
            <label className="settings-field">
              <span className="settings-field-label">渠道名</span>
              <input
                className="settings-input"
                placeholder="如 MyProxy"
                value={customProvider}
                onChange={(e) => setCustomProvider(e.target.value)}
              />
            </label>
            <label className="settings-field">
              <span className="settings-field-label">消息格式</span>
              <select
                className="settings-input"
                value={customWire}
                onChange={(e) => setCustomWire(e.target.value as WireFormat)}
              >
                {WIRE_FORMAT_OPTIONS.map((w) => (
                  <option key={w} value={w}>
                    {WIRE_FORMAT_LABEL[w]}
                  </option>
                ))}
              </select>
            </label>
            <label className="settings-field settings-field-wide">
              <span className="settings-field-label">Host</span>
              <input
                className="settings-input"
                placeholder="https://api.example.com/v1"
                value={customBaseUrl}
                onChange={(e) => setCustomBaseUrl(e.target.value)}
              />
            </label>
            <label className="settings-field settings-field-wide">
              <span className="settings-field-label">API Key</span>
              <input
                className="settings-input"
                type="password"
                autoComplete="off"
                placeholder="只提交不回显"
                value={customApiKey}
                onChange={(e) => setCustomApiKey(e.target.value)}
              />
            </label>
          </div>
        </div>
      )}

      {/* === step 2: discovery / manual === */}
      {inDiscovery && (
        <div className="settings-form-body">
          <div className="settings-conn-summary muted">
            {conn.provider} · {WIRE_FORMAT_LABEL[conn.wireFormat]} · {conn.baseUrl}
          </div>

          {!manualMode && discovered && (
            <div className="settings-model-picker">
              <div className="settings-model-picker-head muted">
                <label className="settings-model-toggle-all">
                  <input
                    type="checkbox"
                    checked={discovered.length > 0 && selectedModels.size === discovered.length}
                    ref={(el) => {
                      if (el) el.indeterminate = selectedModels.size > 0 && selectedModels.size < discovered.length;
                    }}
                    onChange={() => {
                      if (selectedModels.size === discovered.length) {
                        setSelectedModels(new Set());
                      } else {
                        setSelectedModels(new Set(discovered.map((m) => m.id)));
                      }
                    }}
                  />
                  发现 {discovered.length} 个模型，已选 {selectedModels.size} 个：
                </label>
              </div>
              <ul className="settings-model-list">
                {discovered.map((m) => {
                  const checked = selectedModels.has(m.id);
                  return (
                    <li key={m.id} className="settings-model-item">
                      <label className="settings-model-label">
                        <input
                          type="checkbox"
                          checked={checked}
                          onChange={() => toggleModel(m.id)}
                        />
                        <span className="settings-model-id">{m.id}</span>
                        {m.displayName && m.displayName !== m.id && (
                          <span className="muted settings-model-display">
                            {m.displayName}
                          </span>
                        )}
                      </label>
                    </li>
                  );
                })}
              </ul>
            </div>
          )}

          {manualMode && (
            <label className="settings-field settings-field-wide">
              <span className="settings-field-label">
                手动输入模型名（逗号或换行分隔，可多个）
              </span>
              <textarea
                className="settings-input settings-textarea"
                rows={3}
                placeholder={"deepseek-chat\ndeepseek-reasoner"}
                value={manualText}
                onChange={(e) => setManualText(e.target.value)}
              />
              {manualCount > 0 && (
                <span className="muted settings-manual-count">
                  将保存 {manualCount} 个模型
                </span>
              )}
            </label>
          )}
        </div>
      )}

      {/* error */}
      {error && (
        <div className="settings-form-error" role="alert">
          <AlertCircle size={14} />
          <span>{error}</span>
        </div>
      )}

      {/* actions — sticky at bottom so save button is always visible */}
      <div className="settings-form-actions">
        {!inDiscovery ? (
          <button
            type="button"
            className="btn primary"
            onClick={handleDiscover}
            disabled={busy}
          >
            {discovering ? (
              <>
                <Loader2 size={14} className="settings-spin" /> 发现中…
              </>
            ) : (
              "发现模型"
            )}
          </button>
        ) : (
          <>
            <button
              type="button"
              className="btn ghost"
              onClick={resetDiscovery}
              disabled={busy}
            >
              返回
            </button>
            {!manualMode && discovered && discovered.length > 0 && (
              <button
                type="button"
                className="btn ghost"
                onClick={() => {
                  setManualMode(true);
                }}
                disabled={busy}
                title="改为手动输入模型名"
              >
                手动输入
              </button>
            )}
            <button
              type="button"
              className="btn primary"
              onClick={handleSave}
              disabled={busy}
            >
              {saving ? (
                <>
                  <Loader2 size={14} className="settings-spin" /> 保存中…
                </>
              ) : (
                <>
                  <Plus size={14} />{" "}
                  {manualMode
                    ? manualCount > 0
                      ? `保存 ${manualCount} 个模型`
                      : "保存"
                    : selectedModels.size > 0
                      ? `保存 ${selectedModels.size} 个模型`
                      : "保存选中模型"}
                </>
              )}
            </button>
          </>
        )}
      </div>
    </div>
  );
}
