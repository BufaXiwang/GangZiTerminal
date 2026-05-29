// ChannelList — 已配置渠道列表。
//
// Spec: docs/design/frontend-design.md §设置页 — 渠道/模型列表（每个保留模型一行）
//
// 每行：`{model} ({provider})` + wireFormat badge + host + 已配置 key 状态 +
//        enabled / active 标记 + 删除。apiKey 永不展示明文（只显示 apiKeySet）。

import { Check, Trash2 } from "lucide-react";
import type { ProviderChannelView } from "../../bindings";
import { WIRE_FORMAT_LABEL, hostOf } from "./types";

interface ChannelListProps {
  channels: ProviderChannelView[];
  onRemove: (channelId: string) => void;
  busy?: boolean;
}

export function ChannelList({ channels, onRemove, busy }: ChannelListProps) {
  if (channels.length === 0) {
    return (
      <div className="settings-empty muted">
        还没有配置渠道。使用下方「添加渠道」连接服务商并发现模型。
      </div>
    );
  }

  return (
    <ul className="settings-channel-list">
      {channels.map((ch) => (
        <li
          key={ch.channelId}
          className={`settings-channel-row${ch.isActive ? " active" : ""}`}
        >
          <div className="settings-channel-main">
            <span className="settings-channel-name">
              <span className="settings-channel-model">{ch.model}</span>
              <span className="muted"> ({ch.provider})</span>
            </span>
            {ch.isActive && (
              <span className="chip active settings-active-chip" title="当前模型">
                <Check size={12} strokeWidth={2} /> 当前
              </span>
            )}
          </div>

          <div className="settings-channel-meta">
            <span className="chip settings-wire-badge" title="消息格式">
              {WIRE_FORMAT_LABEL[ch.wireFormat]}
            </span>
            <span className="settings-channel-host tabular" title={ch.baseUrl ?? ""}>
              {hostOf(ch.baseUrl)}
            </span>
            <span
              className={`settings-key-status${ch.apiKeySet ? " set" : ""}`}
              title={ch.apiKeySet ? "已配置 API Key" : "未配置 API Key"}
            >
              {ch.apiKeySet ? "已配置 key" : "无 key"}
            </span>
            {!ch.enabled && <span className="settings-disabled-tag">已禁用</span>}
          </div>

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
        </li>
      ))}
    </ul>
  );
}
