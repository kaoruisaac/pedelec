import { describe, expect, it } from "vitest";
import {
  availableWizardProviders,
  defaultWizardSelection,
  formatEffortProfile,
  hasAvailableWizardProvider,
  runningProgress,
  wizardProviderSelectionLabel,
} from "./effortWizardSelectors";
import type { EffortWizardBootstrap, EffortWizardRunState } from "./types";

function bootstrap(overrides: Partial<EffortWizardBootstrap["providers"][number]>[] = []): EffortWizardBootstrap {
  const providerDefaults = {
    codex: { available: true, presetUpdateAvailable: false },
    claude: { available: true, presetUpdateAvailable: false },
    cursor: { available: false, presetUpdateAvailable: false },
    antigravity: { available: true, presetUpdateAvailable: true },
  } as const;
  return {
    providers: Object.entries(providerDefaults).map(([provider, value]) => ({
      provider: provider as EffortWizardBootstrap["providers"][number]["provider"],
      version: null,
      currentPresetRevision: 2,
      appliedPresetRevision: 2,
      hasAnyEffortSetting: false,
      ...value,
      ...overrides.find((item) => item.provider === provider),
    })),
    homeReminder: null,
  };
}

describe("effort wizard selectors", () => {
  it("keeps only the four supported providers in canonical order", () => {
    expect(availableWizardProviders(bootstrap())).toEqual(["codex", "claude", "antigravity"]);
  });

  it("selects every available provider for initial and settings entries", () => {
    const value = bootstrap();
    expect(defaultWizardSelection("home-initial", value)).toEqual(["codex", "claude", "antigravity"]);
    expect(defaultWizardSelection("settings", value)).toEqual(["codex", "claude", "antigravity"]);
  });

  it("selects only available outdated providers for Home update", () => {
    expect(defaultWizardSelection("home-update", bootstrap())).toEqual(["antigravity"]);
  });

  it("never treats an unavailable provider as available or selected", () => {
    const value = bootstrap([{ provider: "antigravity", available: false, presetUpdateAvailable: true }]);
    expect(hasAvailableWizardProvider(value)).toBe(true);
    expect(defaultWizardSelection("home-update", value)).toEqual([]);
  });

  it("labels available provider rows by this run's selection", () => {
    const value = bootstrap();
    const codex = value.providers.find((provider) => provider.provider === "codex")!;
    const cursor = value.providers.find((provider) => provider.provider === "cursor")!;
    expect(wizardProviderSelectionLabel(codex, ["codex"])).toBe("Will check");
    expect(wizardProviderSelectionLabel(codex, [])).toBe("Skipped");
    expect(wizardProviderSelectionLabel(cursor, ["cursor"])).toBe("Unavailable");
  });

  it("formats provider argv into a readable profile", () => {
    expect(formatEffortProfile("codex", ["-m", "gpt-5.6-luna", "-c", 'model_reasoning_effort="max"']))
      .toBe("gpt-5.6-luna · max");
    expect(formatEffortProfile("claude", ["--model", "claude-opus-4-8", "--effort", "medium"]))
      .toBe("claude-opus-4-8 · medium");
    expect(formatEffortProfile("cursor", ["--mystery", "value"])).toBe("--mystery value");
  });

  it("calculates running progress from selected providers only", () => {
    const run: EffortWizardRunState = {
      runId: "run",
      status: "checking",
      selectedProviders: ["codex", "cursor"],
      providers: [
        { provider: "codex", selected: true, status: "review_ready", currentEfforts: { low: [], default: [], high: [] }, recommendation: null, probeResults: [] },
        { provider: "claude", selected: false, status: "skipped", currentEfforts: { low: [], default: [], high: [] }, recommendation: null, probeResults: [] },
        { provider: "cursor", selected: true, status: "checking", currentEfforts: { low: [], default: [], high: [] }, recommendation: null, probeResults: [] },
        { provider: "antigravity", selected: false, status: "unavailable", currentEfforts: { low: [], default: [], high: [] }, recommendation: null, probeResults: [] },
      ],
    };
    expect(runningProgress(run)).toEqual({ completed: 1, total: 2, percent: 50 });
  });

  it("treats no-recommendation as terminal without counting it as unavailable", () => {
    const run: EffortWizardRunState = {
      runId: "run",
      status: "checking",
      selectedProviders: ["codex"],
      providers: [
        { provider: "codex", selected: true, status: "no_recommendation", currentEfforts: { low: [], default: [], high: [] }, recommendation: null, probeResults: [] },
        { provider: "claude", selected: false, status: "skipped", currentEfforts: { low: [], default: [], high: [] }, recommendation: null, probeResults: [] },
      ],
    };
    expect(runningProgress(run)).toEqual({ completed: 1, total: 1, percent: 100 });
  });
});
