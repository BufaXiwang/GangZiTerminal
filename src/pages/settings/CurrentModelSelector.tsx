// CurrentModelSelector — 当前模型选择器（单选，run 走选中渠道）。
//
// Spec: docs/design/frontend-design.md §设置页 — 当前模型选择器；文案 `{model} ({渠道名})`
//
// 渠道少时用 segmented，多时退化为原生 select，避免横向溢出。

import type { ProviderChannelView } from "../../bindings";

interface CurrentModelSelectorProps {
  channels: ProviderChannelView[];
  /** 切换后调 agentSetActiveChannel(channelId) → 由父组件 refetch。 */
  onSelect: (channelId: string) => void;
  busy?: boolean;
}

function label(ch: ProviderChannelView): string {
  return `${ch.model} (${ch.provider})`;
}

export function CurrentModelSelector({
  channels,
  onSelect,
  busy,
}: CurrentModelSelectorProps) {
  if (channels.length === 0) {
    return (
      <div className="settings-current-empty muted">
        还没有可用渠道，先在下方「添加渠道」配置一个。
      </div>
    );
  }

  const activeId =
    channels.find((c) => c.isActive)?.channelId ?? channels[0].channelId;

  // 多于 4 个用 select，避免 segmented 横向溢出。
  if (channels.length > 4) {
    return (
      <select
        className="settings-current-select"
        value={activeId}
        disabled={busy}
        onChange={(e) => onSelect(e.target.value)}
        aria-label="当前模型"
      >
        {channels.map((ch) => (
          <option key={ch.channelId} value={ch.channelId}>
            {label(ch)}
          </option>
        ))}
      </select>
    );
  }

  return (
    <div className="segmented" role="radiogroup" aria-label="当前模型">
      {channels.map((ch) => {
        const active = ch.channelId === activeId;
        return (
          <button
            key={ch.channelId}
            type="button"
            role="radio"
            aria-checked={active}
            className={`segmented-item${active ? " active" : ""}`}
            disabled={busy || active}
            onClick={() => onSelect(ch.channelId)}
            title={label(ch)}
          >
            {label(ch)}
          </button>
        );
      })}
    </div>
  );
}
