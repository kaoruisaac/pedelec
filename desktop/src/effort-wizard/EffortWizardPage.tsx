import { createMemo, For, Show } from "solid-js";
import type { EffortLevel } from "../settings/types";
import {
  formatCurrentToRecommended,
  formatEffortProfile,
  friendlyProbeError,
  runningProgress,
  wizardProviderBootstrap,
  wizardProviderName,
  wizardProviderSelectionLabel,
  WIZARD_EFFORT_LEVELS,
} from "./effortWizardSelectors";
import type { EffortWizardStore } from "./effortWizardStore";
import { WIZARD_PROVIDER_ORDER } from "./types";
import type {
  EffortWizardProviderRunState,
  EffortWizardProviderRecommendation,
  EffortWizardRunState,
  WizardProviderCode,
} from "./types";
import "./effortWizard.css";

interface EffortWizardPageProps {
  wizard: EffortWizardStore;
  onNavigate: (page: "home" | "settings") => void;
  onDone: () => void;
}

export default function EffortWizardPage(props: EffortWizardPageProps) {
  const wizard = props.wizard;
  const run = createMemo(() => wizard.store.runState);
  const stage = createMemo(() => {
    const state = run();
    if (!state) return wizard.store.pageStage;
    if (state.status === "fatal_error") return "fatal";
    if (state.status === "completed") return "complete";
    if (state.status === "review_ready" || state.status === "applying") return "review";
    return "running";
  });
  const originPage = () => wizard.store.entryOrigin === "settings" ? "settings" : "home";

  return (
    <main class="effort-wizard-page">
      <Show when={stage() !== "fatal" && stage() !== "complete"}>
        <WizardHeader stage={stage()} />
      </Show>

      <Show when={stage() === "selection"}>
        <SelectionScreen wizard={wizard} onCancel={() => props.onNavigate(originPage())} />
      </Show>
      <Show when={stage() === "no_supported"}>
        <NoSupportedScreen
          wizard={wizard}
          onBack={() => props.onNavigate("settings")}
        />
      </Show>
      <Show when={stage() === "running"}>
        <RunningScreen run={run()} />
      </Show>
      <Show when={stage() === "review"}>
        <ReviewScreen
          wizard={wizard}
          run={run()}
          onCancel={() => {
            void wizard.cancelAndNavigate().then(() => props.onNavigate(originPage()));
          }}
          onRetry={() => void wizard.retryFatal()}
        />
      </Show>
      <Show when={stage() === "complete"}>
        <CompletionScreen wizard={wizard} run={run()} onDone={props.onDone} />
      </Show>
      <Show when={stage() === "fatal"}>
        <FatalScreen
          wizard={wizard}
          run={run()}
          onTryAgain={() => void wizard.retryFatal()}
          onBack={() => props.onNavigate("settings")}
        />
      </Show>
    </main>
  );
}

function WizardHeader(props: { stage: string }) {
  const checkComplete = () => props.stage === "review";
  const reviewActive = () => props.stage === "review";
  return (
    <header class="effort-wizard-header">
      <div>
        <span class="effort-wizard-eyebrow">Effort Setting Wizard</span>
        <h1>
          {props.stage === "selection"
            ? "Choose providers to check"
            : props.stage === "no_supported"
              ? "Effort recommendations"
              : props.stage === "running"
                ? "Checking effort recommendations"
                : "Review recommended effort settings"}
        </h1>
      </div>
      <Stepper checkComplete={checkComplete()} reviewActive={reviewActive()} />
    </header>
  );
}

function Stepper(props: { checkComplete: boolean; reviewActive: boolean }) {
  return (
    <ol class="effort-wizard-stepper" aria-label="Wizard progress">
      <li classList={{ "is-active": !props.checkComplete, "is-complete": props.checkComplete }}>
        <span>1</span><strong>Check</strong>
      </li>
      <li classList={{ "is-active": props.reviewActive }}>
        <span>2</span><strong>Review</strong>
      </li>
      <li>
        <span>3</span><strong>Apply</strong>
      </li>
    </ol>
  );
}

function SelectionScreen(props: { wizard: EffortWizardStore; onCancel: () => void }) {
  const wizard = props.wizard;
  const providers = createMemo(() => WIZARD_PROVIDER_ORDER.map((provider) => wizardProviderBootstrap(wizard.store.bootstrap, provider) ?? ({
    provider,
    available: false,
    version: null,
    currentPresetRevision: 0,
    appliedPresetRevision: null,
    hasAnyEffortSetting: false,
    presetUpdateAvailable: false,
  })));
  const availableCount = createMemo(() => providers().filter((provider) => provider.available).length);
  const unavailableCount = createMemo(() => providers().filter((provider) => !provider.available).length);
  const canStart = createMemo(() => wizard.store.selectedProviders.length > 0 && !wizard.store.actionLoading);

  return (
    <section class="effort-wizard-shell effort-wizard-selection" aria-labelledby="effort-wizard-selection-title">
      <div class="effort-wizard-intro">
        <h2 id="effort-wizard-selection-title">Select providers for this check</h2>
        <p>Choose which available providers Pedelec should probe in this run. Nothing changes before you review and apply.</p>
      </div>
      <div class="effort-wizard-provider-count">{availableCount()} available · {unavailableCount()} unavailable</div>

      <div class="effort-wizard-provider-list">
        <For each={providers()}>
          {(provider) => (
            <label
              class="effort-wizard-provider-row"
              classList={{
                "is-selected": wizard.store.selectedProviders.includes(provider.provider),
                "is-unavailable": !provider.available,
              }}
            >
              <input
                type="checkbox"
                checked={wizard.store.selectedProviders.includes(provider.provider)}
                disabled={!provider.available || wizard.store.actionLoading}
                aria-label={`Check ${wizardProviderName(provider.provider)}`}
                onChange={() => wizard.toggleProvider(provider.provider)}
              />
              <span class="effort-wizard-provider-copy">
                <strong>{wizardProviderName(provider.provider)}</strong>
                <span>{wizardProviderSelectionLabel(provider, wizard.store.selectedProviders)}</span>
              </span>
              <span class="effort-wizard-row-status" data-status={provider.available ? "available" : "neutral"}>
                {provider.available ? "Available" : "Unavailable"}
              </span>
            </label>
          )}
        </For>
      </div>

      <p class="effort-wizard-note">You can change this selection any time before starting checks.</p>
      <Show when={wizard.store.error}>
        <div class="effort-wizard-alert is-error" role="alert">{wizard.store.error}</div>
      </Show>
      <div class="effort-wizard-actions">
        <button type="button" class="effort-wizard-secondary-button" onClick={props.onCancel} disabled={wizard.store.actionLoading}>Cancel</button>
        <button
          type="button"
          class="effort-wizard-primary-button"
          disabled={!canStart()}
          onClick={() => void wizard.startSelectedProviders()}
        >
          {wizard.store.actionLoading ? "Starting checks..." : "Start checks"}
        </button>
      </div>
    </section>
  );
}

function NoSupportedScreen(props: { wizard: EffortWizardStore; onBack: () => void }) {
  return (
    <section class="effort-wizard-shell effort-wizard-empty-state">
      <div class="effort-wizard-empty-icon" aria-hidden="true">!</div>
      <h2>No supported provider is currently available</h2>
      <p>The Effort Wizard needs at least one available provider from Codex, Claude Code, Cursor, or Antigravity.</p>
      <Show when={props.wizard.store.error}>
        <div class="effort-wizard-alert is-error" role="alert">{props.wizard.store.error}</div>
      </Show>
      <div class="effort-wizard-actions">
        <button type="button" class="effort-wizard-secondary-button" onClick={props.onBack}>Back to Settings</button>
        <button
          type="button"
          class="effort-wizard-primary-button"
          disabled={props.wizard.store.actionLoading}
          onClick={() => void props.wizard.refreshProviderStatus()}
        >
          {props.wizard.store.actionLoading ? "Refreshing..." : "Refresh provider status"}
        </button>
      </div>
    </section>
  );
}

function RunningScreen(props: { run: EffortWizardRunState | null }) {
  const progress = createMemo(() => runningProgress(props.run));
  const rows = createMemo(() => props.run?.providers ?? []);
  return (
    <section class="effort-wizard-shell effort-wizard-running" aria-labelledby="effort-wizard-running-title">
      <div class="effort-wizard-intro">
        <h2 id="effort-wizard-running-title">Checking selected providers</h2>
        <p>Pedelec is checking the providers you selected. You can leave this page; the check will continue.</p>
      </div>
      <div class="effort-wizard-progress-summary">
        <div class="effort-wizard-progress-label">
          <strong>{progress().completed} of {progress().total} providers checked</strong>
          <span>{progress().percent}%</span>
        </div>
        <div class="effort-wizard-progress-track" aria-hidden="true"><span style={{ width: `${progress().percent}%` }} /></div>
      </div>
      <div class="effort-wizard-provider-list">
        <For each={rows()}>{(provider) => <RunningProviderRow provider={provider} />}</For>
      </div>
      <p class="effort-wizard-note">The Review step will appear when all selected providers reach a terminal result.</p>
    </section>
  );
}

function RunningProviderRow(props: { provider: EffortWizardProviderRunState }) {
  const status = () => runningStatusLabel(props.provider);
  return (
    <div class="effort-wizard-provider-row effort-wizard-readonly-row" data-status={props.provider.status}>
      <span class="effort-wizard-status-dot" aria-hidden="true" />
      <span class="effort-wizard-provider-copy">
        <strong>{wizardProviderName(props.provider.provider)}</strong>
        <span>{props.provider.selected ? status() : "Skipped"}</span>
      </span>
      <span class="effort-wizard-row-status" data-status={rowStatusTone(props.provider.status)}>{props.provider.selected ? status() : "Skipped"}</span>
    </div>
  );
}

function runningStatusLabel(provider: EffortWizardProviderRunState): string {
  switch (provider.status) {
    case "selected_pending": return "Pending";
    case "checking": return "Checking";
    case "review_ready": return "Ready";
    case "probe_error": return "Needs attention";
    case "no_recommendation": return "No recommendation";
    case "unavailable": return "Unavailable";
    case "skipped": return "Skipped";
  }
}

function rowStatusTone(status: EffortWizardProviderRunState["status"]): string {
  return status === "review_ready" ? "success" : status === "probe_error" ? "attention" : "neutral";
}

function ReviewScreen(props: {
  wizard: EffortWizardStore;
  run: EffortWizardRunState | null;
  onCancel: () => void;
  onRetry: () => void;
}) {
  const wizard = props.wizard;
  const review = createMemo(() => props.run?.review);
  const providers = createMemo(() => review()?.providers ?? props.run?.providers ?? []);
  const readyCount = createMemo(() => review()?.reviewableProviderCount ?? 0);
  const attentionCount = createMemo(() => providers().filter((provider) => provider.status === "probe_error").length);
  const unavailableCount = createMemo(() => providers().filter((provider) => provider.status === "unavailable").length);
  const hasApplyError = createMemo(() => Boolean(wizard.store.error || props.run?.error));
  const applyNote = createMemo(() => buildApplyNote(wizard, props.run, providers()));
  const applying = createMemo(() => wizard.store.actionLoading || props.run?.status === "applying");

  return (
    <section class="effort-wizard-shell effort-wizard-review" aria-labelledby="effort-wizard-review-title">
      <div class="effort-wizard-intro">
        <h2 id="effort-wizard-review-title">Review recommended effort settings</h2>
        <p>Nothing has changed yet. Confirmed tiers show current → recommended. Unconfirmed tiers keep existing values.</p>
      </div>

      <div class="effort-wizard-metrics" aria-label="Review summary">
        <Metric label="Ready to apply" value={readyCount()} tone="success" />
        <Metric label="Needs attention" value={attentionCount()} tone="attention" />
        <Metric label="Unavailable" value={unavailableCount()} tone="neutral" />
      </div>

      <div class="effort-wizard-review-list">
        <For each={providers()}>
          {(provider) => <ReviewProviderCard wizard={wizard} run={props.run} provider={provider} disabled={applying()} />}
        </For>
      </div>

      <div class="effort-wizard-apply-note">
        <strong>Before you apply</strong>
        <p>{applyNote()}</p>
        <span>Apply confirms the recommendations you reviewed. Unchecked tiers keep their current values.</span>
      </div>

      <Show when={hasApplyError()}>
        <div class="effort-wizard-alert is-error" role="alert">
          <strong>{props.run?.error?.code === "EFFORT_WIZARD_SETTINGS_CHANGED" || wizard.store.error?.startsWith("EFFORT_WIZARD_SETTINGS_CHANGED")
            ? "Settings changed while this review was open. Run the checks again before applying."
            : props.run?.error?.message || wizard.store.error}</strong>
          <Show when={props.run?.error?.code === "EFFORT_WIZARD_SETTINGS_CHANGED" || wizard.store.error?.startsWith("EFFORT_WIZARD_SETTINGS_CHANGED")}>
            <button type="button" class="effort-wizard-inline-button" onClick={props.onRetry}>Run checks again</button>
          </Show>
        </div>
      </Show>

      <div class="effort-wizard-actions">
        <button type="button" class="effort-wizard-secondary-button" onClick={props.onCancel} disabled={applying()}>Cancel</button>
        <button
          type="button"
          class="effort-wizard-primary-button"
          disabled={readyCount() === 0 || applying()}
          onClick={() => void wizard.applyReview()}
        >
          {applying() ? "Applying..." : `Apply settings · ${readyCount()} providers`}
        </button>
      </div>
    </section>
  );
}

function Metric(props: { label: string; value: number; tone: string }) {
  return <div class="effort-wizard-metric"><span>{props.label}</span><strong data-status={props.tone}>{props.value}</strong></div>;
}

function ReviewProviderCard(props: {
  wizard: EffortWizardStore;
  run: EffortWizardRunState | null;
  provider: EffortWizardProviderRunState | { provider: WizardProviderCode; currentEfforts: EffortWizardProviderRunState["currentEfforts"]; recommendation?: EffortWizardProviderRunState["recommendation"]; status: EffortWizardProviderRunState["status"]; error?: string | null };
  disabled: boolean;
}) {
  const provider = props.provider;
  const recommendation = () => provider.recommendation;
  const isReviewable = () => provider.status === "review_ready" && Boolean(recommendation());
  const isSkipped = () => provider.status === "skipped";
  const isAttention = () => provider.status === "probe_error";
  const isNoRecommendation = () => provider.status === "no_recommendation";
  const isUnavailable = () => provider.status === "unavailable";

  return (
    <article class="effort-wizard-review-card" classList={{ "is-attention": isAttention(), "is-neutral": isSkipped() || isUnavailable() || isNoRecommendation() }}>
      <header class="effort-wizard-review-card-header">
        <div>
          <h3>{wizardProviderName(provider.provider)}</h3>
          <p>
            {isReviewable()
              ? "Recommendation ready to review"
              : isAttention()
                ? `Probe error · existing settings will be kept`
                : isSkipped()
                  ? "Not selected for this run"
                  : isNoRecommendation()
                    ? "No bundled recommendation is available for this account"
                  : isUnavailable()
                    ? "Unavailable · existing settings will be kept"
                    : "No recommendation available"}
          </p>
        </div>
        <span class="effort-wizard-row-status" data-status={isReviewable() ? "success" : isAttention() ? "attention" : "neutral"}>
          {isReviewable()
            ? "Ready to apply"
            : isAttention()
              ? "Needs attention"
              : isSkipped()
                ? "Skipped"
                : isNoRecommendation()
                  ? "No recommendation"
                  : "Unavailable"}
        </span>
      </header>

      <Show when={isReviewable()} fallback={<Show when={isAttention()}><p class="effort-wizard-provider-reason">{friendlyProbeError(provider.error)}</p></Show>}>
        <div class="effort-wizard-tier-list">
          <For each={WIZARD_EFFORT_LEVELS}>
            {(level) => (
              <TierRow
                wizard={props.wizard}
                run={props.run}
                provider={provider.provider}
                level={level}
                current={provider.currentEfforts[level]}
                recommended={recommendation()?.confirmed[level] ?? undefined}
                disabled={props.disabled}
              />
            )}
          </For>
        </div>
      </Show>
    </article>
  );
}

function TierRow(props: {
  wizard: EffortWizardStore;
  run: EffortWizardRunState | null;
  provider: WizardProviderCode;
  level: EffortLevel;
  current: string[] | undefined;
  recommended: string[] | undefined;
  disabled: boolean;
}) {
  const confirmed = () => props.recommended !== undefined;
  const checked = () => props.run ? props.wizard.tierDecision(props.run.runId, props.provider, props.level) === "update" : false;
  return (
    <div class="effort-wizard-tier-row" classList={{ "is-unconfirmed": !confirmed() }}>
      <div class="effort-wizard-tier-copy">
        <strong>{props.level[0].toUpperCase() + props.level.slice(1)}</strong>
        <span class="effort-wizard-tier-transition">
          <span>{confirmed() ? formatCurrentToRecommended(props.provider, props.current, props.recommended) : `${formatEffortProfile(props.provider, props.current)} → No recommendation`}</span>
        </span>
        <Show when={!confirmed()}>
          <small>Not available for this account · Keep current</small>
        </Show>
      </div>
      <Show when={confirmed()} fallback={<span class="effort-wizard-keep-label">Keep current</span>}>
        <label class="effort-wizard-tier-checkbox">
          <input
            type="checkbox"
            checked={checked()}
            disabled={props.disabled}
            aria-label={`Update ${wizardProviderName(props.provider)} ${props.level} to recommended profile`}
            onChange={(event) => {
              if (props.run) props.wizard.setTierDecision(props.run.runId, props.provider, props.level, event.currentTarget.checked);
            }}
          />
          <span>{checked() ? "Update" : "Keep current"}</span>
        </label>
      </Show>
    </div>
  );
}

function CompletionScreen(props: { wizard: EffortWizardStore; run: EffortWizardRunState | null; onDone: () => void }) {
  const completion = createMemo(() => props.run?.completion);
  const statusFor = (provider: WizardProviderCode) => {
    const completionStatus = completion()?.providers.find((item) => item.provider === provider)?.status;
    if (completionStatus !== "updated_or_confirmed" || !props.run) return completionStatusLabel(completionStatus);
    const decisions = props.wizard.store.decisionDrafts[props.run.runId]?.[provider];
    return decisions && Object.values(decisions).every((decision) => decision === "keep_current") ? "Kept current" : "Updated / Reviewed";
  };
  return (
    <section class="effort-wizard-shell effort-wizard-complete" aria-labelledby="effort-wizard-complete-title">
      <div class="effort-wizard-complete-icon" aria-hidden="true">✓</div>
      <h2 id="effort-wizard-complete-title">Effort recommendations applied</h2>
      <p>{completion()?.confirmedProviderCount ?? 0} provider recommendations were reviewed and saved.</p>
      <div class="effort-wizard-completion-list">
        <For each={completion()?.providers ?? []}>
          {(provider) => <div class="effort-wizard-completion-row"><strong>{wizardProviderName(provider.provider)}</strong><span data-status={completionTone(provider.status)}>{statusFor(provider.provider)}</span></div>}
        </For>
      </div>
      <div class="effort-wizard-actions"><button type="button" class="effort-wizard-primary-button" onClick={props.onDone}>Done</button></div>
    </section>
  );
}

function completionStatusLabel(status: string | undefined): string {
  switch (status) {
    case "updated_or_confirmed": return "Updated / Reviewed";
    case "kept_current": return "Kept current";
    case "needs_attention": return "Needs attention";
    case "no_recommendation": return "No recommendation";
    case "unavailable": return "Unavailable";
    case "skipped": return "Skipped";
    default: return "Needs attention";
  }
}

function completionTone(status: string): string {
  return status === "updated_or_confirmed" ? "success" : status === "needs_attention" ? "attention" : "neutral";
}

function FatalScreen(props: {
  wizard: EffortWizardStore;
  run: EffortWizardRunState | null;
  onTryAgain: () => void;
  onBack: () => void;
}) {
  return (
    <section class="effort-wizard-shell effort-wizard-empty-state effort-wizard-fatal" aria-labelledby="effort-wizard-fatal-title">
      <div class="effort-wizard-empty-icon is-error" aria-hidden="true">!</div>
      <h2 id="effort-wizard-fatal-title">Wizard couldn't continue</h2>
      <p>Pedelec couldn't complete the overall provider check workflow.</p>
      <strong class="effort-wizard-safety-note">No effort settings were changed.</strong>
      <Show when={props.run?.error?.code || props.wizard.store.error}>
        <p class="effort-wizard-error-detail">The check could not be completed. Please try again.</p>
      </Show>
      <div class="effort-wizard-actions">
        <button type="button" class="effort-wizard-secondary-button" onClick={props.onBack}>Back to Settings</button>
        <button type="button" class="effort-wizard-primary-button" onClick={props.onTryAgain}>Try again</button>
      </div>
    </section>
  );
}

function buildApplyNote(
  wizard: EffortWizardStore,
  run: EffortWizardRunState | null,
  providers: Array<{ provider: WizardProviderCode; status: string; recommendation?: EffortWizardProviderRecommendation | null }>,
): string {
  if (!run) return "Review the recommendations before applying.";
  const reviewable = providers.filter((provider) => provider.status === "review_ready" && provider.recommendation);
  const updateProviders = reviewable.filter((provider) => WIZARD_EFFORT_LEVELS.some((level) => wizard.tierDecision(run.runId, provider.provider, level) === "update"));
  if (updateProviders.length > 0) {
    return `Applying updates only confirmed tiers for ${joinProviderNames(updateProviders.map((provider) => provider.provider))}.`;
  }
  if (reviewable.length > 0) return "Apply will confirm the recommendations you reviewed while keeping current tier values.";
  return "No provider recommendation is ready to apply.";
}

function joinProviderNames(providers: WizardProviderCode[]): string {
  const names = providers.map(wizardProviderName);
  if (names.length <= 1) return names[0] || "the selected providers";
  if (names.length === 2) return `${names[0]} and ${names[1]}`;
  return `${names.slice(0, -1).join(", ")}, and ${names[names.length - 1]}`;
}
