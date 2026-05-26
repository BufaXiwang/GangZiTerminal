import { useEffect, useState } from "react";
import { commands } from "./bindings";

export default function App() {
  const [status, setStatus] = useState<string>("calling ping...");

  useEffect(() => {
    void commands.ping().then((res) => {
      if (res.status === "ok") {
        setStatus(`✅ ${res.data.message}`);
      } else {
        setStatus(`❌ ${res.error.code}${res.error.message ? ": " + res.error.message : ""}`);
      }
    });
  }, []);

  return (
    <main
      style={{
        padding: "32px",
        fontFamily: "'Source Serif 4', serif",
        color: "#2f2618",
        background: "#f7f1e3",
        minHeight: "100vh",
      }}
    >
      <h1 style={{ fontWeight: 600, marginBottom: 12 }}>GangZi Terminal</h1>
      <p style={{ fontFamily: "'IBM Plex Mono', monospace", fontSize: 14 }}>
        Phase 0 walking skeleton
      </p>
      <p style={{ marginTop: 16 }}>{status}</p>
    </main>
  );
}
