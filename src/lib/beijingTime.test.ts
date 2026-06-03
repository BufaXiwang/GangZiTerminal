// beijingTime 纯函数测试 — 北京时区(UTC+8)日历运算。
// Spec: news-module.md §4（dateCounts 按 Asia/Shanghai 聚合 / 锚定到某天）。

import { describe, it, expect } from "vitest";
import { beijingDayEndIso, shiftDateKey, beijingDateKey } from "./beijingTime";

describe("beijingDayEndIso（某北京日 → 当日 23:59:59.999+08:00 的 UTC ISO）", () => {
  it("2026-06-03 北京日末 = 2026-06-03T15:59:59.999Z（UTC）", () => {
    // 23:59:59.999+08:00 → UTC -8h → 15:59:59.999Z
    expect(beijingDayEndIso("2026-06-03")).toBe("2026-06-03T15:59:59.999Z");
  });

  it("跨年最后一天 2025-12-31 北京日末 → UTC 仍在 12-31", () => {
    expect(beijingDayEndIso("2025-12-31")).toBe("2025-12-31T15:59:59.999Z");
  });
});

describe("shiftDateKey（日历日加减，UTC 锚定）", () => {
  it("普通 +1 天", () => {
    expect(shiftDateKey("2026-06-03", 1)).toBe("2026-06-04");
  });
  it("普通 -1 天", () => {
    expect(shiftDateKey("2026-06-03", -1)).toBe("2026-06-02");
  });
  it("跨月（6-30 +1 → 7-01）", () => {
    expect(shiftDateKey("2026-06-30", 1)).toBe("2026-07-01");
  });
  it("跨月回退（3-01 -1 → 2-28，2026 非闰年）", () => {
    expect(shiftDateKey("2026-03-01", -1)).toBe("2026-02-28");
  });
  it("闰年 2-29（2024-02-29 +1 → 03-01）", () => {
    expect(shiftDateKey("2024-02-29", 1)).toBe("2024-03-01");
  });
  it("跨年（12-31 +1 → 次年 01-01）", () => {
    expect(shiftDateKey("2025-12-31", 1)).toBe("2026-01-01");
  });
  it("跨年回退（01-01 -1 → 前年 12-31）", () => {
    expect(shiftDateKey("2026-01-01", -1)).toBe("2025-12-31");
  });
  it("delta=0 原样返回（补零规范化）", () => {
    expect(shiftDateKey("2026-01-05", 0)).toBe("2026-01-05");
  });
});

describe("beijingDateKey（UTC Date → 北京日历日，跨午夜）", () => {
  it("UTC 02:00 = 北京 10:00 同日", () => {
    expect(beijingDateKey(new Date("2026-06-03T02:00:00Z"))).toBe("2026-06-03");
  });
  it("UTC 23:00 = 北京次日 07:00 → 跨到次日", () => {
    expect(beijingDateKey(new Date("2026-06-03T23:00:00Z"))).toBe("2026-06-04");
  });
  it("UTC 16:00 = 北京次日 00:00 整 → 已是次日", () => {
    expect(beijingDateKey(new Date("2026-06-03T16:00:00Z"))).toBe("2026-06-04");
  });
  it("UTC 15:59 = 北京 23:59 → 仍当日", () => {
    expect(beijingDateKey(new Date("2026-06-03T15:59:00Z"))).toBe("2026-06-03");
  });
  it("beijingDayEndIso 与 beijingDateKey 互逆：日末 ISO 解析回同一北京日", () => {
    expect(beijingDateKey(new Date(beijingDayEndIso("2026-06-03")))).toBe("2026-06-03");
  });
});
