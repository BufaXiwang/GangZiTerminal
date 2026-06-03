// @vitest-environment jsdom
//
// scrollAnchor DOM 测试 — 元素锚定补偿。
// Spec: frontend-design.md §4「窗口有界」/ news-module.md §4「裁剪/prepend 做滚动锚定」。
//
// jsdom 不做真实布局：getBoundingClientRect() 默认全 0。这里给容器/行打桩 rect，
// 驱动「选视口顶部第一条可见行」+「按 delta 补偿 scrollTop」+「锚行被裁则跳过」三条逻辑。

import { describe, it, expect, beforeEach } from "vitest";
import { captureTopAnchor, restoreTopAnchor } from "./scrollAnchor";

const SELECTOR = "[data-news-id]";

/** 给元素打桩一个固定的 getBoundingClientRect（只用到 top/bottom）。 */
function stubRect(el: HTMLElement, top: number, bottom: number): void {
  el.getBoundingClientRect = () =>
    ({ top, bottom, left: 0, right: 0, width: 0, height: bottom - top, x: 0, y: 0, toJSON() {} }) as DOMRect;
}

function makeRow(id: string): HTMLElement {
  const el = document.createElement("div");
  el.dataset.newsId = id;
  return el;
}

let container: HTMLElement;

beforeEach(() => {
  document.body.innerHTML = "";
  container = document.createElement("div");
  document.body.appendChild(container);
});

describe("captureTopAnchor", () => {
  it("选视口顶部第一条可见行（bottom 越过容器顶的那条）", () => {
    stubRect(container, 100, 600); // 容器顶 = 100
    const r1 = makeRow("a"); // 已滚出上方：bottom 90 <= 100，不可见
    const r2 = makeRow("b"); // 顶部第一条可见：bottom 150 > 100
    const r3 = makeRow("c");
    stubRect(r1, 40, 90);
    stubRect(r2, 90, 150);
    stubRect(r3, 150, 220);
    container.append(r1, r2, r3);

    const anchor = captureTopAnchor(container, SELECTOR);
    expect(anchor).toEqual({ id: "b", top: 90 });
  });

  it("容器为 null → 返回 null", () => {
    expect(captureTopAnchor(null, SELECTOR)).toBeNull();
  });

  it("无可见行（全部已滚出上方）→ 返回 null", () => {
    stubRect(container, 100, 600);
    const r1 = makeRow("a");
    stubRect(r1, 10, 80); // bottom 80 <= 100
    container.append(r1);
    expect(captureTopAnchor(container, SELECTOR)).toBeNull();
  });

  it("支持自定义 getId", () => {
    stubRect(container, 0, 600);
    const r1 = document.createElement("div");
    r1.setAttribute("data-news-id", "x");
    r1.id = "custom-1";
    stubRect(r1, 10, 50);
    container.append(r1);
    const anchor = captureTopAnchor(container, SELECTOR, (el) => el.id);
    expect(anchor?.id).toBe("custom-1");
  });
});

describe("restoreTopAnchor", () => {
  it("按 delta（新 top − 旧 top）补偿 scrollTop", () => {
    container.scrollTop = 500;
    const r = makeRow("b");
    stubRect(r, 90, 150); // mutation 后该行 top = 90
    container.append(r);
    // mutation 前锚记录 top = 60 → delta = 90 − 60 = 30 → scrollTop += 30
    restoreTopAnchor(container, { id: "b", top: 60 }, SELECTOR);
    expect(container.scrollTop).toBe(530);
  });

  it("delta 为负（内容向上）也正确补偿", () => {
    container.scrollTop = 500;
    const r = makeRow("b");
    stubRect(r, 40, 100);
    container.append(r);
    // 旧 top=90 → delta = 40 − 90 = −50
    restoreTopAnchor(container, { id: "b", top: 90 }, SELECTOR);
    expect(container.scrollTop).toBe(450);
  });

  it("delta=0 不动 scrollTop", () => {
    container.scrollTop = 500;
    const r = makeRow("b");
    stubRect(r, 90, 150);
    container.append(r);
    restoreTopAnchor(container, { id: "b", top: 90 }, SELECTOR);
    expect(container.scrollTop).toBe(500);
  });

  it("锚点行被裁（DOM 里找不到该 id）→ 静默跳过，不补偿", () => {
    container.scrollTop = 500;
    const r = makeRow("other");
    stubRect(r, 90, 150);
    container.append(r);
    restoreTopAnchor(container, { id: "trimmed-away", top: 60 }, SELECTOR);
    expect(container.scrollTop).toBe(500);
  });

  it("anchor 为 null → 不动", () => {
    container.scrollTop = 500;
    restoreTopAnchor(container, null, SELECTOR);
    expect(container.scrollTop).toBe(500);
  });

  it("容器为 null → 不抛错", () => {
    expect(() => restoreTopAnchor(null, { id: "b", top: 0 }, SELECTOR)).not.toThrow();
  });
});
