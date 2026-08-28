import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { monitorEndThread, openThreadSandbox, sendThreadText } from "./eventMonitorActions";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

describe("event monitor actions", () => {
  beforeEach(() => {
    vi.mocked(invoke).mockReset();
    vi.mocked(invoke).mockResolvedValue(undefined);
  });

  it("opens the selected thread sandbox with the expected command payload", async () => {
    await openThreadSandbox("t000123");

    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenCalledWith("open_thread_sandbox", {
      threadId: "t000123",
    });
  });

  it("propagates Tauri invoke failures", async () => {
    const error = new Error("sandbox opener failed");
    vi.mocked(invoke).mockRejectedValueOnce(error);

    await expect(openThreadSandbox("t000123")).rejects.toBe(error);
  });

  it("sends debug thread text with the existing payload shape", async () => {
    vi.mocked(invoke).mockResolvedValueOnce({ threadId: "t000123" });

    await sendThreadText("t000123", "What did you just change?");

    expect(invoke).toHaveBeenCalledWith("debug_send_text", {
      input: {
        threadId: "t000123",
        message: "What did you just change?",
      },
    });
  });

  it("propagates debug_send_text invoke failures", async () => {
    const error = new Error("debug send failed");
    vi.mocked(invoke).mockRejectedValueOnce(error);

    await expect(sendThreadText("t000123", "Please explain the last change.")).rejects.toBe(error);
  });

  it("stops a thread through the Monitor-only command payload", async () => {
    await monitorEndThread("t000123");

    expect(invoke).toHaveBeenCalledWith("monitor_end_thread", {
      input: {
        threadId: "t000123",
      },
    });
  });
});
