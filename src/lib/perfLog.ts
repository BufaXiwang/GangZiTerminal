// perfLog — 性能日志双写：console + 后端 tracing。
//
// 调 perf("xxx") 既在 webview devtools 打印，也通过 IPC 转给 Rust tracing → tauri dev stdout。
// 这样开发者无需打开 webview devtools 就能从 tauri dev 输出文件直接看 log。

import { commands } from "../bindings";

const ENABLED = true;

export function perf(msg: string): void {
  if (!ENABLED) return;
  const line = `[perf] ${msg}`;
  // eslint-disable-next-line no-console
  console.log(line);
  // IPC forward — fire-and-forget，不 await，避免影响性能测量本身
  void commands.forwardLog("info", line);
}
