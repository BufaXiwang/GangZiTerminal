import type { MarketIndex, MarketOverview } from "../types";

export const sectorRows = [
  ["稀土", "+3.21%"],
  ["能源金属", "+2.05%"],
  ["半导体", "+1.48%"],
  ["房地产", "-1.86%"],
  ["教育", "-1.32%"],
] as const;

export const fallbackMarketOverview: MarketOverview = {
  indices: [
    emptyIndex("000001", "上证指数"),
    emptyIndex("399001", "深证成指"),
    emptyIndex("399006", "创业板指"),
    emptyIndex("000688", "科创50"),
  ],
  breadth: { rise: 0, fall: 0, flat: 0 },
  sectors: [],
  capturedAt: 0,
};

function emptyIndex(code: string, name: string): MarketIndex {
  return {
    code,
    name,
    price: null,
    change: null,
    changePercent: null,
    capturedAt: 0,
  };
}

// ====== 市场会话状态 ======
//
// A 股交易时段（北京时间，UTC+8）：
//   集合竞价 09:15-09:30 / 上午盘 09:30-11:30 / 午休 11:30-13:00 / 下午盘 13:00-15:00
// 周末和节假日不交易（节假日识别交给后端 / 用户自行判断；这里仅按周一到周五）。
//
// 用途：MarketOverview 顶上显示当前会话状态——盘外/休市时数据是上一交易日的，
// 必须明确告诉用户避免误读"还没开盘怎么有今天数据"。

export type MarketSessionStatus =
  | "pre_open"      // 09:00-09:15（盘前安静期）
  | "auction"       // 09:15-09:30（集合竞价）
  | "morning"       // 09:30-11:30（上午连续竞价）
  | "lunch"         // 11:30-13:00（午间休市）
  | "afternoon"     // 13:00-15:00（下午连续竞价）
  | "after_hours"   // 15:00 后到次日 09:00（盘后）
  | "weekend";      // 周六周日

export type MarketSession = {
  status: MarketSessionStatus;
  /** 用户友好的中文标签，比如"盘中" / "今日已收盘" / "周六休市" */
  label: string;
  /** 一段补充说明，告诉用户当前显示的数据来自哪天 */
  note: string;
  /** 当前是否处于"会有新成交在跑"的连续竞价时段——用来决定是否标"实时数据" */
  isLive: boolean;
};

/**
 * 根据传入时间（默认 now）算出 A 股市场会话状态。
 * 时间转换走 +8 偏移而不是 toLocaleString—— Date 对象的小时方法依赖系统时区，
 * 在非中国时区的设备上会算错。
 */
export function getMarketSession(now: Date = new Date()): MarketSession {
  // 把 UTC 时间手动 +8 小时拿到北京时间（不依赖系统时区，避免时区设错算崩）
  const beijing = new Date(now.getTime() + 8 * 60 * 60 * 1000);
  const day = beijing.getUTCDay(); // 0=Sunday, 6=Saturday
  const hour = beijing.getUTCHours();
  const minute = beijing.getUTCMinutes();
  const minuteOfDay = hour * 60 + minute;

  if (day === 0 || day === 6) {
    return {
      status: "weekend",
      label: day === 6 ? "周六休市" : "周日休市",
      note: "下次开盘 周一 09:30。当前显示上一交易日数据。",
      isLive: false,
    };
  }

  // 工作日按时间段判定
  if (minuteOfDay < 9 * 60) {
    return {
      status: "pre_open",
      label: "盘前",
      note: "09:15 开始集合竞价。当前显示上一交易日收盘数据。",
      isLive: false,
    };
  }
  if (minuteOfDay < 9 * 60 + 15) {
    return {
      status: "pre_open",
      label: "盘前",
      note: "09:15 开始集合竞价。",
      isLive: false,
    };
  }
  if (minuteOfDay < 9 * 60 + 30) {
    return {
      status: "auction",
      label: "集合竞价",
      note: "09:30 开盘。竞价区间允许撤单到 09:20。",
      isLive: false,
    };
  }
  if (minuteOfDay < 11 * 60 + 30) {
    return {
      status: "morning",
      label: "盘中（上午）",
      note: "11:30 午休。",
      isLive: true,
    };
  }
  if (minuteOfDay < 13 * 60) {
    return {
      status: "lunch",
      label: "午间休市",
      note: "13:00 恢复连续竞价。当前显示午前收盘数据。",
      isLive: false,
    };
  }
  if (minuteOfDay < 15 * 60) {
    return {
      status: "afternoon",
      label: "盘中（下午）",
      note: "15:00 收盘。",
      isLive: true,
    };
  }
  return {
    status: "after_hours",
    label: "今日已收盘",
    note: "下次开盘 次日 09:30（节假日除外）。当前显示今日收盘数据。",
    isLive: false,
  };
}
