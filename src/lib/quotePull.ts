// quotePull — 前端聚焦 pull 协调器（唯一报价 pull 推送方）。
//
// Spec: docs/design/quotes-module.md §5 报价刷新（聚焦 pull 取代热点集）
//
// 设计要点（重构 Step B）：
// - 各视图通过 setSource(key, codes) 声明"自己关心的那撮标的"：
//     account(自选+持仓) / selected(选中) / indices(核心指数) / market(可见列表)
// - 协调器算并集去重，cap 120；超出时按优先级截断：
//     account > selected > indices > market —— 保证最关心的不被列表头挤掉。
// - 全局 3s interval：每 tick 若并集非空 **且市场活跃**（isContinuousAuction，
//   午休/盘后/休市不轮询，避免空刷冻结价）→ commands.refreshQuotes(union)。
// - 并集变化时立即 pull 一次（debounce 300ms 合并抖动），**不 gate 交易时段**：
//   用户打开/切换就该拿到最新可得快照，哪怕盘后也刷一次给最新收盘。
// - in-flight 保护：上一次 refreshQuotes 未回来不叠发。
// - 失败静默，下个 tick 再试。
//
// 后端 refreshQuotes 拉这些标的最新报价写 cache + emit
// market-quotes-refresh-progress，各视图已 listen 做增量更新（此协调器不碰监听）。

import { commands } from "../bindings";
import { isContinuousAuction } from "./tradingSession";

// 优先级从高到低：截断时保留靠前的来源。
const PRIORITY: readonly string[] = ["account", "selected", "indices", "market"];
const CAP = 120;
const TICK_MS = 3000;
const CHANGE_DEBOUNCE_MS = 300;

const sources: Record<string, string[]> = {};
let lastUnionKey = "";
let inFlight = false;
let tickTimer: number | null = null;
let changeTimer: number | null = null;
let refCount = 0;

/** 按来源优先级合并去重，cap 120。靠前来源优先占额。 */
function computeUnion(): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  const keys = [
    ...PRIORITY.filter((k) => k in sources),
    ...Object.keys(sources).filter((k) => !PRIORITY.includes(k)),
  ];
  for (const k of keys) {
    for (const code of sources[k] ?? []) {
      if (!code || seen.has(code)) continue;
      seen.add(code);
      out.push(code);
      if (out.length >= CAP) return out;
    }
  }
  return out;
}

async function pull(union: string[]): Promise<void> {
  if (inFlight || union.length === 0) return;
  inFlight = true;
  try {
    await commands.refreshQuotes(union);
  } catch {
    // 失败静默，下个 tick 再试。
  } finally {
    inFlight = false;
  }
}

/** 全局 3s tick：市场活跃才轮询；非活跃时段不空刷。 */
function onTick(): void {
  if (!isContinuousAuction()) return;
  const union = computeUnion();
  if (union.length === 0) return;
  void pull(union);
}

function ensureTimer(): void {
  if (tickTimer == null) {
    tickTimer = window.setInterval(onTick, TICK_MS);
  }
}

/** 并集变化时立即刷一次（debounce），不 gate 交易时段。 */
function scheduleChangePull(): void {
  if (changeTimer != null) window.clearTimeout(changeTimer);
  changeTimer = window.setTimeout(() => {
    changeTimer = null;
    const union = computeUnion();
    if (union.length > 0) void pull(union);
  }, CHANGE_DEBOUNCE_MS);
}

/**
 * 声明某来源关心的标的集合。变化时（去重后并集发生改变）立即触发一次 pull。
 * 传空数组 = 清空该来源贡献（组件卸载时调用）。
 */
export function setSource(key: string, codes: string[]): void {
  sources[key] = codes;
  ensureTimer();
  const union = computeUnion();
  const unionKey = union.join(",");
  if (unionKey !== lastUnionKey) {
    lastUnionKey = unionKey;
    scheduleChangePull();
  }
}

/** 当前并集（去重 + cap 后），调试 / 测试用。 */
export function currentUnion(): string[] {
  return computeUnion();
}

/**
 * 注册一个使用方，返回注销函数。可选用于显式管理全局 timer 生命周期；
 * 当前 setSource 已自启 timer，调用方一般直接用 setSource/清空即可。
 */
export function acquire(): () => void {
  refCount += 1;
  ensureTimer();
  return () => {
    refCount -= 1;
    if (refCount <= 0 && tickTimer != null) {
      window.clearInterval(tickTimer);
      tickTimer = null;
      refCount = 0;
    }
  };
}
