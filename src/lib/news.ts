import type { NewsItem, StockQuote } from "../types";

export function uniqueById(item: NewsItem, index: number, list: NewsItem[]) {
  return list.findIndex((entry) => entry.id === item.id) === index;
}

export function inferTags(item: NewsItem) {
  const text = `${item.title} ${item.summary ?? ""}`;
  const tags = ["政策", "港股", "AI", "新能源", "半导体", "消费", "地产", "券商", "业绩"].filter((tag) => text.includes(tag));
  return tags.length ? tags.slice(0, 3) : ["待研判"];
}

export function newsMatchesFilter(item: NewsItem, filter: string) {
  if (filter === "全部") return true;
  const text = `${item.title} ${item.summary ?? ""} ${item.source}`;
  if (filter === "快讯") return item.source.includes("快讯") || item.source.includes("电报") || item.source.includes("金十");
  if (filter === "政策") return /政策|监管|会议|部委|央行|证监|财政|关税|国务院/.test(text);
  if (filter === "公司") return /公司|股份|集团|证券|净利|营收|同比|季度|公告|回购|并购/.test(text);
  if (filter === "市场") return /指数|涨|跌|成交|板块|行业|资金|港股|美股|期货|汇率|油价/.test(text);
  return true;
}

export function newsMentionsQuote(item: NewsItem, quote: StockQuote) {
  const text = `${item.title} ${item.summary ?? ""}`;
  return text.includes(quote.code) || (!!quote.name && text.includes(quote.name));
}
