import { invoke } from "@tauri-apps/api/core";

export function openThreadSandbox(threadId: string): Promise<void> {
  return invoke<void>("open_thread_sandbox", { threadId });
}
