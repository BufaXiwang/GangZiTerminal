// HostLatencyPopup — 数据源(TDX)连接延时诊断弹层。
//
// Spec: docs/design/frontend-design.md §4 市场页（数据源连接延时诊断 popup）
//
// 打开时调 commands.probeTdxHosts()（探测 ~3s，期间 loading），返回 HostProbe[]
// （后端已按 可达+延迟 排序），逐行展示 站名 / host:port / 延时 / 状态点，
// inPool=true 的行高亮并加「在用」徽章。支持重新探测；点遮罩 / Esc / X 关闭。

import { useCallback, useEffect, useState } from "react";
import { X } from "lucide-react";
import { commands, type HostProbe } from "../../bindings";

interface HostLatencyPopupProps {
  onClose: () => void;
}

export function HostLatencyPopup({ onClose }: HostLatencyPopupProps) {
  const [hosts, setHosts] = useState<HostProbe[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const probe = useCallback(() => {
    setLoading(true);
    setError(null);
    void commands.probeTdxHosts().then((res) => {
      if (res.status === "ok") {
        setHosts(res.data);
      } else {
        setError(
          `${res.error.code}${res.error.message ? `: ${res.error.message}` : ""}`,
        );
      }
      setLoading(false);
    });
  }, []);

  // 打开即探测一次
  useEffect(() => {
    probe();
  }, [probe]);

  // Esc 关闭
  useEffect(() => {
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", handleKey);
    return () => document.removeEventListener("keydown", handleKey);
  }, [onClose]);

  const reachable = hosts?.filter((h) => h.ok) ?? [];
  const inPoolCount = hosts?.filter((h) => h.inPool).length ?? 0;
  const fastest = reachable.reduce<number | null>((min, h) => {
    if (h.latencyMs == null) return min;
    return min == null || h.latencyMs < min ? h.latencyMs : min;
  }, null);

  return (
    <div
      className="host-latency-backdrop"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="host-latency-modal" role="dialog" aria-label="数据源连接延时">
        <div className="host-latency-header">
          <span className="host-latency-title">数据源连接延时</span>
          <button
            type="button"
            className="host-latency-close"
            title="关闭"
            onClick={onClose}
          >
            <X size={15} />
          </button>
        </div>

        {hosts && hosts.length > 0 && (
          <div className="host-latency-summary">
            共 {hosts.length} 台 · 在用 {inPoolCount} 台
            {fastest != null && (
              <>
                {" "}
                · 最快 <span className="tabular">{fastest}</span>ms
              </>
            )}
          </div>
        )}

        <div className="host-latency-body">
          {loading ? (
            <div className="host-latency-status">探测中…</div>
          ) : error ? (
            <div className="host-latency-status host-latency-error">
              探测失败：{error}
            </div>
          ) : hosts && hosts.length > 0 ? (
            <ul className="host-latency-list">
              {hosts.map((h) => (
                <li
                  key={`${h.host}:${h.port}`}
                  className={`host-latency-row ${h.inPool ? "in-pool" : ""}`}
                >
                  <span
                    className={`host-latency-dot ${h.ok ? "ok" : "down"}`}
                    aria-hidden
                  />
                  <span className="host-latency-name" title={h.name}>
                    {h.name}
                  </span>
                  <span className="host-latency-addr">
                    {h.host}:{h.port}
                  </span>
                  {h.inPool && <span className="host-latency-badge">在用</span>}
                  <span className="host-latency-ms">
                    {h.ok && h.latencyMs != null ? (
                      <>
                        <span className="tabular">{h.latencyMs}</span>
                        <span className="host-latency-unit">ms</span>
                      </>
                    ) : (
                      <span className="host-latency-timeout">超时</span>
                    )}
                  </span>
                </li>
              ))}
            </ul>
          ) : (
            <div className="host-latency-status">暂无数据</div>
          )}
        </div>

        <div className="host-latency-footer">
          <button
            type="button"
            className="btn"
            onClick={probe}
            disabled={loading}
          >
            {loading ? "探测中…" : "重新探测"}
          </button>
        </div>
      </div>
    </div>
  );
}
