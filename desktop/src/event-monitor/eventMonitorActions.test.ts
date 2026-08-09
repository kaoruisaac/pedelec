import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { openThreadSandbox } from "./eventMonitorActions";

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
});
