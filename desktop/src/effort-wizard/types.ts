import type { EffortLevel, EffortsArgs } from "../settings/types";

export type WizardProviderCode = "codex" | "claude" | "cursor" | "antigravity";

export const WIZARD_PROVIDER_ORDER: WizardProviderCode[] = [
  "codex",
  "claude",
  "cursor",
  "antigravity",
];

export const WIZARD_PROVIDER_NAMES: Record<WizardProviderCode, string> = {
  codex: "Codex",
  claude: "Claude Code",
  cursor: "Cursor",
  antigravity: "Antigravity",
};

export type EffortWizardEntryOrigin = "home-initial" | "home-update" | "settings";

export type EffortWizardHomeReminder =
  | { type: "initial_setup" }
  | { type: "preset_update"; providers: WizardProviderCode[] };

export interface EffortWizardProviderBootstrap {
  provider: WizardProviderCode;
  available: boolean;
  version?: string | null;
  currentPresetRevision: number;
  appliedPresetRevision?: number | null;
  hasAnyEffortSetting: boolean;
  presetUpdateAvailable: boolean;
}

export interface EffortWizardBootstrap {
  providers: EffortWizardProviderBootstrap[];
  homeReminder: EffortWizardHomeReminder | null;
}

export interface EffortWizardConfirmedProfiles {
  low?: string[] | null;
  default?: string[] | null;
  high?: string[] | null;
}

export interface EffortWizardProviderRecommendation {
  provider: WizardProviderCode;
  presetRevision: number;
  confirmed: EffortWizardConfirmedProfiles;
  deterministicComplete: boolean;
}

export type EffortWizardProviderStatus =
  | "selected_pending"
  | "checking"
  | "review_ready"
  | "probe_error"
  | "no_recommendation"
  | "unavailable"
  | "skipped";

export interface EffortWizardProbeResultState {
  probeId: string;
  outcome: "supported" | "not_entitled" | "transient_error";
  reason?: string | null;
}

export interface EffortWizardProviderRunState {
  provider: WizardProviderCode;
  selected: boolean;
  status: EffortWizardProviderStatus;
  currentEfforts: EffortsArgs;
  recommendation?: EffortWizardProviderRecommendation | null;
  probeResults: EffortWizardProbeResultState[];
  error?: string | null;
}

export interface EffortWizardProviderReviewState {
  provider: WizardProviderCode;
  currentEfforts: EffortsArgs;
  recommendation?: EffortWizardProviderRecommendation | null;
  status: EffortWizardProviderStatus;
  error?: string | null;
}

export interface EffortWizardReviewState {
  providers: EffortWizardProviderReviewState[];
  reviewableProviderCount: number;
}

export type EffortWizardCompletionStatus =
  | "updated_or_confirmed"
  | "kept_current"
  | "needs_attention"
  | "no_recommendation"
  | "unavailable"
  | "skipped";

export interface EffortWizardProviderCompletionState {
  provider: WizardProviderCode;
  status: EffortWizardCompletionStatus;
}

export interface EffortWizardCompletionState {
  providers: EffortWizardProviderCompletionState[];
  confirmedProviderCount: number;
}

export interface EffortWizardFatalError {
  code: string;
  message: string;
}

export type EffortWizardRunStatus =
  | "checking"
  | "review_ready"
  | "applying"
  | "completed"
  | "fatal_error"
  | "cancelled";

export interface EffortWizardRunState {
  runId: string;
  status: EffortWizardRunStatus;
  selectedProviders: WizardProviderCode[];
  providers: EffortWizardProviderRunState[];
  review?: EffortWizardReviewState | null;
  completion?: EffortWizardCompletionState | null;
  error?: EffortWizardFatalError | null;
  bootstrap?: EffortWizardBootstrap | null;
}

export type EffortWizardTierDecision = "update" | "keep_current";

export type EffortWizardTierDecisions = Record<EffortLevel, EffortWizardTierDecision>;

export interface EffortWizardProviderDecision {
  provider: WizardProviderCode;
  tiers: EffortWizardTierDecisions;
}

export interface EffortWizardSettingsError {
  code?: string;
  message?: string;
  details?: unknown;
}

export interface ApplyEffortWizardOutput {
  state: EffortWizardRunState;
  bootstrap: EffortWizardBootstrap;
}

export type EffortWizardPageStage = "selection" | "no_supported";

export interface EffortWizardStoreState {
  bootstrap: EffortWizardBootstrap | null;
  runState: EffortWizardRunState | null;
  loading: boolean;
  actionLoading: boolean;
  error: string | null;
  initialized: boolean;
  entryOrigin: EffortWizardEntryOrigin;
  selectedProviders: WizardProviderCode[];
  pageStage: EffortWizardPageStage;
  settingsRefreshRevision: number;
  decisionDrafts: Record<string, Partial<Record<WizardProviderCode, EffortWizardTierDecisions>>>;
}
