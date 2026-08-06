import { Provider, Settings } from "./types";

export function canSaveSettings(
  settings: Settings,
  provider: Provider | undefined,
  saving = false,
): boolean {
  if (!provider || saving) return false;
  if (provider.code === "ollama") {
    return Boolean(
      settings.providerSettings.ollama.apiKey.trim()
      && settings.defaultModels.ollama?.trim(),
    );
  }
  return Boolean(provider.available);
}
