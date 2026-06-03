// 设置页本地类型 + wireFormat 文案映射。
//
// Spec: docs/design/frontend-design.md §设置页 + docs/design/agent-infra-module.md §2 ProviderChannel
//
// wireFormat 是渠道的核心抽象（按消息格式而非厂商）。这里集中维护 UI 文案，
// 列表 badge / 自定义表单 select 共用。

import type { WireFormat } from "../../bindings";

/** WireFormat → 人类可读消息格式名（自定义表单 select + 列表 badge 共用）。 */
export const WIRE_FORMAT_LABEL: Record<WireFormat, string> = {
  messages: "Messages",
  responses: "Responses",
  chat_completions: "Chat Completions",
};

/** 自定义表单 select 选项顺序（Messages / Chat Completions / Responses）。 */
export const WIRE_FORMAT_OPTIONS: WireFormat[] = [
  "messages",
  "chat_completions",
  "responses",
];

/** 从 baseUrl 取 host 用于列表展示；解析失败回退原串。 */
export function hostOf(baseUrl: string | null | undefined): string {
  if (!baseUrl) return "-";
  try {
    return new URL(baseUrl).host || baseUrl;
  } catch {
    return baseUrl;
  }
}

/** 渠道/预设头像首字母：取 provider 名首字符大写，给来源一个视觉锚点。 */
export function providerInitial(p: string): string {
  return (p.trim()[0] ?? "?").toUpperCase();
}

/** Result error → 展示文案（code: message）。 */
export function formatError(error: {
  code: string;
  message?: string | null;
}): string {
  return `${error.code}${error.message ? `: ${error.message}` : ""}`;
}
