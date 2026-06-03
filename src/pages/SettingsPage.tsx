// SettingsPage — Agent 模型渠道管理。
//
// Spec: docs/design/frontend-design.md §设置页 + docs/design/agent-infra-module.md §2 ProviderChannel
//
// 结构：
//   PageShell
//     ─ 当前模型选择器（单选，run 走选中渠道）
//     ─ 已配置渠道列表（每个保留模型一行）
//     ─ 添加渠道：[快速预设 | 自定义] → 发现模型 / 手动降级 → 每模型一条渠道
//
// 数据流：
//   - mount: agentChannelPresets() + agentListChannels()
//   - 切当前模型 → agentSetActiveChannel → refetch
//   - 删除 → agentRemoveChannel → refetch
//   - 添加（AddChannelForm 内部 agentAddChannel）→ onSaved → refetch
//
// 红线：全部走 specta 强类型 commands；apiKey 永不回显（列表只显示 apiKeySet）。

import { useCallback, useEffect, useState } from "react";
import { PageShell } from "../components/PageShell";
import {
  commands,
  type ChannelPresetView,
  type ProviderChannelView,
} from "../bindings";
import { AddChannelForm } from "./settings/AddChannelForm";
import { ChannelList } from "./settings/ChannelList";
import { CurrentModelSelector } from "./settings/CurrentModelSelector";
import { formatError } from "./settings/types";

export default function SettingsPage() {
  const [presets, setPresets] = useState<ChannelPresetView[]>([]);
  const [channels, setChannels] = useState<ProviderChannelView[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  // active 切换 / 删除时禁用对应控件，避免并发请求。
  const [mutating, setMutating] = useState(false);

  const loadChannels = useCallback(async () => {
    const res = await commands.agentListChannels();
    if (res.status === "ok") {
      setChannels(res.data);
      setError(null);
    } else {
      setError(formatError(res.error));
    }
  }, []);

  // mount：拉预设 + 渠道列表。
  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    void Promise.all([
      commands.agentChannelPresets(),
      commands.agentListChannels(),
    ]).then(([presetRes, chRes]) => {
      if (cancelled) return;
      if (presetRes.status === "ok") setPresets(presetRes.data);
      else setError(formatError(presetRes.error));
      if (chRes.status === "ok") setChannels(chRes.data);
      else setError(formatError(chRes.error));
      setLoading(false);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  const handleSetActive = useCallback(
    async (channelId: string) => {
      setMutating(true);
      const res = await commands.agentSetActiveChannel(channelId);
      if (res.status === "error") {
        setError(formatError(res.error));
      } else {
        await loadChannels();
      }
      setMutating(false);
    },
    [loadChannels],
  );

  const handleRemove = useCallback(
    async (channelId: string) => {
      setMutating(true);
      const res = await commands.agentRemoveChannel(channelId);
      if (res.status === "error") {
        setError(formatError(res.error));
      } else {
        await loadChannels();
      }
      setMutating(false);
    },
    [loadChannels],
  );

  // 添加成功后 refetch（首条由后端自动设 active，列表自然反映）。
  const handleSaved = useCallback(() => {
    void loadChannels();
  }, [loadChannels]);

  const status = error
    ? `加载失败：${error}`
    : loading
      ? "加载中"
      : channels.length === 0
        ? "未配置渠道"
        : `${channels.length} 个渠道`;
  const statusTone = error
    ? "error"
    : loading
      ? "loading"
      : channels.length === 0
        ? "stale"
        : "ok";

  return (
    <PageShell title="设置" status={status} statusTone={statusTone}>
      <div className="settings-workspace">
        {/* 当前模型 */}
        <section className="settings-section">
          <header className="settings-section-head">
            <h2 className="settings-section-title">当前模型</h2>
            <span className="muted settings-section-hint">
              Agent run 走选中渠道
            </span>
          </header>
          <div className="settings-section-body">
            <CurrentModelSelector
              channels={channels}
              onSelect={handleSetActive}
              busy={mutating}
            />
          </div>
        </section>

        {/* 渠道列表 — 行 full-bleed，不包 body */}
        <section className="settings-section">
          <header className="settings-section-head">
            <h2 className="settings-section-title">模型渠道</h2>
            {channels.length > 0 && (
              <span className="muted settings-section-hint">
                {channels.length} 个
              </span>
            )}
          </header>
          <ChannelList
            channels={channels}
            onRemove={handleRemove}
            busy={mutating}
          />
        </section>

        {/* 添加渠道 */}
        <section className="settings-section">
          <header className="settings-section-head">
            <h2 className="settings-section-title">添加渠道</h2>
          </header>
          <div className="settings-section-body">
            <AddChannelForm presets={presets} onSaved={handleSaved} />
          </div>
        </section>
      </div>
    </PageShell>
  );
}
