import { Provider, ProviderCode, Settings } from "./types";

export type AutomaticProviderCode = Exclude<ProviderCode, "ollama">;

export type AutomaticProvider = Provider & {
  code: AutomaticProviderCode;
};

function isAvailableCliProvider(provider: Provider): provider is AutomaticProvider {
  return provider.code !== "ollama" && provider.scanned && provider.available;
}

export function findFirstAvailableCliProvider(
  providers: Provider[],
): AutomaticProvider | undefined {
  return providers.find(isAvailableCliProvider);
}

/**
 * Automatic initialization is intentionally limited to selecting a provider.
 * In particular, it must not infer or create a model for any provider.
 */
export function buildAutomaticDefaultProviderSettings(
  initialSettings: Settings,
  providerCode: AutomaticProviderCode,
): Settings {
  const providerCodeValue: ProviderCode = providerCode;
  if (providerCodeValue === "ollama") {
    throw new Error(
      "Ollama cannot be selected by automatic default provider initialization.",
    );
  }

  return {
    ...initialSettings,
    defaultProvider: providerCode,
    defaultModels: { ...initialSettings.defaultModels },
  };
}
