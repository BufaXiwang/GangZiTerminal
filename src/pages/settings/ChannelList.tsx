// ChannelList — 已配置渠道列表 + 行内编辑。
//
// Spec: docs/design/frontend-design.md §设置页 — 渠道/模型列表（每个保留模型一行）
//        docs/design/agent-infra-module.md §5 前端命令 — agent_update_channel
//
// 每行：`{model} ({provider})` + wireFormat badge + host + 已配置 key 状态 +
//        enabled / active 标记 + 编辑 + 删除。apiKey 永不展示明文（只显示 apiKeySet）。
// 编辑：点编辑 → 该行原地展开紧凑表单（渠道名/消息格式/Host/model/apiKey/enabled）。
//        apiKey 不预填，留空 = 保留原 key（spec §5）。

import { Check, Globe, Pencil, Trash2, X } from "lucide-react";
import { useState } from "react";
import { commands, type ProviderChannelView } from "../../bindings";
import {
  WIRE_FORMAT_LABEL,
  WIRE_FORMAT_OPTIONS,
  formatError,
  hostOf,
  providerInitial,
} from "./types";
import type { WireFormat } from "../../bindings";

interface ChannelListProps {
  channels: ProviderChannelView[];
  onRemove: (channelId: string) => void;
  /** 编辑保存成功后回调：父组件 refetch 列表。 */
  onSaved: () => void;
  busy?: boolean;
}

export function ChannelList({
  channels,
  onRemove,
  onSaved,
  busy,
}: ChannelListProps) {
  // 当前处于编辑态的 channelId（至多一行）。
  const [editingId, setEditingId] = useState<string | null>(null);

  if (channels.length === 0) {
    return (
      <div className="settings-empty muted">
        还没有配置渠道。使用下方「添加渠道」连接服务商并发现模型。
      </div>
    );
  }

  return (
    <ul className="settings-channel-list">
      {channels.map((ch) => {
        const editing = editingId === ch.channelId;
        return (
          <li key={ch.channelId} className="settings-channel-row">
            <div className="settings-channel-line">
              <span className="settings-avatar" aria-hidden>
                {providerInitial(ch.provider)}
              </span>

              <div className="settings-channel-main">
                <span className="settings-channel-name">
                  <span className="settings-channel-model">{ch.model}</span>
                </span>
                <span className="settings-channel-provider muted">
                  {ch.provider}
                </span>
              </div>

              <div className="settings-channel-meta">
                <span className="settings-wire-badge" title="消息格式">
                  {WIRE_FORMAT_LABEL[ch.wireFormat]}
                </span>
                <span
                  className="settings-channel-host tabular"
                  title={ch.baseUrl ?? ""}
                >
                  <Globe
                    size={11}
                    className="settings-channel-host-icon"
                    aria-hidden
                  />
                  <span className="settings-channel-host-text">
                    {hostOf(ch.baseUrl)}
                  </span>
                </span>
                <span
                  className={`settings-key-status${ch.apiKeySet ? " set" : ""}`}
                  title={ch.apiKeySet ? "已配置 API Key" : "未配置 API Key"}
                >
                  <span className="settings-key-dot" aria-hidden />
                  {ch.apiKeySet ? "已配置" : "无 key"}
                </span>
                {!ch.enabled && (
                  <span className="settings-disabled-tag">已禁用</span>
                )}
              </div>

              <button
                type="button"
                className="btn ghost settings-edit-btn"
                onClick={() =>
                  setEditingId(editing ? null : ch.channelId)
                }
                disabled={busy}
                title="编辑渠道"
                aria-label={`编辑渠道 ${ch.model} (${ch.provider})`}
                aria-expanded={editing}
              >
                <Pencil size={14} />
              </button>
              <button
                type="button"
                className="btn ghost settings-remove-btn"
                onClick={() => onRemove(ch.channelId)}
                disabled={busy}
                title="删除渠道"
                aria-label={`删除渠道 ${ch.model} (${ch.provider})`}
              >
                <Trash2 size={14} />
              </button>
            </div>

            {editing && (
              <ChannelEditForm
                channel={ch}
                onCancel={() => setEditingId(null)}
                onSaved={() => {
                  setEditingId(null);
                  onSaved();
                }}
              />
            )}
          </li>
        );
      })}
    </ul>
  );
}

interface ChannelEditFormProps {
  channel: ProviderChannelView;
  onCancel: () => void;
  onSaved: () => void;
}

/** 行内编辑表单：apiKey 不预填（留空 = 保留原 key）。 */
function ChannelEditForm({ channel, onCancel, onSaved }: ChannelEditFormProps) {
  const [provider, setProvider] = useState(channel.provider);
  const [wireFormat, setWireFormat] = useState<WireFormat>(channel.wireFormat);
  const [baseUrl, setBaseUrl] = useState(channel.baseUrl ?? "");
  const [model, setModel] = useState(channel.model);
  // apiKey 不回读：留空提交则保留原 key。
  const [apiKey, setApiKey] = useState("");
  const [enabled, setEnabled] = useState(channel.enabled);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const handleSave = async () => {
    if (!provider.trim()) {
      setError("请填写渠道名");
      return;
    }
    if (!model.trim()) {
      setError("请填写 model");
      return;
    }
    setError(null);
    setSaving(true);
    const trimmedKey = apiKey.trim();
    const res = await commands.agentUpdateChannel({
      channelId: channel.channelId,
      provider: provider.trim(),
      wireFormat,
      baseUrl: baseUrl.trim() || undefined,
      model: model.trim(),
      apiKey: trimmedKey || undefined,
      enabled,
    });
    setSaving(false);
    if (res.status === "error") {
      setError(formatError(res.error));
      return;
    }
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
        <label className="settings-field">
          <span className="settings-field-label">model</span>
          <input
            className="settings-input"
            value={model}
            onChange={(e) => setModel(e.target.value)}
          />
        </label>
        <label className="settings-field">
          <span className="settings-field-label">API Key</span>
          <input
            className="settings-input"
            type="password"
            autoComplete="off"
            placeholder="留空保留原 key"
            value={apiKey}
            onChange={(e) => setApiKey(e.target.value)}
          />
        </label>
        <label className="settings-field settings-field-toggle">
          <span className="settings-field-label">启用</span>
          <input
            type="checkbox"
            className="settings-toggle"
            checked={enabled}
            onChange={(e) => setEnabled(e.target.checked)}
            aria-label="启用该渠道"
          />
        </label>
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
