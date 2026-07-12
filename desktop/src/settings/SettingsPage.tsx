import { createMemo, createSignal, For, onMount, Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { Provider, ProviderCode, ProviderSettings, Settings } from "./types";
import { DEFAULT_OLLAMA_BASE_URL, DEFAULT_OLLAMA_TIMEOUT_MS } from "./constants";
import { usePopUp } from "../services/PopUpProvider";
import EditingProviderPopup from "./EditingProviderPopup";

const emptySettings: Settings = {
  defaultProvider: null,
  defaultModels: {},
  providerSettings: {
    ollama: {
      baseUrl: DEFAULT_OLLAMA_BASE_URL,
      timeoutMs: DEFAULT_OLLAMA_TIMEOUT_MS,
      apiKey: "",
    },
  },
};

function SettingsPage() {
  const [settings, setSettings] = createSignal<Settings>(emptySettings);
  const [draftSettings, setDraftSettings] = createSignal<Settings>(emptySettings);
  const [providers, setProviders] = createSignal<Provider[]>([]);
  const [loading, setLoading] = createSignal(true);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal("");
  const [savedMessage, setSavedMessage] = createSignal("");
  const [hasUnsavedChanges, setHasUnsavedChanges] = createSignal(false);

  const { pop } = usePopUp();

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
    return provider.available === false;
  });
  const canSave = createMemo(() => {
    const provider = selectedProviderInfo();
    if (!provider || saving()) return false;
    if (provider.code === "ollama") return true;
    return Boolean(provider.available);
  });

  onMount(() => {
    loadSettings();
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
      const ollamaConnection = await checkOllamaConnection(
        normalizedSettings.providerSettings.ollama.baseUrl,
      );
      setSettings(normalizedSettings);
      setDraftSettings(cloneSettings(normalizedSettings));
      setProviders(withOllamaConnectionStatus(nextProviders, ollamaConnection));
      setHasUnsavedChanges(false);
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
    const providerSettings = draftSettings().providerSettings[provider.code as keyof ProviderSettings] || {};
    pop(
      EditingProviderPopup, {
        provider,
        editingBaseUrl: providerSettings?.baseUrl,
        editingApiKey: providerSettings?.apiKey,
        editingModel: draftSettings().defaultModels[provider.code],
        editingTimeoutMs: String(draftSettings().providerSettings.ollama.timeoutMs),
        onApply: ({ model, baseUrl, timeoutMs, apiKey }: { model: string; baseUrl?: string; timeoutMs?: number; apiKey?: string }) => {
          markDraftChanged();
          setDraftSettings((current) => ({ ...current, defaultModels: { ...current.defaultModels, [provider.code]: model } }));
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
    setLoading(true);
    setError("");
    try {
      const nextProviders = await invoke<Provider[]>("refresh_providers");
      const ollamaConnection = await checkOllamaConnection(draftSettings().providerSettings.ollama.baseUrl);
      setProviders(withOllamaConnectionStatus(nextProviders, ollamaConnection));
    } catch (err) {
      setError(formatError(err));
    } finally {
      setLoading(false);
    }
  }

  function canEditProvider(provider: Provider): boolean {
    if (loading() || saving()) return false;
    return provider.available || provider.code === "ollama";
  }

  function providerDefaultModel(provider: Provider): string {
    return draftSettings().defaultModels[provider.code] || "auto";
  }

  return (
    <main class="settings-page">
      <header class="settings-header">
        <div>
          <h1>Settings</h1>
          <p>Choose the default provider and optional model used by SDK sessions.</p>
        </div>
        <button type="button" class="settings-secondary-button" onClick={refreshProviders} disabled={loading()}>
          Refresh
        </button>
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
            <span>{loading() ? "Loading..." : `${providers().length} providers`}</span>
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
                  <span class="provider-main">
                    <strong>{provider.name}</strong>
                    <Show when={provider.version}>
                      <span>version: {provider.version}</span>
                    </Show>
                    <span>default model: {providerDefaultModel(provider)}</span>
                  </span>
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
  return provider.available ? "available" : "unavailable";
}

function providerStatusLabel(provider: Provider): string {
  if (provider.code === "ollama") {
    return provider.connectionStatus === "connected" ? "Connected" : "Disconnected";
  }
  return provider.available ? "Available" : "Unavailable";
}

function isProviderSelectable(provider: Provider): boolean {
  return provider.code === "ollama" || provider.available;
}

function formatError(err: unknown): string {
  if (!err) return "Unknown error";
  if (typeof err === "string") return err;
  const e = err as { code?: string; message?: string };
  if (e.code && e.message) return `${e.code}: ${e.message}`;
  return e.message || JSON.stringify(err);
}

function normalizeSettings(value: Settings | null | undefined): Settings {
  return {
    defaultProvider: value?.defaultProvider ?? null,
    defaultModels: { ...(value?.defaultModels ?? {}) },
    providerSettings: {
      ollama: {
        baseUrl: value?.providerSettings?.ollama?.baseUrl ?? DEFAULT_OLLAMA_BASE_URL,
        timeoutMs: value?.providerSettings?.ollama?.timeoutMs ?? DEFAULT_OLLAMA_TIMEOUT_MS,
        apiKey: value?.providerSettings?.ollama?.apiKey ?? "",
      },
    },
  };
}

function cloneSettings(settings: Settings): Settings {
  return {
    defaultProvider: settings.defaultProvider,
    defaultModels: { ...settings.defaultModels },
    providerSettings: {
      ollama: { ...settings.providerSettings.ollama },
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
