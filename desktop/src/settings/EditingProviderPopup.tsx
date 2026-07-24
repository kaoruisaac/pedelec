import { createMemo, createSignal, For, onMount, Show } from "solid-js";
import { forwardPopUp } from "../services/PopUpProvider";
import { OllamaModelOption, Provider } from "./types";
import { invoke } from "@tauri-apps/api/core";
import { DEFAULT_OLLAMA_BASE_URL, DEFAULT_OLLAMA_TIMEOUT_MS } from "./constants";

interface EditingProviderPopupProps {
    provider: Provider;
    editingModel?: string;
    editingBaseUrl: string;
    editingTimeoutMs: string;
    editingApiKey: string;
    onApply: ({ model, baseUrl, timeoutMs, apiKey }: { model: string; baseUrl?: string; timeoutMs?: number; apiKey?: string }) => void;
}

const EditingProviderPopup = forwardPopUp((popup, props: EditingProviderPopupProps) => {
    const [editingModel, setEditingModel] = createSignal(props.editingModel ?? "");
    const [editingBaseUrl, setEditingBaseUrl] = createSignal(props.editingBaseUrl ?? "");
    const [editingTimeoutMs, setEditingTimeoutMs] = createSignal(props.editingTimeoutMs ?? "");
    const [editingApiKey, setEditingApiKey] = createSignal(props.editingApiKey ?? "");
    const [ollamaModelsLoading, setOllamaModelsLoading] = createSignal(false);
    const [ollamaModelsError, setOllamaModelsError] = createSignal("");
    const [ollamaModels, setOllamaModels] = createSignal<OllamaModelOption[]>([]);
    const [fieldError, setFieldError] = createSignal("");

    const canApplyOllama = createMemo(() => {
        if (ollamaModelsLoading() || ollamaModelsError() || fieldError()) return false;
        if (!editingModel()) return false;
        return ollamaModels().some((model) => model.value === editingModel());
    });
    
    async function loadOllamaModels(
        baseUrlValue = editingBaseUrl(),
        apiKeyValue = editingApiKey(),
        timeoutValue = editingTimeoutMs(),
        currentModel = editingModel(),
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
            input: {
              baseUrl: baseUrl.value,
              apiKey: apiKey.value,
              timeoutMs: timeout.value,
            },
          });
    
          const nextModels = Array.isArray(models) ? models : [];
          setOllamaModels(nextModels);
          if (currentModel && nextModels.some((model) => model.value === currentModel)) {
            setEditingModel(currentModel);
          } else {
            setEditingModel("");
          }
        } catch (err) {
          setOllamaModels([]);
          setOllamaModelsError(formatError(err));
          setEditingModel("");
        } finally {
          setOllamaModelsLoading(false);
        }
    }

    function applyEditor(event: Event): void {
        event.preventDefault();
        const provider = props.provider;
        if (!provider) return;
    
        if (provider.code === "ollama") {
          applyOllamaEditor();
          return;
        }
    
        const trimmedModel = editingModel().trim();
        props.onApply({ model: trimmedModel });
        popup.close();
      }
    
    function applyOllamaEditor(): void {
    setFieldError("");
    if (ollamaModelsLoading() || ollamaModelsError()) return;

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
    const model = editingModel();
    if (!model || !ollamaModels().some((option) => option.value === model)) {
        setFieldError("Select an Ollama model from the latest model list.");
        return;
    }
    props.onApply({ model, baseUrl: baseUrl.value, timeoutMs: timeout.value, apiKey: apiKey.value });
    popup.close();
    }

    onMount(() => {
        loadOllamaModels();
    });

    return (
        <form class="settings-modal" role="dialog" aria-modal="true" onSubmit={applyEditor}>
            <header class="settings-modal-header">
            <div>
                <h2>{props.provider.name}</h2>
                <p>Edit provider settings</p>
            </div>
            </header>

            <Show
            when={props.provider.code === "ollama"}
            fallback={
                <label class="settings-field">
                <span>
                    Default Model <em>Optional</em>
                </span>
                <input
                    type="text"
                    value={editingModel()}
                    onInput={(event) => setEditingModel(event.currentTarget.value)}
                    autofocus
                />
                </label>
            }
            >
            <div class="settings-modal-fields">
                <label class="settings-field">
                <span>
                    Base URL <em>Optional</em>
                </span>
                <input
                    type="text"
                    value={editingBaseUrl()}
                    placeholder={DEFAULT_OLLAMA_BASE_URL}
                    onInput={(event) => setEditingBaseUrl(event.currentTarget.value)}
                    onBlur={() => loadOllamaModels()}
                    autofocus
                />
                </label>
                <label class="settings-field">
                <span>
                    API Key <em>Required</em>
                </span>
                <input
                    type="password"
                    value={editingApiKey()}
                    placeholder={"If it is a local server, type in: 'ollama'"}
                    onInput={(event) => setEditingApiKey(event.currentTarget.value)}
                    onBlur={() => loadOllamaModels()}
                />
                </label>
                <label class="settings-field">
                <span>
                    Timeout Milliseconds <em>Optional</em>
                </span>
                <input
                    type="text"
                    inputmode="numeric"
                    value={editingTimeoutMs()}
                    placeholder="120000"
                    onInput={(event) => setEditingTimeoutMs(event.currentTarget.value)}
                />
                </label>
                <label class="settings-field">
                <span>
                    Default Model <em>Required</em>
                </span>
                <select
                    value={editingModel()}
                    disabled={ollamaModelsLoading() || Boolean(ollamaModelsError()) || ollamaModels().length === 0}
                    onChange={(event) => setEditingModel(event.currentTarget.value)}
                >
                    <option value="">Select a model</option>
                    <For each={ollamaModels()}>
                      {(model) => <option value={model.value}>{model.label}</option>}
                    </For>
                </select>
                </label>
                <div class="settings-modal-status" aria-live="polite">
                <Show when={ollamaModelsLoading()}>
                    <span>Loading models...</span>
                </Show>
                <Show when={!ollamaModelsLoading() && !ollamaModelsError() && ollamaModels().length === 0}>
                    <span>No Ollama models available.</span>
                </Show>
                <Show when={ollamaModelsError()}>
                    <span class="settings-field-error">{ollamaModelsError()}</span>
                </Show>
                <Show when={fieldError()}>
                    <span class="settings-field-error">{fieldError()}</span>
                </Show>
                </div>
            </div>
            </Show>
            <footer class="settings-modal-actions">
            <button type="button" class="settings-secondary-button" onClick={() => popup.close()}>
                Cancel
            </button>
            <button
                type="submit"
                class="settings-primary-button"
                disabled={props.provider.code === "ollama" && !canApplyOllama()}
            >
                Apply
            </button>
            </footer>
        </form>
    )
})

export default EditingProviderPopup;

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
function formatError(err: unknown): string {
    if (!err) return "Unknown error";
    if (typeof err === "string") return err;
    const e = err as { code?: string; message?: string };
    if (e.code && e.message) return `${e.code}: ${e.message}`;
    return e.message || JSON.stringify(err);
}
function normalizeBaseUrlInput(value: string): { ok: true; value: string } | { ok: false; error: string } {
    const trimmed = value.trim();
    if (!trimmed) return { ok: true, value: DEFAULT_OLLAMA_BASE_URL };
    try {
      const url = new URL(trimmed);
      if (url.protocol !== "http:" && url.protocol !== "https:") {
        return { ok: false, error: "Base URL must use http:// or https://." };
      }
      if (url.pathname.split("/").some((segment) => segment.toLowerCase() === "api")) {
        return { ok: false, error: "Base URL must not include /api. Use https://ollama.com or http://127.0.0.1:11434." };
      }
      return { ok: true, value: trimmed.replace(/\/+$/, "") };
    } catch {
      return { ok: false, error: "Base URL must be a valid absolute URL." };
    }
}

function normalizeApiKeyInput(value: string): { ok: true; value: string } | { ok: false; error: string } {
    const trimmed = value.trim();
    if (!trimmed) {
        return { ok: false, error: "Ollama API key is required. For local models, enter 'ollama'." };
    }
    return { ok: true, value: trimmed };
}
