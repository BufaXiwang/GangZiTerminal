import React from "react";
import ReactDOM from "react-dom/client";
import "./styles.css";

function showBootError(error: unknown) {
  const message = error instanceof Error ? error.stack || error.message : String(error);
  const existing = document.getElementById("boot-error");
  const target = existing ?? document.createElement("div");
  target.id = "boot-error";
  target.innerHTML = `
    <main style="padding:24px;max-width:960px;font-family:-apple-system,BlinkMacSystemFont,'PingFang SC',sans-serif;color:#2b221c;">
      <h1 style="font-size:20px;margin:0 0 12px;">前端启动失败</h1>
      <p style="margin:0 0 16px;color:#5c5042;">WebView 捕获到启动错误：</p>
      <pre style="white-space:pre-wrap;background:#fff;border:1px solid #dcd4c4;border-radius:8px;padding:14px;line-height:1.5;overflow:auto;">${escapeHtml(message)}</pre>
    </main>
  `;
  document.getElementById("boot-status")?.remove();
  if (!existing) document.body.prepend(target);
}

function getErrorText(error: unknown) {
  return error instanceof Error ? error.stack || error.message : String(error);
}

function isBenignTauriUnlistenError(error: unknown) {
  const message = getErrorText(error);
  return (
    message.includes("unregisterListener") ||
    message.includes("_unlisten") ||
    message.includes("safeUnlisten")
  );
}

function BootErrorView({ error }: { error: unknown }) {
  const message = error instanceof Error ? error.stack || error.message : String(error);
  return (
    <main style={{ padding: 24, maxWidth: 960, color: "#2b221c" }}>
      <h1 style={{ fontSize: 20, margin: "0 0 12px" }}>前端启动失败</h1>
      <p style={{ margin: "0 0 16px", color: "#5c5042" }}>
        React 渲染捕获到错误：
      </p>
      <pre
        style={{
          whiteSpace: "pre-wrap",
          background: "#fff",
          border: "1px solid #dcd4c4",
          borderRadius: 8,
          padding: 14,
          lineHeight: 1.5,
          overflow: "auto",
        }}
      >
        {message}
      </pre>
    </main>
  );
}

function escapeHtml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#039;");
}

// App 是否已经成功挂载——决定后续 window error 是当"启动失败"满屏处理，还是
// 当"运行时错误"只打日志。之前不区分，导致点击全屏按钮等任意运行时 throw
// 都被当成启动失败，覆盖整个 UI。
let appMounted = false;

window.addEventListener("error", (event) => {
  if (isBenignTauriUnlistenError(event.error ?? event.message)) {
    event.preventDefault();
    return;
  }
  if (appMounted) {
    // App 已挂载——交给 BootErrorBoundary 或具体组件处理，全局只 log
    console.error("[runtime error]", event.error ?? event.message);
    return;
  }
  showBootError(event.error ?? event.message);
});
window.addEventListener("unhandledrejection", (event) => {
  if (isBenignTauriUnlistenError(event.reason)) {
    event.preventDefault();
    return;
  }
  if (appMounted) {
    console.warn("[unhandled rejection]", event.reason);
    event.preventDefault();
    return;
  }
  showBootError(event.reason);
});

class BootErrorBoundary extends React.Component<
  { children: React.ReactNode },
  { error: unknown }
> {
  state = { error: null };

  static getDerivedStateFromError(error: unknown) {
    return { error };
  }

  componentDidCatch(error: unknown) {
    console.error(error);
  }

  render() {
    if (this.state.error) {
      return <BootErrorView error={this.state.error} />;
    }
    return this.props.children;
  }
}

const rootEl = document.getElementById("root");
if (!rootEl) {
  showBootError(new Error("找不到 #root 节点"));
} else {
  const root = ReactDOM.createRoot(rootEl);
  import("./App")
    .then(({ default: App }) => {
      document.getElementById("boot-status")?.remove();
      document.getElementById("boot-error")?.remove();
      root.render(
        <React.StrictMode>
          <BootErrorBoundary>
            <App />
          </BootErrorBoundary>
        </React.StrictMode>,
      );
      appMounted = true;
    })
    .catch(showBootError);
}
