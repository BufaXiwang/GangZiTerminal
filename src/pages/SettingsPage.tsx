// SettingsPage — Agent 服务商渠道管理（统一设置页）。
//
// Spec: docs/design/frontend-design.md §设置页 + docs/design/agent-infra-module.md §2 ProviderChannel
//
// 布局（上→下）：
//   1. 服务商管理 — 按连接分组摘要 + 编辑/发现/删除 + 折叠式添加表单
//
// 数据流：
//   - mount: agentChannelPresets() + agentListChannels()
//   - 删除 → agentRemoveChannel → refetch
//   - 添加（AddChannelForm 内部 agentAddChannel）→ onSaved → refetch
//
// 模型选择已移至 AgentPage sidebar（更贴近使用场景）。
//
// 红线：全部走 specta 强类型 commands；apiKey 永不回显（列表只显示 apiKeySet）。

import { Pencil, Search, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { PageShell } from "../components/PageShell";
import {
  commands,
  type ChannelPresetView,
  type ProviderChannelView,
} from "../bindings";
import { AddChannelForm } from "./settings/AddChannelForm";
import {
  type ChannelGroup,
  groupChannels,
  channelStats,
  GroupEditForm,
  DiscoverMorePanel,
} from "./settings/ChannelList";
import {
  WIRE_FORMAT_LABEL,
  formatError,
  hostOf,
  providerInitial,
} from "./settings/types";

export default function SettingsPage() {
  const [presets, setPresets] = useState<ChannelPresetView[]>([]);
  const [channels, setChannels] = useState<ProviderChannelView[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  // active 切换 / 删除时禁用对应控件，避免并发请求。
  const [mutating, setMutating] = useState(false);

  // 添加服务商展开状态
  const [addOpen, setAddOpen] = useState(false);
  // 编辑 modal
  const [editingGroupKey, setEditingGroupKey] = useState<string | null>(null);
  // 发现更多模型 modal
  const [discoverGroupKey, setDiscoverGroupKey] = useState<string | null>(null);

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

  const groups = useMemo(() => groupChannels(channels), [channels]);
  const hasChannels = channels.length > 0;

  // 没有服务商时自动展开添加表单
  useEffect(() => {
    if (!loading && !hasChannels) setAddOpen(true);
  }, [loading, hasChannels]);

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

  const handleRemoveGroup = useCallback(
    async (group: ChannelGroup) => {
      setMutating(true);
      for (const ch of group.channels) {
        const res = await commands.agentRemoveChannel(ch.channelId);
        if (res.status === "error") {
          setError(formatError(res.error));
          setMutating(false);
          return;
        }
      }
      await loadChannels();
      setMutating(false);
    },
    [loadChannels],
  );

  // 添加成功后 refetch（首条由后端自动设 active，列表自然反映）。
  const handleSaved = useCallback(() => {
    void loadChannels();
  }, [loadChannels]);

  const stats = channelStats(channels);
  const status = error
    ? `加载失败：${error}`
    : loading
      ? "加载中"
      : channels.length === 0
        ? "未配置渠道"
        : `${stats.groupCount} 个服务商 · ${stats.modelCount} 个模型`;
  const statusTone = error
    ? "error"
    : loading
      ? "loading"
      : channels.length === 0
        ? "stale"
        : "ok";

  // Modal group targets
  const editingGroup = editingGroupKey
    ? groups.find((g) => g.key === editingGroupKey) ?? null
    : null;
  const discoverGroup = discoverGroupKey
    ? groups.find((g) => g.key === discoverGroupKey) ?? null
    : null;

  return (
    <PageShell title="设置" status={status} statusTone={statusTone}>
      <div className="settings-page">
        {/* --- 服务商 --- */}
        <section className="settings-block">
          <div className="settings-block-header">
            <h2 className="settings-block-title">
              {hasChannels ? "服务商" : "配置服务商"}
            </h2>
            {hasChannels && !addOpen && (
              <button
                type="button"
                className="btn ghost"
                onClick={() => setAddOpen(true)}
              >
                + 添加
              </button>
            )}
          </div>

          {/* 已有服务商列表 */}
          {groups.map((group) => (
            <div key={group.key} className="settings-provider-row">
              <div className="settings-provider-info">
                <span className="settings-avatar" aria-hidden>
                  {providerInitial(group.provider)}
                </span>
                <div className="settings-provider-text">
                  <div className="provider-name">{group.provider}</div>
                  <div className="provider-meta muted">
                    {WIRE_FORMAT_LABEL[group.wireFormat]}
                    {" · "}
                    {hostOf(group.baseUrl)}
                    {" · "}
                    {group.apiKeySet ? (
                      <span className="provider-key-set">已配置</span>
                    ) : (
                      <span className="provider-key-unset">无 key</span>
                    )}
                    {" · "}
                    {group.channels.length} 个模型
                  </div>
                </div>
              </div>
              <div className="settings-provider-actions">
                <button
                  type="button"
                  onClick={() => {
                    setDiscoverGroupKey(null);
                    setEditingGroupKey(group.key);
                  }}
                  disabled={mutating}
                  title="编辑连接"
                >
                  <Pencil size={12} /> 编辑
                </button>
                <button
                  type="button"
                  onClick={() => {
                    setEditingGroupKey(null);
                    setDiscoverGroupKey(group.key);
                  }}
                  disabled={mutating}
                  title="发现更多模型"
                >
                  <Search size={12} /> 发现模型
                </button>
                <button
                  type="button"
                  onClick={() => {
                    if (
                      confirm(
                        `确认删除服务商「${group.provider}」下的全部 ${group.channels.length} 个模型？`,
                      )
                    ) {
                      void handleRemoveGroup(group);
                    }
                  }}
                  disabled={mutating}
                  title="删除整个服务商"
                >
                  <Trash2 size={12} /> 删除
                </button>
              </div>
            </div>
          ))}

          {/* 添加服务商 (折叠) */}
          {addOpen && (
            <div className="settings-add-provider">
              <AddChannelForm
                presets={presets}
                onSaved={() => {
                  setAddOpen(false);
                  handleSaved();
                }}
              />
              {hasChannels && (
                <button
                  type="button"
                  className="btn ghost settings-add-cancel"
                  onClick={() => setAddOpen(false)}
                >
                  取消
                </button>
              )}
            </div>
          )}

          {!hasChannels && !addOpen && (
            <p className="muted settings-empty-hint">
              连接 LLM 服务商以启用 Agent。
            </p>
          )}
        </section>
      </div>

      {/* --- Edit modal --- */}
      {editingGroup && (
        <div
          className="settings-modal-backdrop"
          onClick={() => setEditingGroupKey(null)}
        >
          <div
            className="settings-modal"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="settings-modal-header">
              <h3>编辑服务商 — {editingGroup.provider}</h3>
              <button
                className="settings-modal-close"
                onClick={() => setEditingGroupKey(null)}
              >
                <X size={18} />
              </button>
            </div>
            <GroupEditForm
              group={editingGroup}
              onCancel={() => setEditingGroupKey(null)}
              onSaved={() => {
                setEditingGroupKey(null);
                handleSaved();
              }}
            />
          </div>
        </div>
      )}

      {/* --- Discover modal --- */}
      {discoverGroup && (
        <div
          className="settings-modal-backdrop"
          onClick={() => setDiscoverGroupKey(null)}
        >
          <div
            className="settings-modal"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="settings-modal-header">
              <h3>发现模型 — {discoverGroup.provider}</h3>
              <button
                className="settings-modal-close"
                onClick={() => setDiscoverGroupKey(null)}
              >
                <X size={18} />
              </button>
            </div>
            <div className="settings-modal-discover-body">
              <DiscoverMorePanel
                group={discoverGroup}
                existingModels={new Set(channels.map((c) => c.model))}
                onCancel={() => setDiscoverGroupKey(null)}
                onSaved={() => {
                  setDiscoverGroupKey(null);
                  handleSaved();
                }}
              />
            </div>
          </div>
        </div>
      )}
    </PageShell>
  );
}
