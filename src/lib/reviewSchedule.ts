// 复盘到期判断——前端 dueReviewRecords memo 用。
// 调度本体在后端 scheduler.rs，前端这层仅用于显示"有 N 条待复盘"。

import type { AnalysisRecord } from "../types";

function nextReviewAt(record: AnalysisRecord) {
  if (record.review && !record.review.nextReviewAt) return null;
  return record.review?.nextReviewAt ?? record.nextReviewAt ?? null;
}

export function isReviewDue(record: AnalysisRecord) {
  const dueAt = nextReviewAt(record);
  if (!dueAt) return false;
  const dueTime = Date.parse(dueAt);
  return !Number.isNaN(dueTime) && dueTime <= Date.now();
}

/** 在到期 records 里挑最早到期的——和后端 scheduler::pick_earliest_due 同语义 */
export function pickEarliestDue(records: AnalysisRecord[]): AnalysisRecord | null {
  let earliest: AnalysisRecord | null = null;
  let earliestMs = Infinity;
  for (const record of records) {
    const dueAt = nextReviewAt(record);
    if (!dueAt) continue;
    const t = Date.parse(dueAt);
    if (Number.isNaN(t) || t > Date.now()) continue;
    if (t < earliestMs) {
      earliestMs = t;
      earliest = record;
    }
  }
  return earliest;
}
