// newsWindow 纯函数测试 — 双向 keyset 窗口流核心状态机。
// Spec: news-module.md §4 line 281-287（去重 / 窗口有界 / 游标 / hasMore）。

import { describe, it, expect } from "vitest";
import {
  mergeOlder,
  mergeNewer,
  clampWindow,
  replaceWindow,
  oldestCursor,
  newestCursor,
  hasMoreFromBatch,
  type WindowItem,
} from "./newsWindow";

// 倒序时间线辅助：id=n 的条目，publishedAt 随 n 递减（n 越大越新）。
function item(id: string, publishedAt: string | null = `2026-06-03T10:00:00Z`): WindowItem {
  return { id, publishedAt };
}

describe("mergeOlder（向更早 append + 去重）", () => {
  it("把新一页 append 到尾部", () => {
    const prev = [item("3"), item("2")];
    const batch = [item("1"), item("0")];
    const out = mergeOlder(prev, batch);
    expect(out.map((x) => x.id)).toEqual(["3", "2", "1", "0"]);
  });

  it("闭区间边界条同 id 不重复（batch 带回边界条 '2'）", () => {
    const prev = [item("3"), item("2")];
    const batch = [item("2"), item("1")]; // '2' 是边界重复
    const out = mergeOlder(prev, batch);
    expect(out.map((x) => x.id)).toEqual(["3", "2", "1"]);
  });

  it("不修改入参", () => {
    const prev = [item("3")];
    const batch = [item("2")];
    mergeOlder(prev, batch);
    expect(prev.map((x) => x.id)).toEqual(["3"]);
    expect(batch.map((x) => x.id)).toEqual(["2"]);
  });
});

describe("mergeNewer（向更新 reverse + prepend + 去重）", () => {
  it("升序批 reverse 成倒序后 prepend 到头部", () => {
    const prev = [item("2"), item("1")];
    const ascBatch = [item("3"), item("4")]; // 升序（4 最新）
    const out = mergeNewer(prev, ascBatch);
    // reverse → [4,3]，prepend → [4,3,2,1]
    expect(out.map((x) => x.id)).toEqual(["4", "3", "2", "1"]);
  });

  it("闭区间边界条同 id 不重复（asc 批带回边界条 '2'）", () => {
    const prev = [item("2"), item("1")];
    const ascBatch = [item("2"), item("3")]; // 升序，'2' 是边界重复
    const out = mergeNewer(prev, ascBatch);
    // reverse → [3,2]，去掉已存在的 2 → [3]，prepend → [3,2,1]
    expect(out.map((x) => x.id)).toEqual(["3", "2", "1"]);
  });

  it("全是边界重复条时长度不变（无新内容信号）", () => {
    const prev = [item("2"), item("1")];
    const ascBatch = [item("1"), item("2")];
    const out = mergeNewer(prev, ascBatch);
    expect(out.length).toBe(prev.length);
    expect(out.map((x) => x.id)).toEqual(["2", "1"]);
  });

  it("不修改入参", () => {
    const prev = [item("2")];
    const ascBatch = [item("3"), item("4")];
    mergeNewer(prev, ascBatch);
    expect(ascBatch.map((x) => x.id)).toEqual(["3", "4"]);
  });
});

describe("clampWindow（窗口有界裁剪）", () => {
  const five = [item("5"), item("4"), item("3"), item("2"), item("1")];

  it("未超 max：原样返回，trimmed=false", () => {
    const { items, trimmed } = clampWindow(five, 10, "head");
    expect(trimmed).toBe(false);
    expect(items).toBe(five); // 同一引用
  });

  it("裁头部（side='head' = 最新端），保留尾部 max 条，trimmed=true", () => {
    const { items, trimmed } = clampWindow(five, 3, "head");
    expect(trimmed).toBe(true);
    expect(items.map((x) => x.id)).toEqual(["3", "2", "1"]);
  });

  it("裁尾部（side='tail' = 最早端），保留头部 max 条，trimmed=true", () => {
    const { items, trimmed } = clampWindow(five, 3, "tail");
    expect(trimmed).toBe(true);
    expect(items.map((x) => x.id)).toEqual(["5", "4", "3"]);
  });

  it("恰好 == max：不裁", () => {
    const { trimmed } = clampWindow(five, 5, "head");
    expect(trimmed).toBe(false);
  });
});

describe("游标取值 oldestCursor / newestCursor", () => {
  it("含 publishedAt：取末/首条", () => {
    const items = [
      { id: "2", publishedAt: "2026-06-03T12:00:00Z" },
      { id: "1", publishedAt: "2026-06-03T09:00:00Z" },
    ];
    expect(newestCursor(items)).toBe("2026-06-03T12:00:00Z");
    expect(oldestCursor(items)).toBe("2026-06-03T09:00:00Z");
  });

  it("空窗口返回 null", () => {
    expect(newestCursor([])).toBeNull();
    expect(oldestCursor([])).toBeNull();
  });

  it("末条缺 publishedAt → oldestCursor 返回 null（停止下扩）", () => {
    const items = [
      { id: "2", publishedAt: "2026-06-03T12:00:00Z" },
      { id: "1", publishedAt: null },
    ];
    expect(oldestCursor(items)).toBeNull();
    expect(newestCursor(items)).toBe("2026-06-03T12:00:00Z");
  });

  it("首条缺 publishedAt → newestCursor 返回 null", () => {
    const items = [
      { id: "2", publishedAt: undefined },
      { id: "1", publishedAt: "2026-06-03T09:00:00Z" },
    ];
    expect(newestCursor(items)).toBeNull();
  });
});

describe("hasMoreFromBatch（满页判定）", () => {
  it("批长 == pageSize → true", () => {
    expect(hasMoreFromBatch(50, 50)).toBe(true);
  });
  it("批长 < pageSize → false", () => {
    expect(hasMoreFromBatch(12, 50)).toBe(false);
  });
  it("批长 > pageSize（防御）→ true", () => {
    expect(hasMoreFromBatch(60, 50)).toBe(true);
  });
});

describe("replaceWindow（锚定替换）", () => {
  it("整体替换为新批", () => {
    const batch = [item("9"), item("8")];
    expect(replaceWindow(batch)).toBe(batch);
  });
});

// === 组合场景：模拟 handler 的「裁剪端 → 该端 hasMore 置回 true」语义 ===
describe("组合：超 MAX_WINDOW 裁剪后被裁端 hasMore=true", () => {
  const MAX = 4;

  it("向更早扩展超限 → 裁头部（最新端），最新端 hasMoreNewer 应置 true", () => {
    const prev = [item("4"), item("3"), item("2"), item("1")]; // 已满 4
    const batch = [item("0")]; // 向更早一页
    const merged = mergeOlder(prev, batch); // [4,3,2,1,0] 超限
    const { items, trimmed } = clampWindow(merged, MAX, "head");
    expect(trimmed).toBe(true); // → handler 把 hasMoreNewer 置回 true
    expect(items.map((x) => x.id)).toEqual(["3", "2", "1", "0"]); // 头部 '4' 被裁
  });

  it("向更新扩展超限 → 裁尾部（最早端），最早端 hasMoreOlder 应置 true", () => {
    const prev = [item("4"), item("3"), item("2"), item("1")];
    const ascBatch = [item("5")]; // 向更新一页（升序）
    const merged = mergeNewer(prev, ascBatch); // [5,4,3,2,1] 超限
    const { items, trimmed } = clampWindow(merged, MAX, "tail");
    expect(trimmed).toBe(true); // → handler 把 hasMoreOlder 置回 true
    expect(items.map((x) => x.id)).toEqual(["5", "4", "3", "2"]); // 尾部 '1' 被裁
  });
});

// === 闭区间边界去重：同一 publishedAt 多条，两页带回边界条 ===
describe("闭区间边界去重（同一 publishedAt 多条）", () => {
  it("两页在同一时刻边界条重叠 → 去重后无重复", () => {
    const t = "2026-06-03T10:00:00Z";
    // 第一页（最新）：同一时刻 a,b,c
    const page1 = [
      { id: "a", publishedAt: t },
      { id: "b", publishedAt: t },
      { id: "c", publishedAt: t },
    ];
    // 向更早 publishedTo=t（闭区间）→ 后端带回同时刻全部 b,c,d,e
    const page2 = [
      { id: "b", publishedAt: t },
      { id: "c", publishedAt: t },
      { id: "d", publishedAt: t },
      { id: "e", publishedAt: t },
    ];
    const out = mergeOlder(page1, page2);
    const ids = out.map((x) => x.id);
    expect(ids).toEqual(["a", "b", "c", "d", "e"]);
    expect(new Set(ids).size).toBe(ids.length); // 无重复
  });
});
