import { Provider, ProviderCode, Settings } from "./types";

export type AutomaticProviderCode = Exclude<ProviderCode, "ollama">;
const AUTOMATIC_PROVIDER_CODES: readonly AutomaticProviderCode[] = [
  "codex",
  "antigravity",
  "opencode",
  "cursor",
  "claude",
];

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
 * In particular, it must not infer or create an effort profile for any provider.
 */
export function buildAutomaticDefaultProviderSettings(
  initialSettings: Settings,
  providerCode: AutomaticProviderCode,
): Settings {
  if (!AUTOMATIC_PROVIDER_CODES.includes(providerCode)) {
    throw new Error(
      "Ollama cannot be selected by automatic default provider initialization.",
    );
  }

  return {
    ...initialSettings,
    defaultProvider: providerCode,
    providerSettings: {
      ...initialSettings.providerSettings,
      codex: { effortsArgs: { default: [...initialSettings.providerSettings.codex.effortsArgs.default], low: [...initialSettings.providerSettings.codex.effortsArgs.low], high: [...initialSettings.providerSettings.codex.effortsArgs.high] } },
      antigravity: { effortsArgs: { default: [...initialSettings.providerSettings.antigravity.effortsArgs.default], low: [...initialSettings.providerSettings.antigravity.effortsArgs.low], high: [...initialSettings.providerSettings.antigravity.effortsArgs.high] } },
      opencode: { effortsArgs: { default: [...initialSettings.providerSettings.opencode.effortsArgs.default], low: [...initialSettings.providerSettings.opencode.effortsArgs.low], high: [...initialSettings.providerSettings.opencode.effortsArgs.high] } },
      cursor: { effortsArgs: { default: [...initialSettings.providerSettings.cursor.effortsArgs.default], low: [...initialSettings.providerSettings.cursor.effortsArgs.low], high: [...initialSettings.providerSettings.cursor.effortsArgs.high] } },
      claude: { effortsArgs: { default: [...initialSettings.providerSettings.claude.effortsArgs.default], low: [...initialSettings.providerSettings.claude.effortsArgs.low], high: [...initialSettings.providerSettings.claude.effortsArgs.high] } },
      ollama: { ...initialSettings.providerSettings.ollama, effortsArgs: { default: [...initialSettings.providerSettings.ollama.effortsArgs.default], low: [...initialSettings.providerSettings.ollama.effortsArgs.low], high: [...initialSettings.providerSettings.ollama.effortsArgs.high] } },
    },
  };
}
