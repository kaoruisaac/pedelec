import { createMemo, createSignal, For, onMount, Show } from "solid-js";
import { FiChevronDown, FiX } from "solid-icons/fi";
import type { PedelecError, PedelecSettings, ProviderCode, ProviderInfo } from "@kaoruisaac/pedelec";
import { forwardPopUp } from "../services/PopUpProvider";
import "./SettingPop.css";

export type ShapeRainSessionSettings = {
  provider: "default" | ProviderCode;
  model: string;
};

export type PedelecProviderSettings = {
  providers: ProviderInfo[];
  settings: PedelecSettings;
};

type ProviderOption = {
  value: ShapeRainSessionSettings["provider"];
  label: string;
  defaultModel?: string;
};

export type SettingPopProps = {
  value: ShapeRainSessionSettings;
  loadProviderSettings: () => Promise<PedelecProviderSettings>;
  onApply?: (settings: ShapeRainSessionSettings) => void;
};

const SettingPop = forwardPopUp<SettingPopProps>((popup, props) => {
  const [provider, setProvider] = createSignal<ShapeRainSessionSettings["provider"]>(props.value.provider);
  const [model, setModel] = createSignal(props.value.model);
  const [providerMenuOpen, setProviderMenuOpen] = createSignal(false);
  const [providerSettings, setProviderSettings] = createSignal<PedelecProviderSettings | null>(null);
  const [loading, setLoading] = createSignal(true);
  const [error, setError] = createSignal("");

  const providerOptions = createMemo<ProviderOption[]>(() => {
    const loaded = providerSettings();
    const availableProviders = loaded?.providers.filter((item) => item.available) ?? [];
    return [
      { value: "default", label: "Default" },
      ...availableProviders.map((item) => ({
        value: item.code,
        label: item.name,
        defaultModel: loaded?.settings.defaultModels[item.code],
      })),
    ];
  });

  const selectedProvider = createMemo(() => providerOptions().find((option) => option.value === provider()) ?? providerOptions()[0]);
  const modelDisabled = createMemo(() => provider() === "default" || loading() || Boolean(error()));

  onMount(() => {
    void loadOptions();
  });

  async function loadOptions(): Promise<void> {
    setLoading(true);
    setError("");
    setProviderMenuOpen(false);
    try {
      const loaded = await props.loadProviderSettings();
      setProviderSettings(loaded);
      const available = new Set(loaded.providers.filter((item) => item.available).map((item) => item.code));
      const current = props.value.provider;
      if (current !== "default" && !available.has(current)) {
        setProvider("default");
        setModel("");
      } else {
        setProvider(current);
        setModel(current === "default" ? "" : props.value.model);
      }
    } catch (err) {
      setError(providerSettingsErrorMessage(err));
    } finally {
      setLoading(false);
    }
  }

  function selectProvider(value: ShapeRainSessionSettings["provider"]): void {
    setProvider(value);
    if (value === "default") {
      setModel("");
    }
    setProviderMenuOpen(false);
  }

  function handleApply(): void {
    if (loading() || error()) return;
    const selected = provider();
    props.onApply?.({
      provider: selected,
      model: selected === "default" ? "" : model().trim(),
    });
    popup.close();
  }

  return (
    <div class="SettingPop">
      <div class="SettingPop-header" ref={(el) => popup.setDraggableElement(el)}>
        <span class="SettingPop-title">Shape Rain Settings</span>
        <button type="button" class="SettingPop-closeBtn" title="Close" onClick={() => popup.close()}>
          <FiX size={16} />
        </button>
      </div>

      <div class="SettingPop-content">
        <div class="SettingPop-field">
          <label class="SettingPop-label">Provider</label>
          <div class="SettingPop-dropdown" classList={{ open: providerMenuOpen() }}>
            <button
              type="button"
              class="SettingPop-dropdownTrigger"
              disabled={loading() || Boolean(error())}
              onClick={() => setProviderMenuOpen((open) => !open)}
            >
              <span class="SettingPop-dropdownValue">
                {loading() ? "Loading providers..." : selectedProvider().label}
                <Show when={!loading() && selectedProvider().defaultModel}>
                  {(defaultModel) => <span class="SettingPop-defaultModel">default: {defaultModel()}</span>}
                </Show>
              </span>
              <FiChevronDown size={16} />
            </button>
            <Show when={providerMenuOpen()}>
              <div class="SettingPop-dropdownMenu">
                <For each={providerOptions()}>
                  {(option) => (
                    <button
                      type="button"
                      class="SettingPop-dropdownOption"
                      classList={{ selected: option.value === provider() }}
                      onClick={() => selectProvider(option.value)}
                    >
                      <span>{option.label}</span>
                      <Show when={option.defaultModel}>
                        {(defaultModel) => <span class="SettingPop-optionMeta">default: {defaultModel()}</span>}
                      </Show>
                    </button>
                  )}
                </For>
              </div>
            </Show>
          </div>
        </div>

        <div class="SettingPop-field">
          <label class="SettingPop-label">Model (optional)</label>
          <div class="SettingPop-inputWrap">
            <input
              class="SettingPop-input"
              type="text"
              value={model()}
              disabled={modelDisabled()}
              placeholder="e.g. llama3.2"
              onInput={(event) => setModel(event.currentTarget.value)}
            />
          </div>
          <Show when={provider() === "default"}>
            <p class="SettingPop-helpText">Default uses the Pedelec Desktop default provider and model.</p>
          </Show>
        </div>

        <Show when={error()}>
          {(message) => (
            <div class="SettingPop-error" role="alert">
              <span>{message()}</span>
              <button type="button" onClick={() => void loadOptions()}>
                Retry
              </button>
            </div>
          )}
        </Show>
      </div>

      <div class="SettingPop-footer">
        <button type="button" class="SettingPop-cancelBtn" onClick={() => popup.close()}>
          Cancel
        </button>
        <button type="button" class="SettingPop-applyBtn" disabled={loading() || Boolean(error())} onClick={handleApply}>
          Apply
        </button>
      </div>
    </div>
  );
});

function providerSettingsErrorMessage(err: unknown): string {
  const error = toPedelecError(err);
  return error.message || "Could not load Pedelec providers.";
}

function toPedelecError(err: unknown): PedelecError {
  if (!err) return { code: "UNKNOWN_ERROR", message: "Could not load Pedelec providers." };
  if (typeof err === "string") return { code: "UNKNOWN_ERROR", message: err };
  if (err instanceof Error) return { code: "UNKNOWN_ERROR", message: err.message };

  const value = err as Partial<PedelecError>;
  if (typeof value.code === "string" && typeof value.message === "string") {
    return { code: value.code, message: value.message, details: value.details };
  }

  return { code: "UNKNOWN_ERROR", message: "Could not load Pedelec providers.", details: err };
}

export default SettingPop;
