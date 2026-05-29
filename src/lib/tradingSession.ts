// 交易时段判断（前端展示用，按北京时间 UTC+8）。
//
// 注意：忽略法定节假日（前端无交易日历），周末按休市；最坏情况节假日显示"已收盘/盘前"
// 而非"休市"，不影响功能。权威交易日历在 Rust（resolve_market_time）。
//
// A 股时段：
//   集合竞价 09:15–09:25 / 14:57–15:00
//   连续竞价 09:30–11:30 / 13:00–15:00
//   午休     11:30–13:00

import { useEffect, useState } from "react";

function beijing(now: Date): { min: number; weekend: boolean } {
  const utcMin = now.getUTCHours() * 60 + now.getUTCMinutes();
  const total = utcMin + 8 * 60;
  const min = total % (24 * 60);
  const dayShift = total >= 24 * 60 ? 1 : 0;
  const bjDay = (now.getUTCDay() + dayShift) % 7;
  return { min, weekend: bjDay === 0 || bjDay === 6 };
}

/** 是否处于连续竞价（用于 K 线盘中轮询门控）。 */
export function isContinuousAuction(now: Date = new Date()): boolean {
  const { min, weekend } = beijing(now);
  if (weekend) return false;
  const am = min >= 9 * 60 + 30 && min <= 11 * 60 + 30;
  const pm = min >= 13 * 60 && min <= 15 * 60;
  return am || pm;
}

export interface MarketSession {
  label: string;
  /** true = 行情活跃（连续竞价 / 集合竞价），数据在跳。 */
  active: boolean;
}

/** 当前 A 股时段标签。 */
export function marketSession(now: Date = new Date()): MarketSession {
  const { min, weekend } = beijing(now);
  if (weekend) return { label: "休市", active: false };
  if (min < 9 * 60 + 15) return { label: "盘前", active: false };
  if (min < 9 * 60 + 25) return { label: "集合竞价", active: true };
  if (min < 9 * 60 + 30) return { label: "盘前", active: false };
  if (min <= 11 * 60 + 30) return { label: "交易中", active: true };
  if (min < 13 * 60) return { label: "午休", active: false };
  if (min < 15 * 60) return { label: "交易中", active: true };
  return { label: "已收盘", active: false };
}

/** 每 20s 重新评估当前时段，让标签自动过渡。 */
export function useMarketSession(): MarketSession {
  const [s, setS] = useState<MarketSession>(() => marketSession());
  useEffect(() => {
    const t = window.setInterval(() => setS(marketSession()), 20_000);
    return () => window.clearInterval(t);
  }, []);
  return s;
}
