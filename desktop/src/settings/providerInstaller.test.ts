import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import {
  openProviderInstaller,
  restartPedelec,
} from "./providerInstaller";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

describe("provider installer commands", () => {
  beforeEach(() => {
    vi.mocked(invoke).mockReset();
    vi.mocked(invoke).mockResolvedValue(undefined);
  });

  it("opens the existing installer command with the selected provider", async () => {
    await openProviderInstaller("codex");

    expect(invoke).toHaveBeenCalledWith("open_provider_installer", {
      input: { provider: "codex" },
    });
  });

  it("restarts directly through restart_app without a provider refresh", async () => {
    await restartPedelec();

    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenCalledWith("restart_app");
  });
});
