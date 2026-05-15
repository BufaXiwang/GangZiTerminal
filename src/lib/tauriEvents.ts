export type UnlistenFn = () => void;

export function safeUnlisten(unlisten: UnlistenFn | null | undefined) {
  if (!unlisten) return;
  try {
    unlisten();
  } catch (err) {
    console.warn("Tauri event unlisten failed:", err);
  }
}
