import { invoke } from "@tauri-apps/api/core";

export interface SendThreadTextOutput {
  threadId: string;
}

export function sendThreadText(
  threadId: string,
  message: string,
): Promise<SendThreadTextOutput> {
  return invoke<SendThreadTextOutput>("debug_send_text", {
    input: {
      threadId,
      message,
    },
  });
}

export function openThreadWorkspace(threadId: string): Promise<void> {
  return invoke<void>("open_thread_workspace", { threadId });
}

export function monitorEndThread(threadId: string): Promise<void> {
  return invoke<void>("monitor_end_thread", {
    input: {
      threadId,
    },
  });
}
