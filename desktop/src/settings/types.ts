export type ProviderCode = "codex" | "antigravity" | "opencode" | "cursor" | "claude" | "ollama";
export type EffortLevel = "default" | "low" | "high";

export interface EffortsArgs {
  default: string[];
  low: string[];
  high: string[];
}

export interface Provider {
  code: ProviderCode;
  name: string;
  scanned: boolean;
  version?: string | null;
  available: boolean;
  error?: string | null;
  description?: string;
  connectionStatus?: "connected" | "disconnected";
}

export interface OllamaProviderSettings {
  baseUrl: string;
  timeoutMs: number;
  apiKey: string;
  tavilyApiKey: string;
  effortsArgs: EffortsArgs;
}

export interface CommonProviderSettings {
  effortsArgs: EffortsArgs;
}

export interface ProviderSettings {
  codex: CommonProviderSettings;
  antigravity: CommonProviderSettings;
  opencode: CommonProviderSettings;
  cursor: CommonProviderSettings;
  claude: CommonProviderSettings;
  ollama: OllamaProviderSettings;
}

export interface Settings {
  defaultProvider: ProviderCode | null;
  providerSettings: ProviderSettings;
}

export interface OllamaModelOption {
  value: string;
  label: string;
}
