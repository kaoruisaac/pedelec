import { createMemo, createSignal, For, onMount, Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";

interface Provider {
    code: string;
    name: string;
    available: boolean;
    error?: string;
    description?: string;
  }
  
  interface Settings {
    defaultProvider: string | null;
    defaultModel: string | null;
  }

function SettingsPage() {
    const [settings, setSettings] = createSignal<Settings>({
      defaultProvider: null,
      defaultModel: null,
    });
    const [providers, setProviders] = createSignal<Provider[]>([]);
    const [selectedProvider, setSelectedProvider] = createSignal("");
    const [model, setModel] = createSignal("");
    const [loading, setLoading] = createSignal(true);
    const [saving, setSaving] = createSignal(false);
    const [error, setError] = createSignal("");
    const [savedMessage, setSavedMessage] = createSignal("");
  
    const selectedProviderInfo = createMemo(() =>
      providers().find((provider) => provider.code === selectedProvider()),
    );
    const savedProviderInfo = createMemo(() =>
      providers().find((provider) => provider.code === settings().defaultProvider),
    );
    const savedProviderUnavailable = createMemo(
      () => Boolean(settings().defaultProvider) && savedProviderInfo()?.available === false,
    );
    const canSave = createMemo(() => Boolean(selectedProviderInfo()?.available) && !saving());
  
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
        setSettings(nextSettings || { defaultProvider: null, defaultModel: null });
        setProviders(Array.isArray(nextProviders) ? nextProviders : []);
        setSelectedProvider(nextSettings?.defaultProvider || "");
        setModel(nextSettings?.defaultModel || "");
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
      if (!provider?.available) {
        setError("Choose an available provider before saving.");
        return;
      }
  
      setSaving(true);
      try {
        const nextSettings = await invoke<Settings>("update_settings", {
          input: {
            defaultProvider: provider.code,
            defaultModel: model().trim() || null,
          },
        });
        setSettings(nextSettings);
        setSelectedProvider(nextSettings.defaultProvider || "");
        setModel(nextSettings.defaultModel || "");
        setSavedMessage("Settings saved.");
      } catch (err) {
        setError(formatError(err));
      } finally {
        setSaving(false);
      }
    }
  
    return (
      <main class="settings-page">
        <header class="settings-header">
          <div>
            <h1>Settings</h1>
            <p>Choose the default provider and optional model used by SDK sessions.</p>
          </div>
          <button type="button" class="settings-secondary-button" onClick={loadSettings} disabled={loading()}>
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
                  <label
                    class="provider-option"
                    classList={{
                      "is-unavailable": !provider.available,
                      "is-selected": selectedProvider() === provider.code,
                    }}
                  >
                    <input
                      type="radio"
                      name="defaultProvider"
                      value={provider.code}
                      checked={selectedProvider() === provider.code}
                      disabled={!provider.available}
                      onChange={() => setSelectedProvider(provider.code)}
                    />
                    <span class="provider-main">
                      <strong>{provider.name}</strong>
                      <span>{provider.code}</span>
                    </span>
                    <span class="provider-status" data-status={provider.available ? "available" : "unavailable"}>
                      {provider.available ? "Available" : "Unavailable"}
                    </span>
                    <Show when={provider.error}>
                      <span class="provider-error">{provider.error}</span>
                    </Show>
                  </label>
                )}
              </For>
            </div>
          </section>
  
          <section class="settings-section">
            <label class="settings-field">
              <span>Default Model</span>
              <input
                type="text"
                value={model()}
                placeholder="Optional"
                onInput={(event) => setModel(event.currentTarget.value)}
              />
            </label>
          </section>
  
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

  function formatError(err: unknown): string {
    if (!err) return "Unknown error";
    if (typeof err === "string") return err;
    const e = err as { code?: string; message?: string };
    if (e.code && e.message) return `${e.code}: ${e.message}`;
    return e.message || JSON.stringify(err);
  }