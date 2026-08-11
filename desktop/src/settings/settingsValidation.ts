import { Provider, Settings } from "./types";

export function selectedOllamaModel(settings: Settings): string {
  const args = settings.providerSettings.ollama.effortsArgs.default;
  const modelIndex = args.findIndex((arg) => arg === "--model");
  return modelIndex >= 0 ? args[modelIndex + 1]?.trim() ?? "" : "";
}

export function canSaveSettings(
  settings: Settings,
  provider: Provider | undefined,
  saving = false,
): boolean {
  if (!provider || saving) return false;
  if (provider.code === "ollama") {
    return Boolean(
      settings.providerSettings.ollama.apiKey.trim()
      && selectedOllamaModel(settings),
    );
  }
  return Boolean(provider.available);
}
