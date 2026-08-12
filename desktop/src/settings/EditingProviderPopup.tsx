import { createMemo, createSignal, For, onMount, Show } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { forwardPopUp } from "../services/PopUpProvider";
import TavilyInfoDialog from "./TavilyInfoDialog";
import { EffortLevel, EffortsArgs, OllamaModelOption, Provider } from "./types";
import {
  cloneEffortsArgs,
  effortArgsToText,
  effortLevelLabel,
  effortSettingsPlaceholder,
  effortTextToArgs,
  EFFORT_LEVELS,
  ollamaModelsToEffortsArgs,
  validateOllamaDefaultModelSelection,
  validateEffortArgs,
} from "./effortSettings";
import { DEFAULT_OLLAMA_BASE_URL, DEFAULT_OLLAMA_TIMEOUT_MS } from "./constants";

interface EditingProviderPopupProps {
  provider: Provider;
  isDefaultProvider: boolean;
  editingEffortsArgs: EffortsArgs;
  editingBaseUrl?: string;
  editingTimeoutMs?: string;
  editingApiKey?: string;
  editingTavilyApiKey?: string;
  onApply: (payload: {
    effortsArgs: EffortsArgs;
    baseUrl?: string;
    timeoutMs?: number;
    apiKey?: string;
    tavilyApiKey?: string;
  }) => void;
}

const EditingProviderPopup = forwardPopUp((popup, props: EditingProviderPopupProps) => {
  const [selectedLevel, setSelectedLevel] = createSignal<EffortLevel>("default");
  const [editingTexts, setEditingTexts] = createSignal<Record<EffortLevel, string>>(
    effortTextsFromArgs(props.editingEffortsArgs),
  );
  const [editingModels, setEditingModels] = createSignal<Record<EffortLevel, string>>(
    modelsFromArgs(props.editingEffortsArgs),
  );
  const [editingBaseUrl, setEditingBaseUrl] = createSignal(props.editingBaseUrl ?? "");
  const [editingTimeoutMs, setEditingTimeoutMs] = createSignal(props.editingTimeoutMs ?? "");
  const [editingApiKey, setEditingApiKey] = createSignal(props.editingApiKey ?? "");
  const [editingTavilyApiKey, setEditingTavilyApiKey] = createSignal(props.editingTavilyApiKey ?? "");
  const [ollamaModelsLoading, setOllamaModelsLoading] = createSignal(false);
  const [ollamaModelsError, setOllamaModelsError] = createSignal("");
  const [ollamaModels, setOllamaModels] = createSignal<OllamaModelOption[]>([]);
  const [fieldError, setFieldError] = createSignal("");
  const [showTavilyInfo, setShowTavilyInfo] = createSignal(false);
  let learnMoreButton: HTMLButtonElement | undefined;

  const canApplyOllama = createMemo(() => {
    if (ollamaModelsLoading() || ollamaModelsError() || fieldError()) return false;
    const selections = editingModels();
    const defaultIsValid = !props.isDefaultProvider || isKnownModel(selections.default);
    return defaultIsValid && EFFORT_LEVELS.every((level) =>
      !selections[level] || isKnownModel(selections[level]),
    );
  });

  function isKnownModel(value: string): boolean {
    return Boolean(value) && ollamaModels().some((model) => model.value === value);
  }

  async function loadOllamaModels(
    baseUrlValue = editingBaseUrl(),
    apiKeyValue = editingApiKey(),
    timeoutValue = editingTimeoutMs(),
  ): Promise<void> {
    setFieldError("");
    setOllamaModelsError("");
    setOllamaModels([]);

    const baseUrl = normalizeBaseUrlInput(baseUrlValue);
    if (!baseUrl.ok) {
      setFieldError(baseUrl.error);
      return;
    }
    const timeout = parseOptionalTimeout(timeoutValue);
    if (!timeout.ok) {
      setFieldError(timeout.error);
      return;
    }
    const apiKey = normalizeApiKeyInput(apiKeyValue);
    if (!apiKey.ok) {
      setFieldError(apiKey.error);
      return;
    }

    setOllamaModelsLoading(true);
    try {
      const models = await invoke<OllamaModelOption[]>("list_ollama_models", {
        input: { baseUrl: baseUrl.value, apiKey: apiKey.value, timeoutMs: timeout.value },
      });
      const nextModels = Array.isArray(models) ? models : [];
      setOllamaModels(nextModels);
      setEditingModels((current) => ({
        default: isModelInList(current.default, nextModels) ? current.default : "",
        low: isModelInList(current.low, nextModels) ? current.low : "",
        high: isModelInList(current.high, nextModels) ? current.high : "",
      }));
    } catch (err) {
      setOllamaModels([]);
      setOllamaModelsError(formatError(err));
      setEditingModels((current) => ({ default: "", low: "", high: "" }));
    } finally {
      setOllamaModelsLoading(false);
    }
  }

  function applyEditor(event: Event): void {
    event.preventDefault();
    setFieldError("");
    if (props.provider.code === "ollama") {
      applyOllamaEditor();
      return;
    }

    const efforts = cloneEffortsArgs(props.editingEffortsArgs);
    for (const level of EFFORT_LEVELS) {
      try {
        efforts[level] = effortTextToArgs(editingTexts()[level]);
      } catch (err) {
        setFieldError(`${effortLevelLabel(level)}: ${formatError(err)}`);
        return;
      }
      const validationError = validateEffortArgs(props.provider.code, efforts[level]);
      if (validationError) {
        setFieldError(`${effortLevelLabel(level)}: ${validationError}`);
        return;
      }
    }
    props.onApply({ effortsArgs: efforts });
    popup.close();
  }

  function applyOllamaEditor(): void {
    if (ollamaModelsLoading() || ollamaModelsError() || !canApplyOllama()) return;
    const baseUrl = normalizeBaseUrlInput(editingBaseUrl());
    if (!baseUrl.ok) {
      setFieldError(baseUrl.error);
      return;
    }
    const timeout = parseOptionalTimeout(editingTimeoutMs());
    if (!timeout.ok) {
      setFieldError(timeout.error);
      return;
    }
    const apiKey = normalizeApiKeyInput(editingApiKey());
    if (!apiKey.ok) {
      setFieldError(apiKey.error);
      return;
    }

    const selections = editingModels();
    const modelError = validateOllamaDefaultModelSelection(selections, props.isDefaultProvider);
    if (modelError) {
      setFieldError(modelError);
      return;
    }
    const efforts = ollamaModelsToEffortsArgs(selections);
    props.onApply({
      effortsArgs: efforts,
      baseUrl: baseUrl.value,
      timeoutMs: timeout.value,
      apiKey: apiKey.value,
      tavilyApiKey: editingTavilyApiKey().trim(),
    });
    popup.close();
  }

  onMount(() => {
    if (props.provider.code === "ollama") void loadOllamaModels();
  });

  return (
    <Show when={!showTavilyInfo()} fallback={<TavilyInfoDialog onClose={() => { setShowTavilyInfo(false); queueMicrotask(() => learnMoreButton?.focus()); }} />}>
      <form class="settings-modal" role="dialog" aria-modal="true" onSubmit={applyEditor}>
        <header class="settings-modal-header">
          <div>
            <h2>{props.provider.name}</h2>
            <p>Edit provider settings</p>
          </div>
        </header>

        <Show when={props.provider.code === "ollama"}>
          <div class="settings-modal-fields">
            <label class="settings-field">
              <span>Base URL <em>Optional</em></span>
              <input type="text" value={editingBaseUrl()} placeholder={DEFAULT_OLLAMA_BASE_URL} onInput={(event) => setEditingBaseUrl(event.currentTarget.value)} onBlur={() => void loadOllamaModels()} autofocus />
            </label>
            <label class="settings-field">
              <span>API Key <em>Required</em></span>
              <input type="password" value={editingApiKey()} placeholder="If it is a local server, type in: 'ollama'" onInput={(event) => setEditingApiKey(event.currentTarget.value)} onBlur={() => void loadOllamaModels()} />
            </label>
            <label class="settings-field">
              <span>Timeout Milliseconds <em>Optional</em></span>
              <input type="text" inputmode="numeric" value={editingTimeoutMs()} placeholder="120000" onInput={(event) => setEditingTimeoutMs(event.currentTarget.value)} />
            </label>
            <label class="settings-field">
              <span class="settings-field-label">
                Tavily API Key <em>Optional</em>
                <button ref={learnMoreButton} type="button" class="settings-inline-link-button" onClick={() => setShowTavilyInfo(true)}>Learn more</button>
              </span>
              <input type="password" value={editingTavilyApiKey()} onInput={(event) => setEditingTavilyApiKey(event.currentTarget.value)} />
            </label>
          </div>
        </Show>

        <section class="effort-settings-editor">
          <h3>Efforts Settings <em>Optional</em></h3>
          <div class="effort-tabs" role="tablist" aria-label="Effort level">
            <For each={EFFORT_LEVELS}>
              {(level) => <button type="button" role="tab" aria-selected={selectedLevel() === level} classList={{ "is-active": selectedLevel() === level }} onClick={() => setSelectedLevel(level)}>{effortLevelLabel(level)}</button>}
            </For>
          </div>
          <Show when={props.provider.code === "ollama"} fallback={
            <label class="settings-field">
              <span>{effortLevelLabel(selectedLevel())} Effort</span>
              <textarea rows="5" value={editingTexts()[selectedLevel()]} onInput={(event) => setEditingTexts((current) => ({ ...current, [selectedLevel()]: event.currentTarget.value }))} placeholder={effortSettingsPlaceholder(props.provider.code)} />
            </label>
          }>
            <label class="settings-field">
              <span>{effortLevelLabel(selectedLevel())} model {selectedLevel() === "default" && <em>Required for selected default provider</em>}</span>
              <select value={editingModels()[selectedLevel()]} disabled={ollamaModelsLoading() || Boolean(ollamaModelsError()) || ollamaModels().length === 0} onChange={(event) => setEditingModels((current) => ({ ...current, [selectedLevel()]: event.currentTarget.value }))}>
                <option value="">{selectedLevel() === "default" && props.isDefaultProvider ? "Select a model" : "No explicit model"}</option>
                <For each={ollamaModels()}>{(model) => <option value={model.value}>{model.label}</option>}</For>
              </select>
            </label>
          </Show>
          <div class="settings-modal-status" aria-live="polite">
            <Show when={ollamaModelsLoading()}><span>Loading models...</span></Show>
            <Show when={!ollamaModelsLoading() && !ollamaModelsError() && props.provider.code === "ollama" && ollamaModels().length === 0}><span>No Ollama models available.</span></Show>
            <Show when={ollamaModelsError()}><span class="settings-field-error">{ollamaModelsError()}</span></Show>
            <Show when={fieldError()}><span class="settings-field-error">{fieldError()}</span></Show>
          </div>
        </section>

        <footer class="settings-modal-actions">
          <button type="button" class="settings-secondary-button" onClick={() => popup.close()}>Cancel</button>
          <button type="submit" class="settings-primary-button" disabled={props.provider.code === "ollama" && !canApplyOllama()}>Apply</button>
        </footer>
      </form>
    </Show>
  );
});

export default EditingProviderPopup;

function effortTextsFromArgs(efforts: EffortsArgs): Record<EffortLevel, string> {
  return {
    default: safeArgsToText(efforts.default),
    low: safeArgsToText(efforts.low),
    high: safeArgsToText(efforts.high),
  };
}

function modelsFromArgs(efforts: EffortsArgs): Record<EffortLevel, string> {
  return {
    default: modelFromArgs(efforts.default),
    low: modelFromArgs(efforts.low),
    high: modelFromArgs(efforts.high),
  };
}

function safeArgsToText(args: string[]): string {
  try { return effortArgsToText(args); } catch { return ""; }
}

function modelFromArgs(args: string[]): string {
  const index = args.findIndex((arg) => arg === "--model");
  return index >= 0 ? args[index + 1] ?? "" : "";
}

function isModelInList(value: string, models: OllamaModelOption[]): boolean {
  return !value || models.some((model) => model.value === value);
}

function parseOptionalTimeout(value: string): { ok: true; value: number } | { ok: false; error: string } {
  const trimmed = value.trim();
  if (!trimmed) return { ok: true, value: DEFAULT_OLLAMA_TIMEOUT_MS };
  if (!/^[0-9]+$/.test(trimmed)) return { ok: false, error: "Timeout must be a positive integer." };
  const parsed = Number(trimmed);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) return { ok: false, error: "Timeout must be a positive integer." };
  return { ok: true, value: parsed };
}

function formatError(err: unknown): string {
  if (!err) return "Unknown error";
  if (typeof err === "string") return err;
  const value = err as { code?: string; message?: string };
  if (value.code && value.message) return `${value.code}: ${value.message}`;
  return value.message || JSON.stringify(err);
}

function normalizeBaseUrlInput(value: string): { ok: true; value: string } | { ok: false; error: string } {
  const trimmed = value.trim();
  if (!trimmed) return { ok: true, value: DEFAULT_OLLAMA_BASE_URL };
  try {
    const url = new URL(trimmed);
    if (url.protocol !== "http:" && url.protocol !== "https:") return { ok: false, error: "Base URL must use http:// or https://." };
    if (url.pathname.split("/").some((segment) => segment.toLowerCase() === "api")) return { ok: false, error: "Base URL must not include /api." };
    return { ok: true, value: trimmed.replace(/\/+$/, "") };
  } catch {
    return { ok: false, error: "Base URL must be a valid absolute URL." };
  }
}

function normalizeApiKeyInput(value: string): { ok: true; value: string } | { ok: false; error: string } {
  const trimmed = value.trim();
  return trimmed ? { ok: true, value: trimmed } : { ok: false, error: "Ollama API key is required. For local models, enter 'ollama'." };
}
