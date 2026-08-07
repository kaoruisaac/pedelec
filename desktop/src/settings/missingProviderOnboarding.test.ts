import { describe, expect, it, vi } from "vitest";
import {
  createMissingProviderOnboardingController,
  phaseAfterInstallerLaunch,
  RECOMMENDED_PROVIDERS,
} from "./missingProviderOnboarding";
import { isOnboardingInstallerSupported, type OnboardingInstallerCode } from "./providerInstaller";

describe("missing provider onboarding", () => {
  it.each(["codex", "antigravity", "cursor"] as const)(
    "%s is available for one-click installation",
    (provider) => {
      expect(isOnboardingInstallerSupported(provider)).toBe(true);
    },
  );

  it("keeps Claude Code in the list without enabling one-click installation", () => {
    expect(RECOMMENDED_PROVIDERS.map((provider) => provider.code)).toEqual([
      "codex",
      "claude",
      "antigravity",
      "cursor",
    ]);
    expect(isOnboardingInstallerSupported("claude")).toBe(false);
  });

  it("does not include OpenCode in the onboarding recommendations", () => {
    expect(RECOMMENDED_PROVIDERS.some((provider) => (provider.code as string) === "opencode")).toBe(false);
  });

  it("moves to installation progress only after installer launch succeeds", () => {
    expect(phaseAfterInstallerLaunch(true)).toBe("installation-progress");
    expect(phaseAfterInstallerLaunch(false)).toBe("selection");
  });

  it("recovers from an installer failure and allows a retry", async () => {
    const openProviderInstaller = vi.fn(async (_provider: OnboardingInstallerCode) => undefined);
    openProviderInstaller.mockRejectedValueOnce(new Error("Terminal could not be opened"));
    const restartPedelec = vi.fn(async () => undefined);
    const controller = createMissingProviderOnboardingController({
      openProviderInstaller,
      restartPedelec,
    });

    await controller.installProvider("codex");

    expect(controller.getState()).toMatchObject({
      phase: "selection",
      launchingProvider: null,
      error: "Terminal could not be opened",
    });

    await controller.installProvider("cursor");

    expect(openProviderInstaller).toHaveBeenNthCalledWith(2, "cursor");
    expect(controller.getState()).toMatchObject({
      phase: "installation-progress",
      launchingProvider: null,
      error: "",
    });
  });

  it("clears installer launching state after a successful launch", async () => {
    const openProviderInstaller = vi.fn(async (_provider: OnboardingInstallerCode) => undefined);
    const controller = createMissingProviderOnboardingController({
      openProviderInstaller,
      restartPedelec: vi.fn(async () => undefined),
    });

    await controller.installProvider("antigravity");

    expect(controller.getState()).toEqual({
      phase: "installation-progress",
      launchingProvider: null,
      restarting: false,
      error: "",
    });
  });

  it("recovers from a restart failure and allows another restart", async () => {
    const restartPedelec = vi.fn(async () => undefined);
    restartPedelec.mockRejectedValueOnce(new Error("Restart was blocked"));
    const controller = createMissingProviderOnboardingController({
      openProviderInstaller: vi.fn(async (_provider: OnboardingInstallerCode) => undefined),
      restartPedelec,
    });

    await controller.installProvider("codex");
    await controller.restart();

    expect(controller.getState()).toMatchObject({
      phase: "installation-progress",
      restarting: false,
      error: "Restart was blocked",
    });

    await controller.restart();

    expect(restartPedelec).toHaveBeenCalledTimes(2);
    expect(controller.getState()).toMatchObject({
      phase: "installation-progress",
      restarting: true,
      error: "",
    });
  });
});
