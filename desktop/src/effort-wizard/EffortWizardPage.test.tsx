/** @vitest-environment jsdom */

import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import EffortWizardPage from "./EffortWizardPage";
import type { EffortWizardStore } from "./effortWizardStore";
import type { EffortWizardRunState, EffortWizardStoreState } from "./types";

const emptyEfforts = () => ({ low: [], default: [], high: [] });

function bootstrap() {
  return {
    providers: [
      { provider: "codex" as const, available: true, version: null, currentPresetRevision: 2, appliedPresetRevision: 2, hasAnyEffortSetting: false, presetUpdateAvailable: false },
      { provider: "claude" as const, available: true, version: null, currentPresetRevision: 2, appliedPresetRevision: 2, hasAnyEffortSetting: false, presetUpdateAvailable: false },
      { provider: "cursor" as const, available: false, version: null, currentPresetRevision: 2, appliedPresetRevision: null, hasAnyEffortSetting: false, presetUpdateAvailable: false },
      { provider: "antigravity" as const, available: true, version: null, currentPresetRevision: 2, appliedPresetRevision: 2, hasAnyEffortSetting: false, presetUpdateAvailable: false },
    ],
    homeReminder: null,
  };
}

function makeWizard(overrides: Partial<EffortWizardStoreState> = {}): EffortWizardStore {
  const store: EffortWizardStoreState = {
    bootstrap: bootstrap(),
    runState: null,
    loading: false,
    actionLoading: false,
    error: null,
    initialized: true,
    entryOrigin: "settings",
    selectedProviders: ["codex"],
    pageStage: "selection",
    settingsRefreshRevision: 0,
    decisionDrafts: {},
    ...overrides,
  };
  return {
    store,
    initialize: vi.fn(async () => undefined),
    dispose: vi.fn(),
    refreshBootstrap: vi.fn(async () => true),
    refreshProviderStatus: vi.fn(async () => true),
    openWizard: vi.fn(async () => undefined),
    toggleProvider: vi.fn(),
    startSelectedProviders: vi.fn(async () => true),
    setTierDecision: vi.fn(),
    tierDecision: vi.fn(() => "update"),
    applyReview: vi.fn(async () => true),
    resetForSelection: vi.fn(async () => undefined),
    cancelAndNavigate: vi.fn(async () => undefined),
    retryFatal: vi.fn(async () => undefined),
    finish: vi.fn(async () => undefined),
    hasResumableRun: vi.fn(() => false),
    clearError: vi.fn(),
  } as EffortWizardStore;
}

function run(overrides: Partial<EffortWizardRunState> = {}): EffortWizardRunState {
  return {
    runId: "run-1",
    status: "review_ready",
    selectedProviders: ["codex"],
    providers: [],
    review: null,
    completion: null,
    error: null,
    ...overrides,
  };
}

describe("EffortWizardPage integration states", () => {
  let dispose: (() => void) | undefined;

  afterEach(() => {
    dispose?.();
    dispose = undefined;
    document.body.innerHTML = "";
  });

  it("shows Will check, Skipped, and disabled Unavailable selection rows", () => {
    const wizard = makeWizard();
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <EffortWizardPage wizard={wizard} onNavigate={() => undefined} onDone={() => undefined} />, container);

    const rows = [...container.querySelectorAll<HTMLElement>(".effort-wizard-provider-row")];
    expect(rows.map((row) => row.textContent?.replace(/\s+/g, " ").trim())).toEqual([
      "CodexWill checkAvailable",
      "Claude CodeSkippedAvailable",
      "CursorUnavailableUnavailable",
      "AntigravitySkippedAvailable",
    ]);
    expect(rows[2].querySelector("input")?.disabled).toBe(true);
  });

  it("keeps Start checks disabled when no provider is selected", () => {
    const wizard = makeWizard({ selectedProviders: [] });
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <EffortWizardPage wizard={wizard} onNavigate={() => undefined} onDone={() => undefined} />, container);

    expect(container.querySelector<HTMLButtonElement>("button.effort-wizard-primary-button")?.disabled).toBe(true);
  });

  it("forwards provider checkbox toggles and keeps unavailable providers disabled", () => {
    const wizard = makeWizard();
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <EffortWizardPage wizard={wizard} onNavigate={() => undefined} onDone={() => undefined} />, container);

    container.querySelector<HTMLInputElement>('input[aria-label="Check Codex"]')!.click();
    expect(wizard.toggleProvider).toHaveBeenCalledWith("codex");
    expect(container.querySelector<HTMLInputElement>('input[aria-label="Check Cursor"]')?.disabled).toBe(true);
  });

  it("treats no-recommendation as a terminal neutral running row", () => {
    const wizard = makeWizard({
      runState: run({
        status: "checking",
        providers: [{ provider: "codex", selected: true, status: "no_recommendation", currentEfforts: emptyEfforts(), recommendation: null, probeResults: [] }],
      }),
    });
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <EffortWizardPage wizard={wizard} onNavigate={() => undefined} onDone={() => undefined} />, container);

    expect(container.textContent).toContain("1 of 1 providers checked");
    expect(container.textContent).toContain("No recommendation");
    expect(container.querySelector(".effort-wizard-readonly-row")?.getAttribute("data-status")).toBe("no_recommendation");
  });

  it("renders no-recommendation without tier checkboxes while confirmed tiers stay independently checkable", () => {
    const recommendation = {
      provider: "codex" as const,
      presetRevision: 2,
      confirmed: { low: ["--model", "low-model"], default: ["--model", "default-model"], high: null },
      deterministicComplete: true,
    };
    const reviewProviders = [
      { provider: "codex" as const, currentEfforts: emptyEfforts(), recommendation, status: "review_ready" as const, error: null },
      { provider: "claude" as const, currentEfforts: emptyEfforts(), recommendation: null, status: "no_recommendation" as const, error: "no bundled recommendation" },
    ];
    const wizard = makeWizard({
      runState: run({
        review: { providers: reviewProviders, reviewableProviderCount: 1 },
        providers: reviewProviders.map((provider) => ({ ...provider, selected: true, probeResults: [] })),
      }),
      decisionDrafts: { "run-1": { codex: { low: "update", default: "update", high: "keep_current" } } },
    });
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <EffortWizardPage wizard={wizard} onNavigate={() => undefined} onDone={() => undefined} />, container);

    expect(container.textContent).toContain("No bundled recommendation is available for this account");
    expect(container.textContent).toContain("No recommendation");
    expect(container.querySelectorAll<HTMLInputElement>('input[type="checkbox"]').length).toBe(2);
    expect(container.textContent).not.toContain("Unchanged");
  });

  it("renders a checkbox when current and recommended profiles are equal", () => {
    const profile = ["--model", "same-model"];
    const recommendation = {
      provider: "codex" as const,
      presetRevision: 2,
      confirmed: { low: profile, default: null, high: null },
      deterministicComplete: true,
    };
    const provider = {
      provider: "codex" as const,
      currentEfforts: { low: profile, default: [], high: [] },
      recommendation,
      status: "review_ready" as const,
      error: null,
    };
    const wizard = makeWizard({
      runState: run({
        review: { providers: [provider], reviewableProviderCount: 1 },
        providers: [{ ...provider, selected: true, probeResults: [] }],
      }),
      decisionDrafts: { "run-1": { codex: { low: "update", default: "keep_current", high: "keep_current" } } },
    });
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <EffortWizardPage wizard={wizard} onNavigate={() => undefined} onDone={() => undefined} />, container);

    expect(container.querySelector<HTMLInputElement>('input[aria-label="Update Codex low to recommended profile"]')).not.toBeNull();
    expect(container.textContent).not.toContain("Unchanged");
  });

  it("toggles confirmed tiers independently", () => {
    const recommendation = {
      provider: "codex" as const,
      presetRevision: 2,
      confirmed: { low: ["--model", "low-model"], default: ["--model", "default-model"], high: null },
      deterministicComplete: true,
    };
    const provider = {
      provider: "codex" as const,
      currentEfforts: emptyEfforts(),
      recommendation,
      status: "review_ready" as const,
      error: null,
    };
    const wizard = makeWizard({
      runState: run({ review: { providers: [provider], reviewableProviderCount: 1 }, providers: [{ ...provider, selected: true, probeResults: [] }] }),
      decisionDrafts: { "run-1": { codex: { low: "update", default: "update", high: "keep_current" } } },
    });
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <EffortWizardPage wizard={wizard} onNavigate={() => undefined} onDone={() => undefined} />, container);

    container.querySelector<HTMLInputElement>('input[aria-label="Update Codex low to recommended profile"]')!.click();
    container.querySelector<HTMLInputElement>('input[aria-label="Update Codex default to recommended profile"]')!.click();
    expect(wizard.setTierDecision).toHaveBeenNthCalledWith(1, "run-1", "codex", "low", false);
    expect(wizard.setTierDecision).toHaveBeenNthCalledWith(2, "run-1", "codex", "default", false);
  });

  it("keeps Apply enabled and counts providers when all tiers are unchecked", () => {
    const recommendation = {
      provider: "codex" as const,
      presetRevision: 2,
      confirmed: { low: ["--model", "low-model"], default: ["--model", "default-model"], high: null },
      deterministicComplete: true,
    };
    const provider = {
      provider: "codex" as const,
      currentEfforts: emptyEfforts(),
      recommendation,
      status: "review_ready" as const,
      error: null,
    };
    const wizard = makeWizard({
      runState: run({ review: { providers: [provider], reviewableProviderCount: 1 }, providers: [{ ...provider, selected: true, probeResults: [] }] }),
      tierDecision: vi.fn(() => "keep_current"),
    });
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <EffortWizardPage wizard={wizard} onNavigate={() => undefined} onDone={() => undefined} />, container);

    const apply = [...container.querySelectorAll<HTMLButtonElement>("button")].find((button) => button.textContent?.includes("Apply settings"));
    expect(apply?.disabled).toBe(false);
    expect(apply?.textContent).toContain("1 provider");
  });

  it("renders non-reviewable provider states without tier checkboxes", () => {
    const providers = [
      { provider: "codex" as const, status: "probe_error" as const, error: "timeout", recommendation: null },
      { provider: "claude" as const, status: "unavailable" as const, error: null, recommendation: null },
      { provider: "cursor" as const, status: "skipped" as const, error: null, recommendation: null },
      { provider: "antigravity" as const, status: "no_recommendation" as const, error: "not entitled", recommendation: null },
    ].map((provider) => ({ ...provider, currentEfforts: emptyEfforts() }));
    const wizard = makeWizard({
      runState: run({
        review: { providers, reviewableProviderCount: 0 },
        providers: providers.map((provider) => ({ ...provider, selected: provider.status !== "skipped", probeResults: [] })),
      }),
    });
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <EffortWizardPage wizard={wizard} onNavigate={() => undefined} onDone={() => undefined} />, container);

    expect(container.querySelectorAll<HTMLInputElement>('input[type="checkbox"]').length).toBe(0);
    expect(container.textContent).toContain("Needs attention");
    expect(container.textContent).toContain("Unavailable");
    expect(container.textContent).toContain("Skipped");
    expect(container.textContent).toContain("No recommendation");
  });

  it("renders distinct completion labels including no recommendation", () => {
    const wizard = makeWizard({
      runState: run({
        status: "completed",
        completion: {
          confirmedProviderCount: 1,
          providers: [
            { provider: "codex", status: "updated_or_confirmed" },
            { provider: "claude", status: "kept_current" },
            { provider: "cursor", status: "needs_attention" },
            { provider: "antigravity", status: "no_recommendation" },
          ],
        },
      }),
    });
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <EffortWizardPage wizard={wizard} onNavigate={() => undefined} onDone={() => undefined} />, container);

    expect(container.textContent).toContain("Updated / Reviewed");
    expect(container.textContent).toContain("Kept current");
    expect(container.textContent).toContain("Needs attention");
    expect(container.textContent).toContain("No recommendation");
  });

  it("renders every canonical completion state", () => {
    const wizard = makeWizard({
      runState: run({
        status: "completed",
        completion: {
          confirmedProviderCount: 1,
          providers: [
            { provider: "codex", status: "updated_or_confirmed" },
            { provider: "claude", status: "kept_current" },
            { provider: "cursor", status: "needs_attention" },
            { provider: "antigravity", status: "unavailable" },
          ],
        },
      }),
    });
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <EffortWizardPage wizard={wizard} onNavigate={() => undefined} onDone={() => undefined} />, container);

    expect(container.textContent).toContain("Updated / Reviewed");
    expect(container.textContent).toContain("Kept current");
    expect(container.textContent).toContain("Needs attention");
    expect(container.textContent).toContain("Unavailable");
    expect(container.textContent).not.toContain("OpenCode");
    expect(container.textContent).not.toContain("Ollama");
  });

  it("renders the skipped completion state distinctly", () => {
    const wizard = makeWizard({
      runState: run({
        status: "completed",
        completion: {
          confirmedProviderCount: 0,
          providers: [{ provider: "codex", status: "skipped" }],
        },
      }),
    });
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <EffortWizardPage wizard={wizard} onNavigate={() => undefined} onDone={() => undefined} />, container);

    expect(container.textContent).toContain("Skipped");
  });
});
