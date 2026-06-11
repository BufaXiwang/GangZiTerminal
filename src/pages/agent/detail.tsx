// Agent 页右侧/弹层的详情组件：分析结果详情、复盘报告、策略编辑弹窗。

import { useCallback, useEffect, useMemo, useState } from "react";
import { X } from "lucide-react";
import { KlineModal } from "../../components/KlineModal";
import { renderMarkdown } from "../../lib/simpleMarkdown";
import {
  commands,
  type AgentRun,
  type AnalysisResult,
  type InvestmentStrategy,
  type StrategyHistoryEntry,
} from "../../bindings";
import { fmtTime, summarizeLine } from "./chatModel";

/* ---------- inline sub-components ---------- */

export function AnalysisDetail({ data, runs }: { data: AnalysisResult; runs: AgentRun[] }) {
  const [codeInfo, setCodeInfo] = useState<Map<string, { name: string; pct?: number }>>(new Map());
  const [klineCode, setKlineCode] = useState<{ tsCode: string; name?: string } | null>(null);
  const [newsItems, setNewsItems] = useState<Array<{id: string; title: string; source?: string}>>([]);

  useEffect(() => {
    if (!data.relatedCodes?.length) return;
    commands.fetchData({
      tsCodes: data.relatedCodes,
      include: { quote: true },
      limit: null,
    }).then(res => {
      if (res.status === "ok") {
        const map = new Map<string, { name: string; pct?: number }>();
        for (const item of res.data.items) {
          map.set(item.tsCode, {
            name: item.name ?? item.tsCode,
            pct: item.quote?.changePercent ?? undefined,
          });
        }
        setCodeInfo(map);
      }
    });
  }, [data.relatedCodes]);

  // Find related news IDs from the run's trigger (news_batch mode).
  const relatedRun = useMemo(
    () => runs.find(r => r.runId === data.runId),
    [runs, data.runId],
  );
  const newsIds = useMemo(() => {
    if (!relatedRun) return null;
    const trigger = relatedRun.trigger as Record<string, unknown>;
    if (trigger?.kind === "news_batch" && Array.isArray(trigger.newsIds)) {
      return trigger.newsIds as string[];
    }
    return null;
  }, [relatedRun]);

  // Fetch actual news items when newsIds are available.
  useEffect(() => {
    if (!newsIds?.length) {
      setNewsItems([]);
      return;
    }
    commands.fetchNews({
      ids: newsIds,
      limit: newsIds.length,
      query: null,
      sources: null,
      publishedFrom: null,
      publishedTo: null,
      includeArticle: null,
      offset: null,
      order: null,
    }).then(res => {
      if (res.status === "ok") {
        setNewsItems(res.data.items.map(n => ({
          id: n.id,
          title: n.title ?? n.id,
          source: n.source,
        })));
      }
    });
  }, [newsIds]);

  return (
    <div className="agent-analysis-detail">
      <div className="detail-field">
        <span className="detail-label">判定</span>
        <span className={`detail-kind kind-${data.kind}`}>
          {data.kind === "action" ? "操作" : "观望"}
        </span>
      </div>
      <div className="detail-field">
        <span className="detail-label">摘要</span>
        <div className="md-content" dangerouslySetInnerHTML={{ __html: renderMarkdown(data.summary) }} />
      </div>
      {data.relatedCodes?.length > 0 && (
        <div className="detail-field">
          <span className="detail-label">相关标的</span>
          <div className="detail-codes">
            {data.relatedCodes.map((code: string) => {
              const info = codeInfo.get(code);
              const pct = info?.pct;
              const pctClass = pct != null ? (pct > 0 ? "up" : pct < 0 ? "down" : "flat") : "";
              return (
                <button
                  key={code}
                  type="button"
                  className="detail-code-link"
                  onClick={() => setKlineCode({ tsCode: code, name: info?.name })}
                >
                  <span>{info?.name ?? ""} {code}</span>
                  {pct != null && (
                    <span className={`detail-code-pct ${pctClass}`}>
                      {pct > 0 ? "+" : ""}{pct.toFixed(2)}%
                    </span>
                  )}
                </button>
              );
            })}
          </div>
        </div>
      )}
      <KlineModal
        open={!!klineCode}
        tsCode={klineCode?.tsCode ?? null}
        name={klineCode?.name}
        onClose={() => setKlineCode(null)}
      />
      {data.tradeIds?.length > 0 && (
        <div className="detail-field">
          <span className="detail-label">关联交易</span>
          <span className="tabular">{data.tradeIds.join(", ")}</span>
        </div>
      )}
      {/* Related news: show titles fetched from backend */}
      {newsItems.length > 0 && (
        <div className="detail-field">
          <span className="detail-label">触发新闻（{newsItems.length} 条）</span>
          <div className="detail-news-list">
            {newsItems.map(n => (
              <div key={n.id} className="detail-news-item">
                {n.source && <span className="detail-news-source">{n.source}</span>}
                <span className="detail-news-title">{n.title}</span>
              </div>
            ))}
          </div>
        </div>
      )}
      <div className="detail-field">
        <span className="detail-label">时间</span>
        <span>{fmtTime(data.createdAt)}</span>
      </div>
    </div>
  );
}

export function ReportDetail({ name, path }: { name: string; path: string }) {
  const [content, setContent] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  useEffect(() => {
    commands.agentReadReviewReport(path).then((res) => {
      if (res.status === "ok") {
        setContent(res.data);
      } else {
        setLoadError(res.error.message ?? "读取失败");
      }
    });
  }, [path]);

  if (loadError) {
    return (
      <div className="agent-report-detail">
        <p className="muted">{name} — {loadError}</p>
      </div>
    );
  }
  if (content === null) {
    return (
      <div className="agent-report-detail">
        <p className="muted">加载中…</p>
      </div>
    );
  }
  // Strip the run_id line from report markdown before rendering.
  const cleaned = content.replace(/^-\s*run_id:.*$/m, "").trim();
  return (
    <div className="agent-report-detail">
      <div className="md-content" dangerouslySetInnerHTML={{ __html: renderMarkdown(cleaned) }} />
    </div>
  );
}

export function StrategyModal({
  strategy,
  history,
  onClose,
}: {
  strategy: InvestmentStrategy | null;
  history: StrategyHistoryEntry[] | null;
  onClose: () => void;
}) {
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  return (
    <div className="agent-modal-backdrop" onClick={onClose}>
      <div className="agent-modal" onClick={(e) => e.stopPropagation()}>
        <div className="agent-modal-header">
          <h2>投资策略{strategy ? ` V${strategy.version}` : ""}</h2>
          <span className="muted" style={{ fontSize: 12 }}>
            通过对话修改
          </span>
          <button className="agent-modal-close" onClick={onClose}>
            <X size={18} />
          </button>
        </div>
        <div className="agent-modal-body">
          <div className="md-content" dangerouslySetInnerHTML={{ __html: renderMarkdown(strategy?.strategy ?? "（未设置策略）") }} />
          {history && history.length > 0 && (
            <div className="agent-strategy-history">
              <h4 className="agent-strategy-history-title">版本历史</h4>
              {history.map((h) => (
                <div key={h.version} className="agent-strategy-history-item">
                  <span className="tabular">V{h.version}</span>
                  <span className="muted">{h.updatedAt}</span>
                  <span className="muted">{h.reason}</span>
                </div>
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

