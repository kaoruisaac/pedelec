import type { EffortLevel, EffortsArgs } from "../settings/types";
import {
  WIZARD_PROVIDER_NAMES,
  WIZARD_PROVIDER_ORDER,
  type EffortWizardBootstrap,
  type EffortWizardEntryOrigin,
  type EffortWizardProviderBootstrap,
  type EffortWizardProviderRecommendation,
  type EffortWizardProviderRunState,
  type EffortWizardRunState,
  type WizardProviderCode,
} from "./types";

export const WIZARD_EFFORT_LEVELS: EffortLevel[] = ["low", "default", "high"];

export function wizardProviderName(provider: WizardProviderCode): string {
  return WIZARD_PROVIDER_NAMES[provider];
}

export function wizardProviderBootstrap(
  bootstrap: EffortWizardBootstrap | null,
  provider: WizardProviderCode,
): EffortWizardProviderBootstrap | undefined {
  return bootstrap?.providers.find((item) => item.provider === provider);
}

export function availableWizardProviders(bootstrap: EffortWizardBootstrap | null): WizardProviderCode[] {
  return WIZARD_PROVIDER_ORDER.filter((provider) => wizardProviderBootstrap(bootstrap, provider)?.available === true);
}

export function hasAvailableWizardProvider(bootstrap: EffortWizardBootstrap | null): boolean {
  return availableWizardProviders(bootstrap).length > 0;
}

export function defaultWizardSelection(
  origin: EffortWizardEntryOrigin,
  bootstrap: EffortWizardBootstrap | null,
): WizardProviderCode[] {
  const available = availableWizardProviders(bootstrap);
  if (origin !== "home-update") return available;
  return available.filter((provider) => wizardProviderBootstrap(bootstrap, provider)?.presetUpdateAvailable === true);
}

export function wizardProviderSelectionLabel(
  provider: EffortWizardProviderBootstrap,
  selectedProviders: readonly WizardProviderCode[],
): "Will check" | "Skipped" | "Unavailable" {
  if (!provider.available) return "Unavailable";
  return selectedProviders.includes(provider.provider) ? "Will check" : "Skipped";
}

export function isRunResumable(run: EffortWizardRunState | null): boolean {
  return Boolean(run && run.status !== "cancelled");
}

export function isSelectedProviderTerminal(state: EffortWizardProviderRunState): boolean {
  return state.status === "review_ready"
    || state.status === "probe_error"
    || state.status === "no_recommendation"
    || state.status === "unavailable"
    || state.status === "skipped";
}

export function runningProgress(run: EffortWizardRunState | null): { completed: number; total: number; percent: number } {
  if (!run) return { completed: 0, total: 0, percent: 0 };
  const selected = new Set(run.selectedProviders);
  const total = run.selectedProviders.length;
  const completed = run.providers.filter((provider) => selected.has(provider.provider) && isSelectedProviderTerminal(provider)).length;
  return { completed, total, percent: total === 0 ? 0 : Math.round((completed / total) * 100) };
}

export function recommendationHasTier(
  recommendation: EffortWizardProviderRecommendation | null | undefined,
  level: EffortLevel,
): boolean {
  return recommendation?.confirmed[level] !== undefined && recommendation?.confirmed[level] !== null;
}

export function formatEffortProfile(provider: WizardProviderCode, args: string[] | undefined): string {
  if (!args || args.length === 0) return "Not configured";

  let model: string | undefined;
  let effort: string | undefined;
  const unknown: string[] = [];
  for (let index = 0; index < args.length; index += 1) {
    const key = args[index];
    const value = args[index + 1];
    if (key === "-m" || key === "--model") {
      if (value) model = value;
      index += 1;
      continue;
    }
    if (key === "--effort") {
      if (value) effort = value;
      index += 1;
      continue;
    }
    if (key === "-c" && value) {
      const match = /^model_reasoning_effort\s*=\s*["']?([^"']+)["']?$/.exec(value);
      if (match) {
        effort = match[1];
        index += 1;
        continue;
      }
    }
    unknown.push(key);
    if (value) {
      unknown.push(value);
      index += 1;
    }
  }

  if (model && effort) return `${model} · ${effort}`;
  if (model) return model;
  if (effort) return provider === "codex" ? effort : effort;
  return unknown.join(" ") || args.join(" ");
}

export function formatCurrentToRecommended(
  provider: WizardProviderCode,
  current: string[] | undefined,
  recommended: string[] | undefined,
): string {
  return `${formatEffortProfile(provider, current)} → ${formatEffortProfile(provider, recommended)}`;
}

export function friendlyProbeError(error: string | null | undefined): string {
  const value = (error || "").toLowerCase();
  if (value.includes("auth") || value.includes("sign") || value.includes("login")) return "Authentication needs attention";
  if (value.includes("timeout") || value.includes("timed out")) return "Probe timed out";
  return "Provider check failed";
}

export function countConfiguredWizardProviders(
  settings: Partial<Record<WizardProviderCode, { effortsArgs?: EffortsArgs }>>,
): number {
  return WIZARD_PROVIDER_ORDER.filter((provider) => {
    const efforts = settings[provider]?.effortsArgs;
    return WIZARD_EFFORT_LEVELS.some((level) => (efforts?.[level] ?? []).length > 0);
  }).length;
}
