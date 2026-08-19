import { createEffect, createMemo, createSignal, For, onMount, Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { EffortsArgs, Provider, ProviderCode, ProviderSettings, Settings } from "./types";
import { DEFAULT_OLLAMA_BASE_URL, DEFAULT_OLLAMA_TIMEOUT_MS } from "./constants";
import { usePopUp } from "../services/PopUpProvider";
import EditingProviderPopup from "./EditingProviderPopup";
import MissingProviderPopup from "./MissingProviderPopup";
import {
  buildAutomaticDefaultProviderSettings,
  findFirstAvailableCliProvider,
} from "./providerInitialization";
import { canSaveSettings } from "./settingsValidation";
import { cloneEffortsArgs, configuredEffortLevels, emptyEffortsArgs, EFFORT_LEVELS, effortLevelLabel } from "./effortSettings";
import { OcTerminal2 } from "solid-icons/oc";
import ExistingSettingsWarningPopup from "../effort-wizard/ExistingSettingsWarningPopup";
import UnsavedSettingsWizardPopup from "../effort-wizard/UnsavedSettingsWizardPopup";
import type { EffortWizardEntryOrigin } from "../effort-wizard/types";
import type { EffortWizardStore } from "../effort-wizard/effortWizardStore";
import {
  isProviderInstallerSupported,
  openProviderInstaller as openProviderInstallerCommand,
  restartPedelec as restartPedelecCommand,
  type OnboardingInstallerCode,
  type ProviderInstallerCode,
} from "./providerInstaller";

const emptySettings: Settings = {
  defaultProvider: null,
  providerSettings: {
    codex: { effortsArgs: emptyEffortsArgs() },
    antigravity: { effortsArgs: emptyEffortsArgs() },
    opencode: { effortsArgs: emptyEffortsArgs() },
    cursor: { effortsArgs: emptyEffortsArgs() },
    claude: { effortsArgs: emptyEffortsArgs() },
    ollama: {
      baseUrl: DEFAULT_OLLAMA_BASE_URL,
      timeoutMs: DEFAULT_OLLAMA_TIMEOUT_MS,
      apiKey: "",
      tavilyApiKey: "",
      effortsArgs: emptyEffortsArgs(),
    },
  },
};

interface SettingsPageProps {
  onNavigateToSettings: () => void;
  effortWizard?: EffortWizardStore;
  onOpenEffortWizard?: (origin: EffortWizardEntryOrigin) => void;
  onProvidersRefreshed?: () => Promise<void>;
}

function SettingsPage(props: SettingsPageProps) {
  const [settings, setSettings] = createSignal<Settings>(emptySettings);
  const [draftSettings, setDraftSettings] = createSignal<Settings>(emptySettings);
  const [providers, setProviders] = createSignal<Provider[]>([]);
  const [loading, setLoading] = createSignal(true);
  const [refreshingProviders, setRefreshingProviders] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal("");
  const [savedMessage, setSavedMessage] = createSignal("");
  const [hasUnsavedChanges, setHasUnsavedChanges] = createSignal(false);
  const [launchingInstaller, setLaunchingInstaller] = createSignal<ProviderCode | null>(null);
  const [launchingTerminal, setLaunchingTerminal] = createSignal<ProviderCode | null>(null);
  const [installerOpenedProviders, setInstallerOpenedProviders] = createSignal<Set<ProviderCode>>(new Set());
  const [restarting, setRestarting] = createSignal(false);
  const [missingProviderPopupShown, setMissingProviderPopupShown] = createSignal(false);

  const { pop } = usePopUp();
  let settingsLoaded = false;
  let lastSettingsRefreshRevision = props.effortWizard?.store.settingsRefreshRevision ?? 0;

  const selectedProviderInfo = createMemo(() =>
    providers().find((provider) => provider.code === draftSettings().defaultProvider),
  );
  const savedProviderInfo = createMemo(() =>
    providers().find((provider) => provider.code === settings().defaultProvider),
  );
  const savedProviderUnavailable = createMemo(() => {
    const provider = savedProviderInfo();
    if (!settings().defaultProvider || !provider) return false;
    if (provider.code === "ollama") return false;
    return provider.scanned && provider.available === false;
  });
  const canSave = createMemo(() => {
    return canSaveSettings(draftSettings(), selectedProviderInfo(), saving());
  });

  onMount(() => {
    void loadSettings().finally(() => {
      settingsLoaded = true;
      lastSettingsRefreshRevision = props.effortWizard?.store.settingsRefreshRevision ?? lastSettingsRefreshRevision;
    });
  });

  createEffect(() => {
    const revision = props.effortWizard?.store.settingsRefreshRevision;
    if (!settingsLoaded || revision === undefined || revision === lastSettingsRefreshRevision) return;
    lastSettingsRefreshRevision = revision;
    void loadSettings();
  });

  async function loadSettings(): Promise<void> {
    setLoading(true);
    setError("");
    setSavedMessage("");
    try {
      const [nextSettings, nextProviders] = await Promise.all([
        invoke<Settings>("get_settings"),
        invoke<Provider[]>("list_providers"),
      ]);
      const normalizedSettings = normalizeSettings(nextSettings);
      setSettings(normalizedSettings);
      setDraftSettings(cloneSettings(normalizedSettings));
      setHasUnsavedChanges(false);

      const ollamaConnection = normalizedSettings.defaultProvider === null
        ? "disconnected"
        : await checkOllamaConnection(
          normalizedSettings.providerSettings.ollama.baseUrl,
        );
      const providersWithConnectionStatus = withOllamaConnectionStatus(
        nextProviders,
        ollamaConnection,
      );
      setProviders(providersWithConnectionStatus);

      if (normalizedSettings.defaultProvider === null) {
        await initializeDefaultProviderIfNeeded(normalizedSettings, providersWithConnectionStatus);
        return;
      }
    } catch (err) {
      setError(formatError(err));
    } finally {
      setLoading(false);
    }
  }

  async function saveSettings(event: Event): Promise<void> {
    event.preventDefault();
    setError("");
    setSavedMessage("");

    const provider = selectedProviderInfo();
    if (!provider) {
      setError("Choose a provider before saving.");
      return;
    }
    if (provider.code !== "ollama" && !provider.available) {
      setError("Choose an available provider before saving.");
      return;
    }
    if (provider.code === "ollama" && !canSaveSettings(draftSettings(), provider)) {
      setError("Ollama API key and a model are required before saving.");
      return;
    }

    setSaving(true);
    try {
      const nextSettings = await invoke<Settings>("update_settings", {
        input: cloneSettings({
          ...draftSettings(),
          defaultProvider: provider.code,
        }),
      });
      const normalizedSettings = normalizeSettings(nextSettings);
      setSettings(normalizedSettings);
      setDraftSettings(cloneSettings(normalizedSettings));
      setHasUnsavedChanges(false);
      setSavedMessage("Settings saved.");
    } catch (err) {
      setError(formatError(err));
    } finally {
      setSaving(false);
    }
  }

  function markDraftChanged(): void {
    setHasUnsavedChanges(true);
    setSavedMessage("");
  }

  function setDraftProvider(provider: ProviderCode): void {
    markDraftChanged();
    setDraftSettings((current) => ({ ...current, defaultProvider: provider }));
  }

  function openEditor(provider: Provider): void {
    if (!canEditProvider(provider)) return;
    const currentSettings = draftSettings();
    const ollamaSettings = currentSettings.providerSettings.ollama;
    const providerSettings = provider.code === "ollama"
      ? ollamaSettings
      : currentSettings.providerSettings[provider.code];
    pop(
      EditingProviderPopup, {
        provider,
        isDefaultProvider: draftSettings().defaultProvider === provider.code,
        editingEffortsArgs: cloneEffortsArgs(providerSettings.effortsArgs),
        editingBaseUrl: provider.code === "ollama" ? ollamaSettings.baseUrl : undefined,
        editingApiKey: provider.code === "ollama" ? ollamaSettings.apiKey : undefined,
        editingTavilyApiKey: provider.code === "ollama" ? ollamaSettings.tavilyApiKey : undefined,
        editingTimeoutMs: provider.code === "ollama" ? String(ollamaSettings.timeoutMs) : undefined,
        onApply: ({ effortsArgs, baseUrl, timeoutMs, apiKey, tavilyApiKey }: { effortsArgs: EffortsArgs; baseUrl?: string; timeoutMs?: number; apiKey?: string; tavilyApiKey?: string }) => {
          markDraftChanged();
          setDraftSettings((current) => ({
            ...current,
            providerSettings: {
              ...current.providerSettings,
              [provider.code]: {
                ...current.providerSettings[provider.code],
                effortsArgs: cloneEffortsArgs(effortsArgs),
              },
            },
          }));
          if (provider.code === "ollama") {
            const nextBaseUrl = baseUrl ?? DEFAULT_OLLAMA_BASE_URL;
            setDraftSettings((current) => ({
              ...current,
              providerSettings: {
                ...current.providerSettings,
                ollama: {
                  baseUrl: nextBaseUrl,
                  timeoutMs: timeoutMs ?? DEFAULT_OLLAMA_TIMEOUT_MS,
                  apiKey: apiKey ?? "",
                  tavilyApiKey: tavilyApiKey ?? "",
                  effortsArgs: cloneEffortsArgs(effortsArgs),
                },
              },
            }));
            void refreshOllamaConnectionStatus(nextBaseUrl);
          }
        }
      }
    );
  }

  async function refreshOllamaConnectionStatus(baseUrl: string): Promise<void> {
    const connectionStatus = await checkOllamaConnection(baseUrl);
    setProviders((current) => withOllamaConnectionStatus(current, connectionStatus));
  }

  async function refreshProviders(): Promise<void> {
    if (refreshingProviders()) return;
    setRefreshingProviders(true);
    setError("");
    try {
      await fetchRefreshedProviders({ checkOllamaConnection: true });
      await props.onProvidersRefreshed?.();
    } catch (err) {
      setError(formatError(err));
    } finally {
      setRefreshingProviders(false);
    }
  }

  async function fetchRefreshedProviders(options: { checkOllamaConnection: boolean }): Promise<Provider[]> {
    const nextProviders = await invoke<Provider[]>("refresh_providers");
    const ollamaConnection = options.checkOllamaConnection
      ? await checkOllamaConnection(draftSettings().providerSettings.ollama.baseUrl)
      : "disconnected";
    const providersWithConnectionStatus = withOllamaConnectionStatus(nextProviders, ollamaConnection);
    setProviders(providersWithConnectionStatus);
    return providersWithConnectionStatus;
  }

  async function initializeDefaultProviderIfNeeded(
    initialSettings: Settings,
    refreshedProviders: Provider[],
  ): Promise<void> {
    const provider = findFirstAvailableCliProvider(refreshedProviders);
    if (!provider) {
      if (!missingProviderPopupShown()) {
        setMissingProviderPopupShown(true);
        pop(MissingProviderPopup, {
          onGoToSettings: props.onNavigateToSettings,
          onOpenProviderInstaller: openOnboardingProviderInstaller,
          onRestart: restartPedelecCommand,
        }, {
          background: true,
          closeOnBackground: false,
        });
      }
      return;
    }

    if (
      settings().defaultProvider !== null ||
      draftSettings().defaultProvider !== null ||
      hasUnsavedChanges()
    ) {
      return;
    }

    const savedSettings = await invoke<Settings>("update_settings", {
      input: buildAutomaticDefaultProviderSettings(initialSettings, provider.code),
    });
    const normalizedSettings = normalizeSettings(savedSettings);
    setSettings(normalizedSettings);
    setDraftSettings(cloneSettings(normalizedSettings));
    setHasUnsavedChanges(false);
  }

  function canEditProvider(provider: Provider): boolean {
    if (loading() || refreshingProviders() || saving()) return false;
    return provider.available || provider.code === "ollama";
  }

  function providerEffortLevels(provider: Provider): ReturnType<typeof configuredEffortLevels> {
    const efforts = draftSettings().providerSettings[provider.code].effortsArgs;
    return configuredEffortLevels(efforts);
  }

  function canInstallProvider(provider: Provider): provider is Provider & { code: ProviderInstallerCode } {
    return provider.scanned && !provider.available && isProviderInstallerSupported(provider.code);
  }

  function canOpenProviderTerminal(provider: Provider): boolean {
    if (loading() || refreshingProviders() || saving() || launchingTerminal() !== null) return false;
    return provider.code !== "ollama" && provider.scanned && provider.available;
  }

  async function openProviderTerminal(provider: Provider): Promise<void> {
    if (!canOpenProviderTerminal(provider)) return;
    setError("");
    setLaunchingTerminal(provider.code);
    try {
      await invoke("open_provider_terminal", { input: { provider: provider.code } });
    } catch (err) {
      setError(formatError(err));
    } finally {
      setLaunchingTerminal(null);
    }
  }

  async function openProviderInstaller(provider: Provider): Promise<void> {
    if (!canInstallProvider(provider)) return;
    setError("");
    setLaunchingInstaller(provider.code);
    try {
      await openProviderInstallerCommand(provider.code);
      setInstallerOpenedProviders((current) => new Set(current).add(provider.code));
    } catch (err) {
      setError(formatError(err));
    } finally {
      setLaunchingInstaller(null);
    }
  }

  async function restartPedelec(): Promise<void> {
    setRestarting(true);
    try { await restartPedelecCommand(); } catch (err) { setError(formatError(err)); setRestarting(false); }
  }

  function openOnboardingProviderInstaller(provider: OnboardingInstallerCode): Promise<void> {
    return openProviderInstallerCommand(provider);
  }

  function discardDraftAndContinue(): void {
    setDraftSettings(cloneSettings(settings()));
    setHasUnsavedChanges(false);
    setSavedMessage("");
    openEffortWizardAfterSafety();
  }

  function openEffortWizardAfterSafety(): void {
    if (!props.onOpenEffortWizard) return;
    if (props.effortWizard?.hasResumableRun()) {
      props.onOpenEffortWizard("settings");
      return;
    }
    if (!hasExistingWizardEffortSettings(draftSettings())) {
      props.onOpenEffortWizard("settings");
      return;
    }
    pop(ExistingSettingsWarningPopup, {
      onContinue: () => props.onOpenEffortWizard?.("settings"),
    });
  }

  function requestEffortWizard(): void {
    if (!props.onOpenEffortWizard) return;
    if (hasUnsavedChanges()) {
      pop(UnsavedSettingsWizardPopup, { onDiscardAndContinue: discardDraftAndContinue });
      return;
    }
    openEffortWizardAfterSafety();
  }

  return (
    <main class="settings-page">
      <header class="settings-header">
        <div>
          <h1>Settings</h1>
          <p>Choose the default provider and optional effort profiles used by SDK sessions.</p>
        </div>
        <div class="settings-header-actions">
          <Show when={props.onOpenEffortWizard}>
            <button type="button" class="settings-secondary-button settings-effort-wizard-button" onClick={requestEffortWizard} disabled={loading() || refreshingProviders()}>
              Effort Wizard
            </button>
          </Show>
          <button type="button" class="settings-secondary-button" onClick={refreshProviders} disabled={loading() || refreshingProviders()}>
            {refreshingProviders() ? "Refreshing..." : "Refresh"}
          </button>
        </div>
      </header>
      <Show when={error()}>
        <div class="settings-alert is-error">{error()}</div>
      </Show>
      <Show when={savedMessage()}>
        <div class="settings-alert is-success">{savedMessage()}</div>
      </Show>
      <Show when={savedProviderUnavailable()}>
        <div class="settings-alert is-warning">
          Saved default provider "{settings().defaultProvider}" is currently unavailable. Choose an available provider before saving.
        </div>
      </Show>

      <form class="settings-panel" onSubmit={saveSettings}>
        <section class="settings-section">
          <div class="settings-section-heading">
            <h2>Default Provider</h2>
            <span>{loading() ? "Loading..." : refreshingProviders() ? "Refreshing providers..." : `${providers().length} providers`}</span>
          </div>

          <div class="provider-list">
            <For each={providers()}>
              {(provider) => (
                <div
                  class="provider-option"
                  classList={{
                    "is-unavailable": provider.code !== "ollama" && !provider.available,
                    "is-selected": draftSettings().defaultProvider === provider.code,
                  }}
                >
                  <label class="provider-radio">
                    <input
                      type="radio"
                      name="defaultProvider"
                      value={provider.code}
                      checked={draftSettings().defaultProvider === provider.code}
                      disabled={!isProviderSelectable(provider)}
                      onChange={() => setDraftProvider(provider.code)}
                    />
                  </label>
                  <div class="provider-main">
                    <div class="provider-name">
                      <strong>{provider.name}</strong>
                      <Show when={provider.code !== "ollama"}>
                        <button
                          type="button"
                          class="provider-terminal-button"
                          aria-label={`Open ${provider.name} CLI in Terminal`}
                          title={`Open ${provider.name} CLI in Terminal`}
                          disabled={!canOpenProviderTerminal(provider)}
                          onClick={(event) => { event.stopPropagation(); void openProviderTerminal(provider); }}
                        >
                          <OcTerminal2 aria-hidden="true" />
                        </button>
                      </Show>
                    </div>
                    <Show when={provider.version}>
                      <span>version: {provider.version}</span>
                    </Show>
                    <Show
                      when={canInstallProvider(provider)}
                      fallback={<>
                        <Show when={providerEffortLevels(provider).length > 0}>
                          <span class="provider-effort-summary">effort: {providerEffortLevels(provider).map((level) => <span class="settings-effort-badge">{effortLevelLabel(level)} ✓</span>)}</span>
                        </Show>
                      </>}
                    >
                      <Show
                        when={installerOpenedProviders().has(provider.code)}
                        fallback={
                          <button
                            type="button"
                            class="provider-install-link"
                            disabled={loading() || refreshingProviders() || saving() || launchingInstaller() === provider.code}
                            onClick={() => void openProviderInstaller(provider)}
                          >
                            {launchingInstaller() === provider.code ? `Opening install ${provider.code}...` : `install ${provider.code}`}
                          </button>
                        }
                      >
                        <span>
                          {provider.code === "codex"
                            ? "Installation opened in Terminal. Complete the installation and sign-in flow, then restart Pedelec."
                            : provider.code === "claude"
                              ? "Installation opened in Terminal. Complete the installation and Claude sign-in flow, then restart Pedelec."
                            : provider.code === "antigravity"
                              ? "Installation opened in Terminal. Complete the Antigravity sign-in and onboarding flow, then restart Pedelec."
                              : provider.code === "cursor"
                                ? "Installation opened in Terminal. Complete the installation and Cursor sign-in flow, then restart Pedelec."
                              : "Installation opened in Terminal. Complete the installation, then restart Pedelec."}
                        </span>
                        <button type="button" class="provider-restart-button" disabled={restarting()} onClick={() => void restartPedelec()}>
                          {restarting() ? "Restarting..." : "Restart Pedelec"}
                        </button>
                      </Show>
                    </Show>
                  </div>
                  <span class="provider-status" data-status={providerStatusValue(provider)}>
                    {providerStatusLabel(provider)}
                  </span>
                  <button
                    type="button"
                    class="provider-edit-button"
                    aria-label={`Edit ${provider.name} settings`}
                    disabled={!canEditProvider(provider)}
                    onClick={() => openEditor(provider)}
                  >
                    Edit
                  </button>
                  <Show when={provider.code !== "ollama" ? provider.error : null}>
                    <span class="provider-error">{provider.error}</span>
                  </Show>
                </div>
              )}
            </For>
          </div>
        </section>

        <Show when={props.onOpenEffortWizard}>
          <section class="settings-effort-wizard-card">
            <div>
              <h2>Effort Setting Wizard</h2>
              <p>Check available providers against Pedelec-maintained recommendations, then review before applying.</p>
            </div>
            <button type="button" class="settings-secondary-button" onClick={requestEffortWizard} disabled={loading() || refreshingProviders()}>
              Check recommendations
            </button>
          </section>
        </Show>

        <Show when={hasUnsavedChanges()}>
          <div class="settings-alert is-warning">
            You have unsaved changes. Click Save to apply your settings.
          </div>
        </Show>

        <footer class="settings-actions">
          <button type="submit" class="settings-primary-button" disabled={!canSave()}>
            {saving() ? "Saving..." : "Save"}
          </button>
        </footer>
      </form>
    </main>
  );
}

export default SettingsPage;

async function checkOllamaConnection(baseUrl: string): Promise<"connected" | "disconnected"> {
  try {
    const result = await invoke<{ connected: boolean }>("check_ollama_connection", {
      input: { baseUrl },
    });
    return result.connected ? "connected" : "disconnected";
  } catch {
    return "disconnected";
  }
}

function withOllamaConnectionStatus(
  providers: Provider[],
  connectionStatus: "connected" | "disconnected",
): Provider[] {
  if (!Array.isArray(providers)) return [];
  return providers.map((provider) =>
    provider.code === "ollama" ? { ...provider, connectionStatus } : provider,
  );
}

function providerStatusValue(provider: Provider): string {
  if (provider.code === "ollama") {
    return provider.connectionStatus === "connected" ? "connected" : "disconnected";
  }
  if (!provider.scanned) return "scanning";
  return provider.available ? "available" : "unavailable";
}

function providerStatusLabel(provider: Provider): string {
  if (provider.code === "ollama") {
    return provider.connectionStatus === "connected" ? "Connected" : "Disconnected";
  }
  if (!provider.scanned) return "Scanning...";
  return provider.available ? "Available" : "Unavailable";
}

function isProviderSelectable(provider: Provider): boolean {
  return provider.code === "ollama" || provider.available;
}

function hasExistingWizardEffortSettings(settings: Settings): boolean {
  return (["codex", "claude", "cursor", "antigravity"] as const).some((provider) => {
    const efforts = settings.providerSettings[provider].effortsArgs;
    return (["low", "default", "high"] as const).some((level) => efforts[level].length > 0);
  });
}

function formatError(err: unknown): string {
  if (!err) return "Unknown error";
  if (typeof err === "string") return err;
  const e = err as { code?: string; message?: string };
  if (e.code && e.message) return `${e.code}: ${e.message}`;
  return e.message || JSON.stringify(err);
}

function normalizeSettings(value: Settings | null | undefined): Settings {
  const common = (provider: keyof Omit<ProviderSettings, "ollama">) => ({
    effortsArgs: {
      default: [...(value?.providerSettings?.[provider]?.effortsArgs?.default ?? [])],
      low: [...(value?.providerSettings?.[provider]?.effortsArgs?.low ?? [])],
      high: [...(value?.providerSettings?.[provider]?.effortsArgs?.high ?? [])],
    },
  });
  const ollama = value?.providerSettings?.ollama;
  return {
    defaultProvider: value?.defaultProvider ?? null,
    providerSettings: {
      codex: common("codex"),
      antigravity: common("antigravity"),
      opencode: common("opencode"),
      cursor: common("cursor"),
      claude: common("claude"),
      ollama: {
        baseUrl: ollama?.baseUrl ?? DEFAULT_OLLAMA_BASE_URL,
        timeoutMs: ollama?.timeoutMs ?? DEFAULT_OLLAMA_TIMEOUT_MS,
        apiKey: ollama?.apiKey ?? "",
        tavilyApiKey: ollama?.tavilyApiKey ?? "",
        effortsArgs: {
          default: [...(ollama?.effortsArgs?.default ?? [])],
          low: [...(ollama?.effortsArgs?.low ?? [])],
          high: [...(ollama?.effortsArgs?.high ?? [])],
        },
      },
    },
  };
}

function cloneSettings(settings: Settings): Settings {
  return {
    defaultProvider: settings.defaultProvider,
    providerSettings: {
      codex: { effortsArgs: cloneEffortsArgs(settings.providerSettings.codex.effortsArgs) },
      antigravity: { effortsArgs: cloneEffortsArgs(settings.providerSettings.antigravity.effortsArgs) },
      opencode: { effortsArgs: cloneEffortsArgs(settings.providerSettings.opencode.effortsArgs) },
      cursor: { effortsArgs: cloneEffortsArgs(settings.providerSettings.cursor.effortsArgs) },
      claude: { effortsArgs: cloneEffortsArgs(settings.providerSettings.claude.effortsArgs) },
      ollama: { ...settings.providerSettings.ollama, effortsArgs: cloneEffortsArgs(settings.providerSettings.ollama.effortsArgs) },
    },
  };
}

function parseOptionalTimeout(value: string): { ok: true; value?: number } | { ok: false; error: string } {
  const trimmed = value.trim();
  if (!trimmed) return { ok: true, value: DEFAULT_OLLAMA_TIMEOUT_MS };
  if (!/^[0-9]+$/.test(trimmed)) {
    return { ok: false, error: "Timeout must be a positive integer." };
  }
  const parsed = Number(trimmed);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) {
    return { ok: false, error: "Timeout must be a positive integer." };
  }
  return { ok: true, value: parsed };
}
