import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  ApplyEffortWizardOutput,
  EffortWizardBootstrap,
  EffortWizardHomeReminder,
  EffortWizardProviderDecision,
  EffortWizardProviderRunState,
  EffortWizardRunState,
  EffortWizardSettingsError,
  WizardProviderCode,
} from "./types";

export interface EffortWizardStartInput {
  providers: WizardProviderCode[];
}

export interface EffortWizardApplyInput {
  runId: string;
  providers: EffortWizardProviderDecision[];
}

export function getEffortWizardBootstrap(): Promise<EffortWizardBootstrap> {
  return invoke<EffortWizardBootstrap>("get_effort_wizard_bootstrap").then(normalizeBootstrap);
}

export function getEffortWizardState(): Promise<EffortWizardRunState | null> {
  return invoke<EffortWizardRunState | null>("get_effort_wizard_state").then((state) => state ? normalizeRunState(state) : null);
}

export function startEffortWizard(providers: WizardProviderCode[]): Promise<EffortWizardRunState> {
  return invoke<EffortWizardRunState>("start_effort_wizard", {
    input: { providers } satisfies EffortWizardStartInput,
  }).then(normalizeRunState);
}

export function applyEffortWizardSettings(
  input: EffortWizardApplyInput,
): Promise<ApplyEffortWizardOutput> {
  const wireInput = {
    runId: input.runId,
    providers: input.providers.map((provider) => ({
      provider: provider.provider,
      tiers: {
        low: toBackendTierDecision(provider.tiers.low),
        default: toBackendTierDecision(provider.tiers.default),
        high: toBackendTierDecision(provider.tiers.high),
      },
    })),
  };
  return invoke<ApplyEffortWizardOutput>("apply_effort_wizard_settings", { input: wireInput }).then((output) => ({
    state: normalizeRunState(output.state),
    bootstrap: normalizeBootstrap(output.bootstrap),
  }));
}

function toBackendTierDecision(decision: "update" | "keep_current"): "update" | "keepCurrent" {
  return decision === "update" ? "update" : "keepCurrent";
}

export function resetEffortWizard(): Promise<void> {
  return invoke<void>("reset_effort_wizard");
}

export function refreshProviders(): Promise<unknown> {
  return invoke("refresh_providers");
}

export function listenToEffortWizardState(
  callback: (state: EffortWizardRunState | null) => void,
): Promise<UnlistenFn> {
  return listen<unknown>("effort_wizard_state_changed", (event) => {
    callback(event.payload ? normalizeRunState(event.payload as EffortWizardRunState) : null);
  });
}

export function normalizeBootstrap(value: EffortWizardBootstrap): EffortWizardBootstrap {
  const raw = value as unknown as {
    providers?: Array<Record<string, unknown>>;
    homeReminder?: unknown;
  };
  return {
    providers: Array.isArray(raw.providers)
      ? raw.providers.map((provider) => ({
        provider: provider.provider as EffortWizardBootstrap["providers"][number]["provider"],
        available: provider.available === true,
        version: (provider.version as string | null | undefined) ?? null,
        currentPresetRevision: Number(provider.currentPresetRevision ?? 0),
        appliedPresetRevision: (provider.appliedPresetRevision as number | null | undefined) ?? null,
        hasAnyEffortSetting: provider.hasAnyEffortSetting === true,
        presetUpdateAvailable: provider.presetUpdateAvailable === true,
      }))
      : [],
    homeReminder: normalizeHomeReminder(raw.homeReminder),
  };
}

export function normalizeHomeReminder(value: unknown): EffortWizardHomeReminder | null {
  if (value === "initialSetup" || value === "initial_setup") {
    return { type: "initial_setup" };
  }
  if (!value || typeof value !== "object") return null;

  const record = value as Record<string, unknown>;
  const preset = record.presetUpdate ?? record.preset_update;
  if (preset && typeof preset === "object") {
    const providers = (preset as { providers?: unknown }).providers;
    return {
      type: "preset_update",
      providers: Array.isArray(providers) ? providers as EffortWizardBootstrap["providers"][number]["provider"][] : [],
    };
  }
  if (record.initialSetup !== undefined || record.initial_setup !== undefined) {
    return { type: "initial_setup" };
  }
  return null;
}

function normalizeRunState(value: EffortWizardRunState): EffortWizardRunState {
  const raw = value as unknown as Record<string, unknown>;
  return {
    ...value,
    runId: String(raw.runId ?? ""),
    selectedProviders: Array.isArray(raw.selectedProviders) ? raw.selectedProviders as WizardProviderCode[] : [],
    providers: Array.isArray(raw.providers) ? raw.providers as EffortWizardProviderRunState[] : [],
    review: raw.review ? raw.review as EffortWizardRunState["review"] : null,
    completion: raw.completion ? raw.completion as EffortWizardRunState["completion"] : null,
    error: raw.error ? raw.error as EffortWizardRunState["error"] : null,
    bootstrap: raw.bootstrap ? normalizeBootstrap(raw.bootstrap as EffortWizardBootstrap) : null,
  };
}

export function errorDetails(error: unknown): EffortWizardSettingsError {
  if (typeof error === "string") return { message: error };
  if (error && typeof error === "object") {
    const value = error as EffortWizardSettingsError;
    return { code: value.code, message: value.message, details: value.details };
  }
  return { message: String(error) };
}

export function formatApiError(error: unknown): string {
  const details = errorDetails(error);
  if (details.code && details.message) return `${details.code}: ${details.message}`;
  return details.message || "Unknown effort wizard error";
}
