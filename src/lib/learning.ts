import type { AnalysisRecord, LearningProfile, SimulatedPosition, StockQuote } from "../types";
import { calculateSimulationAccount } from "./simulation";

export function buildLearningProfile(
  records: AnalysisRecord[],
  positions: SimulatedPosition[],
  quotes: StockQuote[],
  initialCash: number,
): LearningProfile {
  const reviewed = records.filter((record) => record.review);
  const statusCounts = reviewed.reduce(
    (acc, record) => {
      const status = record.review?.thesisStatus ?? "inconclusive";
      acc[status] += 1;
      return acc;
    },
    { validated: 0, invalidated: 0, watching: 0, inconclusive: 0 },
  );
  const account = calculateSimulationAccount(initialCash, positions, quotes);
  const topThemes = rankText([
    ...records.flatMap((record) => record.result.themes),
    ...records.flatMap((record) => record.result.sectors),
    ...records.flatMap((record) => record.result.relatedStocks.filter((item) => !/^\d{6}$/.test(item))),
  ]).slice(0, 6);
  const commonMistakes = rankText(reviewed.flatMap((record) => record.review?.mistakes ?? []))
    .slice(0, 5)
    .map((item) => ({ text: item.name, count: item.count }));
  const recentLearningUpdates = reviewed
    .slice(0, 8)
    .map((record) => record.review?.learningUpdate)
    .filter((text): text is string => Boolean(text));
  const reviewRate = records.length ? reviewed.length / records.length : 0;
  const decisiveReviews = statusCounts.validated + statusCounts.invalidated;
  const validationRate = decisiveReviews ? statusCounts.validated / decisiveReviews : 0;
  const score = Math.round(
    Math.min(100, records.length * 1.4 + reviewRate * 28 + validationRate * 18 + Math.max(-10, Math.min(12, account.totalPnl / 100))),
  );

  return {
    level: Math.max(1, Math.min(20, Math.floor(score / 8) + 1)),
    score,
    totalRecords: records.length,
    reviewedRecords: reviewed.length,
    reviewRate,
    validatedCount: statusCounts.validated,
    invalidatedCount: statusCounts.invalidated,
    watchingCount: statusCounts.watching,
    inconclusiveCount: statusCounts.inconclusive,
    validationRate,
    topThemes,
    commonMistakes,
    recentLearningUpdates,
    focusSuggestions: buildFocusSuggestions(records, reviewRate, validationRate, commonMistakes),
    updatedAt: new Date().toISOString(),
  };
}

function buildFocusSuggestions(
  records: AnalysisRecord[],
  reviewRate: number,
  validationRate: number,
  mistakes: Array<{ text: string; count: number }>,
) {
  const suggestions: string[] = [];
  if (records.length === 0) suggestions.push("先积累 5 条事件分析，建立可复盘样本。");
  if (records.length > 0 && reviewRate < 0.35) suggestions.push("提高复盘完成率，把验证清单逐条回看。");
  if (reviewRate >= 0.35 && validationRate < 0.45) suggestions.push("减少过度映射，优先验证公告、成交量和板块强弱。");
  if (mistakes[0]) suggestions.push(`重点修正：${mistakes[0].text}`);
  if (suggestions.length === 0) suggestions.push("继续扩大主题样本，比较不同事件类型的验证表现。");
  return suggestions.slice(0, 4);
}

function rankText(items: string[]) {
  const map = new Map<string, number>();
  items
    .map((item) => item.trim())
    .filter(Boolean)
    .forEach((item) => map.set(item, (map.get(item) ?? 0) + 1));
  return Array.from(map.entries())
    .map(([name, count]) => ({ name, count }))
    .sort((a, b) => b.count - a.count || a.name.localeCompare(b.name, "zh-CN"));
}
