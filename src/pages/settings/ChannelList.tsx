// ChannelList — 按渠道连接分组的工具函数 + 子组件。
//
// Spec: docs/design/frontend-design.md §设置页 — 渠道/模型列表
//        docs/design/agent-infra-module.md §5 前端命令 — agent_update_channel
//
// 概念分离：
//   渠道 (Channel) = provider connection (provider + wireFormat + baseUrl + apiKey)
//   模型 (Model)   = 该连接下的单个 model 记录
//
// 导出：
//   - ChannelGroup 类型 + groupChannels() + channelStats()
//   - GroupEditForm — 编辑渠道连接 modal 内容
//   - DiscoverMorePanel — 发现更多模型 modal 内容

import { Check, Loader2, X } from "lucide-react";
import { useCallback, useState } from "react";
import {
  commands,
  type ProviderChannelView,
  type DiscoveredModel,
  type AddChannelInput,
} from "../../bindings";
import {
  WIRE_FORMAT_LABEL,
  WIRE_FORMAT_OPTIONS,
  formatError,
} from "./types";
import type { WireFormat } from "../../bindings";

// ---------------------------------------------------------------------------
// Grouping
// ---------------------------------------------------------------------------

export type ChannelGroup = {
  key: string;
  provider: string;
  wireFormat: WireFormat;
  baseUrl: string | null;
  apiKeySet: boolean;
  channels: ProviderChannelView[];
};

export function groupChannels(channels: ProviderChannelView[]): ChannelGroup[] {
  const map = new Map<string, ChannelGroup>();
  for (const ch of channels) {
    const key = `${ch.provider}|${ch.wireFormat}|${ch.baseUrl ?? ""}`;
    let group = map.get(key);
    if (!group) {
      group = {
        key,
        provider: ch.provider,
        wireFormat: ch.wireFormat,
        baseUrl: ch.baseUrl ?? null,
        apiKeySet: ch.apiKeySet,
        channels: [],
      };
      map.set(key, group);
    }
    group.channels.push(ch);
    if (ch.apiKeySet) group.apiKeySet = true;
  }
  return Array.from(map.values());
}

/** 统计 channel groups 数量和总 model 数。 */
export function channelStats(channels: ProviderChannelView[]): {
  groupCount: number;
  modelCount: number;
} {
  const groups = groupChannels(channels);
  return { groupCount: groups.length, modelCount: channels.length };
}

// ---------------------------------------------------------------------------
// GroupEditForm — 编辑渠道连接（不含 model）
// ---------------------------------------------------------------------------

interface GroupEditFormProps {
  group: ChannelGroup;
  onCancel: () => void;
  onSaved: () => void;
  onRemoveModel: (channelId: string) => void;
}

/** 编辑渠道连接信息 + 管理模型列表。 */
export function GroupEditForm({ group, onCancel, onSaved, onRemoveModel }: GroupEditFormProps) {
  const [provider, setProvider] = useState(group.provider);
  const [wireFormat, setWireFormat] = useState<WireFormat>(group.wireFormat);
  const [baseUrl, setBaseUrl] = useState(group.baseUrl ?? "");
  // apiKey 不回读：留空提交则保留原 key。
  const [apiKey, setApiKey] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const handleSave = async () => {
    if (!provider.trim()) {
      setError("请填写渠道名");
      return;
    }
    setError(null);
    setSaving(true);
    const trimmedKey = apiKey.trim();
    // 批量更新 group 下每个 channel。
    for (const ch of group.channels) {
      const res = await commands.agentUpdateChannel({
        channelId: ch.channelId,
        provider: provider.trim(),
        wireFormat,
        baseUrl: baseUrl.trim() || undefined,
        model: ch.model, // model 保持不变
        apiKey: trimmedKey || undefined,
        enabled: ch.enabled,
      });
      if (res.status === "error") {
        setSaving(false);
        setError(`更新「${ch.model}」失败：${formatError(res.error)}`);
        return;
      }
    }
    setSaving(false);
    onSaved();
  };

  return (
    <div className="settings-channel-edit">
      <div className="settings-field-grid">
        <label className="settings-field">
          <span className="settings-field-label">渠道名</span>
          <input
            className="settings-input"
            value={provider}
            onChange={(e) => setProvider(e.target.value)}
          />
        </label>
        <label className="settings-field">
          <span className="settings-field-label">消息格式</span>
          <select
            className="settings-input"
            value={wireFormat}
            onChange={(e) => setWireFormat(e.target.value as WireFormat)}
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
            value={baseUrl}
            onChange={(e) => setBaseUrl(e.target.value)}
          />
        </label>
        <label className="settings-field settings-field-wide">
          <span className="settings-field-label">API Key</span>
          <input
            className="settings-input"
            type="password"
            autoComplete="off"
            placeholder={group.apiKeySet ? "••••••••（留空保留原 key）" : "输入 API Key"}
            value={apiKey}
            onChange={(e) => setApiKey(e.target.value)}
          />
        </label>
      </div>

      {/* 模型列表 */}
      <div className="settings-edit-models">
        <span className="settings-field-label">模型（{group.channels.length} 个）</span>
        <div className="settings-edit-model-list">
          {group.channels.map((ch) => (
            <div key={ch.channelId} className="settings-edit-model-row">
              <span className="settings-edit-model-name">{ch.model}</span>
              {ch.isActive ? (
                <span className="settings-edit-model-active">使用中</span>
              ) : (
                <button
                  type="button"
                  className="settings-edit-model-del"
                  onMouseDown={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    onRemoveModel(ch.channelId);
                  }}
                  title="删除模型"
                >
                  <X size={13} />
                </button>
              )}
            </div>
          ))}
        </div>
      </div>

      {error && (
        <div className="settings-form-error" role="alert">
          <span>{error}</span>
        </div>
      )}

      <div className="settings-form-actions">
        <button
          type="button"
          className="btn ghost"
          onClick={onCancel}
          disabled={saving}
        >
          <X size={14} /> 取消
        </button>
        <button
          type="button"
          className="btn primary"
          onClick={handleSave}
          disabled={saving}
        >
          <Check size={14} /> {saving ? "保存中…" : "保存"}
        </button>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// DiscoverMorePanel — 复用已有连接发现并批量添加更多模型
// ---------------------------------------------------------------------------

export function DiscoverMorePanel({
  group,
  existingModels,
  onCancel,
  onSaved,
}: {
  group: ChannelGroup;
  existingModels: Set<string>;
  onCancel: () => void;
  onSaved: () => void;
}) {
  const [discovering, setDiscovering] = useState(false);
  const [discovered, setDiscovered] = useState<DiscoveredModel[] | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [manualInput, setManualInput] = useState("");
  const [validationWarning, setValidationWarning] = useState<string[] | null>(null);

  // Extracted add-models helper shared by handleManualAdd and the "仍然添加" confirmation.
  const doAddModels = useCallback(async (models: string[]) => {
    setSaving(true);
    setError(null);
    for (const model of models) {
      const input: AddChannelInput = {
        provider: group.provider,
        wireFormat: group.wireFormat,
        baseUrl: group.baseUrl ?? "",
        apiKey: "",
        model,
      };
      const res = await commands.agentAddChannel(input);
      if (res.status === "error") {
        setSaving(false);
        setError(`添加「${model}」失败：${formatError(res.error)}`);
        return;
      }
    }
    setSaving(false);
    onSaved();
  }, [group, onSaved]);

  const handleDiscover = useCallback(async () => {
    setDiscovering(true);
    setError(null);
    const firstChannelId = group.channels[0]?.channelId;
    const res = firstChannelId
      ? await commands.agentDiscoverModelsForChannel(firstChannelId)
      : await commands.agentDiscoverModels(
          group.wireFormat,
          group.baseUrl ?? "",
          "",
        );
    setDiscovering(false);
    if (res.status === "ok") {
      const sorted = [...res.data].sort((a, b) => b.id.localeCompare(a.id));
      setDiscovered(sorted);
      const fresh = sorted.filter((m) => !existingModels.has(m.id));
      setSelected(new Set(fresh.map((m) => m.id)));
    } else {
      setError(formatError(res.error));
    }
  }, [group, existingModels]);

  const toggle = useCallback((id: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }, []);

  const handleSave = useCallback(async () => {
    const models = Array.from(selected).filter((m) => !existingModels.has(m));
    if (models.length === 0) {
      setError("没有选中新模型");
      return;
    }
    setSaving(true);
    setError(null);
    for (const model of models) {
      const input: AddChannelInput = {
        provider: group.provider,
        wireFormat: group.wireFormat,
        baseUrl: group.baseUrl ?? "",
        apiKey: "",
        model,
      };
      const res = await commands.agentAddChannel(input);
      if (res.status === "error") {
        setSaving(false);
        setError(`添加「${model}」失败：${formatError(res.error)}`);
        return;
      }
    }
    setSaving(false);
    onSaved();
  }, [selected, existingModels, group, onSaved]);

  const newModels =
    discovered?.filter((m) => !existingModels.has(m.id)) ?? [];
  const oldModels =
    discovered?.filter((m) => existingModels.has(m.id)) ?? [];

  const handleManualAdd = useCallback(async () => {
    const all = manualInput.split(/[\n,]/).map((s) => s.trim()).filter(Boolean);
    if (all.length === 0) {
      setError("请输入至少一个模型名");
      return;
    }
    const alreadyAdded = all.filter((s) => existingModels.has(s));
    const models = all.filter((s) => !existingModels.has(s));
    if (models.length === 0) {
      setError(`${alreadyAdded.join("、")} 已添加`);
      return;
    }

    // Validate: discover available models and check if the input exists
    setSaving(true);
    setError(null);

    const firstChannelId = group.channels[0]?.channelId;
    if (firstChannelId) {
      const discoverRes = await commands.agentDiscoverModelsForChannel(firstChannelId);
      if (discoverRes.status === "ok") {
        const available = new Set(discoverRes.data.map((m) => m.id));
        const unknown = models.filter((m) => !available.has(m));
        if (unknown.length > 0) {
          // Show confirmation — user can still force-add
          setValidationWarning(unknown);
          setSaving(false);
          return;
        }
      }
      // If discover fails, skip validation and proceed
    }

    await doAddModels(models);
  }, [manualInput, existingModels, group, doAddModels]);

  return (
    <div className="settings-discover-panel">
      {!discovered ? (
        <>
          <div style={{ display: "flex", gap: 8, alignItems: "center", marginBottom: 12 }}>
            <input
              className="settings-input"
              style={{ flex: 1 }}
              placeholder="手动输入模型名（逗号分隔）"
              value={manualInput}
              onChange={(e) => setManualInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && manualInput.trim()) {
                  e.preventDefault();
                  void handleManualAdd();
                }
              }}
            />
            <button
              type="button"
              className="btn primary"
              onClick={handleManualAdd}
              disabled={saving || !manualInput.trim()}
              style={{ whiteSpace: "nowrap" }}
            >
              {saving ? "添加中…" : "添加"}
            </button>
          </div>
          {validationWarning && (
            <div className="settings-validation-warn">
              <p>以下模型未在服务商返回的模型列表中找到：</p>
              <p className="tabular">{validationWarning.join(", ")}</p>
              <div className="settings-form-actions">
                <button
                  className="btn ghost"
                  onClick={() => setValidationWarning(null)}
                >
                  取消
                </button>
                <button
                  className="btn primary"
                  onClick={() => {
                    setValidationWarning(null);
                    const models = manualInput
                      .split(/[\n,]/)
                      .map((s) => s.trim())
                      .filter((s) => s && !existingModels.has(s));
                    void doAddModels(models);
                  }}
                >
                  仍然添加
                </button>
              </div>
            </div>
          )}
          <div className="settings-form-actions">
            <button type="button" className="btn ghost" onClick={onCancel}>
              取消
            </button>
            <button
              type="button"
              className="btn primary"
              onClick={handleDiscover}
              disabled={discovering}
            >
              {discovering ? (
                <>
                  <Loader2 size={14} className="settings-spin" /> 发现中…
                </>
              ) : (
                <>接口发现</>
              )}
            </button>
          </div>
        </>
      ) : (
        <>
          {newModels.length > 0 && (
            <div className="settings-model-picker">
              <div className="settings-model-picker-head muted">
                <label className="settings-model-toggle-all">
                  <input
                    type="checkbox"
                    checked={
                      newModels.length > 0 &&
                      selected.size === newModels.length
                    }
                    ref={(el) => {
                      if (el)
                        el.indeterminate =
                          selected.size > 0 &&
                          selected.size < newModels.length;
                    }}
                    onChange={() => {
                      if (selected.size === newModels.length) {
                        setSelected(new Set());
                      } else {
                        setSelected(new Set(newModels.map((m) => m.id)));
                      }
                    }}
                  />
                  {newModels.length} 个新模型可添加：
                </label>
              </div>
              <ul className="settings-model-list">
                {newModels.map((m) => (
                  <li key={m.id} className="settings-model-item">
                    <label className="settings-model-label">
                      <input
                        type="checkbox"
                        checked={selected.has(m.id)}
                        onChange={() => toggle(m.id)}
                      />
                      <span className="settings-model-id">{m.id}</span>
                      {m.displayName && m.displayName !== m.id && (
                        <span className="muted settings-model-display">
                          {m.displayName}
                        </span>
                      )}
                    </label>
                  </li>
                ))}
              </ul>
            </div>
          )}
          {oldModels.length > 0 && (
            <div
              className="muted"
              style={{ fontSize: 12, padding: "4px 0" }}
            >
              已添加：{oldModels.map((m) => m.id).join("、")}
            </div>
          )}
          {newModels.length === 0 && (
            <div
              className="muted"
              style={{ fontSize: 13, padding: "8px 0" }}
            >
              该服务商的所有模型都已添加。
            </div>
          )}
        </>
      )}

      {error && (
        <div className="settings-form-error" role="alert">
          <span>{error}</span>
        </div>
      )}

      {discovered && (
        <div className="settings-form-actions">
          <button type="button" className="btn ghost" onClick={onCancel}>
            取消
          </button>
          {newModels.length > 0 && (
            <button
              type="button"
              className="btn primary"
              onClick={handleSave}
              disabled={saving || selected.size === 0}
            >
              {saving ? "添加中…" : `添加 ${selected.size} 个模型`}
            </button>
          )}
        </div>
      )}
    </div>
  );
}
