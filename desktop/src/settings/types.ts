export type ProviderCode = "codex" | "gemini" | "opencode" | "cursor" | "claude" | "ollama";

export interface Provider {
  code: ProviderCode;
  name: string;
  available: boolean;
  error?: string | null;
  description?: string;
  connectionStatus?: "connected" | "disconnected";
}

export interface OllamaProviderSettings {
  baseUrl: string;
  timeoutMs: number;
  apiKey: string;
}

export interface ProviderSettings {
  ollama: OllamaProviderSettings;
}

export interface Settings {
  defaultProvider: ProviderCode | null;
  defaultModels: Partial<Record<ProviderCode, string>>;
  providerSettings: ProviderSettings;
}

export interface OllamaModelOption {
  value: string;
  label: string;
}
