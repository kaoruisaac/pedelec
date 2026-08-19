import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  applyEffortWizardSettings,
  getEffortWizardBootstrap,
  getEffortWizardState,
  listenToEffortWizardState,
  resetEffortWizard,
  startEffortWizard,
} from "./effortWizardApi";
import { createEffortWizardStore } from "./effortWizardStore";
import type { EffortWizardBootstrap, EffortWizardRunState } from "./types";

vi.mock("./effortWizardApi", () => ({
  applyEffortWizardSettings: vi.fn(),
  errorDetails: vi.fn(),
  formatApiError: (error: unknown) => error instanceof Error ? error.message : String(error),
  getEffortWizardBootstrap: vi.fn(),
  getEffortWizardState: vi.fn(),
  listenToEffortWizardState: vi.fn(async () => () => undefined),
  refreshProviders: vi.fn(),
  resetEffortWizard: vi.fn(async () => undefined),
  startEffortWizard: vi.fn(),
}));

const bootstrapMock = vi.mocked(getEffortWizardBootstrap);
const stateMock = vi.mocked(getEffortWizardState);
const listenMock = vi.mocked(listenToEffortWizardState);
const applyMock = vi.mocked(applyEffortWizardSettings);
const startMock = vi.mocked(startEffortWizard);
const resetMock = vi.mocked(resetEffortWizard);

const emptyEfforts = () => ({ low: [], default: [], high: [] });

function bootstrap(): EffortWizardBootstrap {
  return {
    providers: [
      { provider: "codex", available: true, version: null, currentPresetRevision: 2, appliedPresetRevision: 1, hasAnyEffortSetting: false, presetUpdateAvailable: true },
      { provider: "claude", available: true, version: null, currentPresetRevision: 2, appliedPresetRevision: 2, hasAnyEffortSetting: false, presetUpdateAvailable: false },
      { provider: "cursor", available: false, version: null, currentPresetRevision: 2, appliedPresetRevision: null, hasAnyEffortSetting: false, presetUpdateAvailable: false },
      { provider: "antigravity", available: false, version: null, currentPresetRevision: 2, appliedPresetRevision: null, hasAnyEffortSetting: false, presetUpdateAvailable: false },
    ],
    homeReminder: { type: "preset_update", providers: ["codex"] },
  };
}

function reviewRun(): EffortWizardRunState {
  const recommendation = {
    provider: "codex" as const,
    presetRevision: 2,
    confirmed: { low: ["--model", "low-model"], default: null, high: ["--model", "high-model"] },
    deterministicComplete: true,
  };
  return {
    runId: "review-run",
    status: "review_ready",
    selectedProviders: ["codex"],
    providers: [{ provider: "codex", selected: true, status: "review_ready", currentEfforts: emptyEfforts(), recommendation, probeResults: [], error: null }],
    review: {
      providers: [{ provider: "codex", status: "review_ready", currentEfforts: emptyEfforts(), recommendation, error: null }],
      reviewableProviderCount: 1,
    },
    error: null,
  };
}

describe("effort wizard store", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    bootstrapMock.mockResolvedValue(bootstrap());
    stateMock.mockResolvedValue(null);
    listenMock.mockResolvedValue(() => undefined);
    resetMock.mockResolvedValue(undefined);
  });

  it("initializes with the bootstrap, state, and event listener", async () => {
    const wizard = createEffortWizardStore();
    await wizard.initialize();
    expect(bootstrapMock).toHaveBeenCalledTimes(1);
    expect(stateMock).toHaveBeenCalledTimes(1);
    expect(listenMock).toHaveBeenCalledTimes(1);
    expect(wizard.store.initialized).toBe(true);
  });

  it("uses the right default selection for Home update and disables zero-selection start", async () => {
    const wizard = createEffortWizardStore();
    await wizard.openWizard("home-update");
    expect(wizard.store.selectedProviders).toEqual(["codex"]);
    wizard.toggleProvider("codex");
    expect(wizard.store.selectedProviders).toEqual([]);
    expect(await wizard.startSelectedProviders()).toBe(false);
    expect(startMock).not.toHaveBeenCalled();
  });

  it("lets Home update users expand the outdated default selection", async () => {
    const wizard = createEffortWizardStore();
    await wizard.openWizard("home-update");

    wizard.toggleProvider("claude");
    startMock.mockResolvedValue({
      ...reviewRun(),
      selectedProviders: ["codex", "claude"],
      status: "checking",
      review: null,
    });

    expect(wizard.store.selectedProviders).toEqual(["codex", "claude"]);
    expect(await wizard.startSelectedProviders()).toBe(true);
    expect(startMock).toHaveBeenCalledWith(["codex", "claude"]);
  });

  it("forwards exactly the selected available Wizard providers", async () => {
    bootstrapMock.mockResolvedValue({
      ...bootstrap(),
      providers: bootstrap().providers.map((provider) =>
        provider.provider === "cursor"
          ? { ...provider, available: true }
          : provider,
      ),
    });
    const wizard = createEffortWizardStore();
    await wizard.openWizard("settings");
    wizard.toggleProvider("claude");
    wizard.toggleProvider("antigravity");
    startMock.mockResolvedValue({ ...reviewRun(), selectedProviders: ["codex", "cursor"] });

    await wizard.startSelectedProviders();

    expect(startMock).toHaveBeenCalledWith(["codex", "cursor"]);
    expect(startMock).not.toHaveBeenCalledWith(expect.arrayContaining(["antigravity", "opencode", "ollama"]));
  });

  it("resumes an active run instead of starting a second one", async () => {
    stateMock.mockResolvedValue({ ...reviewRun(), status: "checking", review: null });
    const wizard = createEffortWizardStore();
    await wizard.openWizard("settings");
    expect(wizard.hasResumableRun()).toBe(true);
    expect(startMock).not.toHaveBeenCalled();
  });

  it("keeps tier drafts local and sends only decisions on Apply", async () => {
    const run = reviewRun();
    stateMock.mockResolvedValue(run);
    applyMock.mockResolvedValue({ state: { ...run, status: "completed", completion: { providers: [{ provider: "codex", status: "kept_current" }], confirmedProviderCount: 1 } }, bootstrap: bootstrap() });
    const wizard = createEffortWizardStore();
    await wizard.openWizard("settings");
    wizard.setTierDecision(run.runId, "codex", "low", false);
    await wizard.applyReview();
    expect(applyMock).toHaveBeenCalledWith({
      runId: "review-run",
      providers: [{ provider: "codex", tiers: { low: "keep_current", default: "keep_current", high: "update" } }],
    });
    expect(wizard.store.settingsRefreshRevision).toBe(1);
  });

  it("preserves a decision draft when re-entering the same run and resets it for a new run", async () => {
    const run = reviewRun();
    let stateListener: ((state: EffortWizardRunState | null) => void) | undefined;
    listenMock.mockImplementation(async (listener) => {
      stateListener = listener;
      return () => undefined;
    });
    stateMock.mockResolvedValue(run);
    const wizard = createEffortWizardStore();

    await wizard.openWizard("settings");
    wizard.setTierDecision(run.runId, "codex", "low", false);
    await wizard.openWizard("settings");
    expect(wizard.tierDecision(run.runId, "codex", "low")).toBe("keep_current");

    const newRun = { ...reviewRun(), runId: "new-run" };
    stateListener?.(newRun);
    expect(wizard.tierDecision(newRun.runId, "codex", "low")).toBe("update");
    expect(wizard.tierDecision(newRun.runId, "codex", "high")).toBe("update");
  });

  it("still sends Apply when every confirmed tier is Keep Current", async () => {
    const run = reviewRun();
    stateMock.mockResolvedValue(run);
    applyMock.mockResolvedValue({
      state: {
        ...run,
        status: "completed",
        completion: { providers: [{ provider: "codex", status: "kept_current" }], confirmedProviderCount: 1 },
      },
      bootstrap: bootstrap(),
    });
    const wizard = createEffortWizardStore();
    await wizard.openWizard("settings");
    wizard.setTierDecision(run.runId, "codex", "low", false);
    wizard.setTierDecision(run.runId, "codex", "high", false);

    expect(await wizard.applyReview()).toBe(true);
    expect(applyMock).toHaveBeenCalledWith({
      runId: run.runId,
      providers: [{ provider: "codex", tiers: { low: "keep_current", default: "keep_current", high: "keep_current" } }],
    });
  });

  it("keeps the review and decision drafts after a stale Apply error", async () => {
    const run = reviewRun();
    stateMock.mockResolvedValue(run);
    applyMock.mockRejectedValue(new Error("EFFORT_WIZARD_SETTINGS_CHANGED: settings changed"));
    const wizard = createEffortWizardStore();
    await wizard.openWizard("settings");
    wizard.setTierDecision(run.runId, "codex", "low", false);

    expect(await wizard.applyReview()).toBe(false);
    expect(wizard.store.runState?.status).toBe("review_ready");
    expect(wizard.store.settingsRefreshRevision).toBe(0);
    expect(wizard.tierDecision(run.runId, "codex", "low")).toBe("keep_current");
    expect(wizard.store.error).toContain("EFFORT_WIZARD_SETTINGS_CHANGED");
  });
});
