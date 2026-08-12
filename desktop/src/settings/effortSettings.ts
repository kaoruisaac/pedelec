import { EffortLevel, EffortsArgs, ProviderCode } from "./types";

export const EFFORT_LEVELS: EffortLevel[] = ["default", "low", "high"];

export const PROVIDER_NATIVE_EFFORT_VALUES: Record<ProviderCode, readonly string[]> = {
  codex: ["low", "medium", "high", "xhigh"],
  antigravity: ["low", "medium", "high"],
  claude: ["low", "medium", "high", "xhigh", "max"],
  opencode: [],
  cursor: [],
  ollama: [],
};

const PROVIDER_EFFORT_SETTINGS_PLACEHOLDERS: Record<Exclude<ProviderCode, "ollama">, string> = {
  codex: '-m model-name\n-c model_reasoning_effort="high"',
  antigravity: "--model model-name\n--effort high",
  claude: "--model model-name\n--effort high",
  opencode: "--model provider/model-name",
  cursor: "--model model-name",
};

export function effortSettingsPlaceholder(provider: ProviderCode): string {
  return provider === "ollama" ? "" : PROVIDER_EFFORT_SETTINGS_PLACEHOLDERS[provider];
}

export function emptyEffortsArgs(): Record<EffortLevel, string[]> {
  return { default: [], low: [], high: [] };
}

export function cloneEffortsArgs(
  efforts: Record<EffortLevel, string[]> | undefined,
): Record<EffortLevel, string[]> {
  return {
    default: [...(efforts?.default ?? [])],
    low: [...(efforts?.low ?? [])],
    high: [...(efforts?.high ?? [])],
  };
}

export function configuredEffortLevels(efforts: EffortsArgs | undefined): EffortLevel[] {
  return EFFORT_LEVELS.filter((level) => (efforts?.[level] ?? []).length > 0);
}

export function ollamaModelsToEffortsArgs(
  models: Record<EffortLevel, string>,
): EffortsArgs {
  return {
    default: models.default ? ["--model", models.default] : [],
    low: models.low ? ["--model", models.low] : [],
    high: models.high ? ["--model", models.high] : [],
  };
}

export function validateOllamaDefaultModelSelection(
  models: Record<EffortLevel, string>,
  isDefaultProvider: boolean,
): string | undefined {
  if (isDefaultProvider && !models.default.trim()) {
    return "Select a Default Ollama model before saving.";
  }
  return undefined;
}

/** Converts the deliberately simple key/value textarea format into argv tokens. */
export function effortTextToArgs(text: string): string[] {
  const args: string[] = [];
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line) continue;
    const match = /^(\S+)\s+(.+)$/.exec(line);
    if (!match) {
      throw new Error("Each effort setting row must contain a key and a value.");
    }
    args.push(match[1], match[2].trim());
  }
  return args;
}

export function effortArgsToText(args: string[]): string {
  const lines: string[] = [];
  for (let index = 0; index < args.length; index += 2) {
    if (args[index + 1] === undefined) {
      throw new Error("Effort settings must contain key/value pairs.");
    }
    lines.push(`${args[index]} ${args[index + 1]}`);
  }
  return lines.join("\n");
}

export function validateEffortArgs(
  provider: ProviderCode,
  args: string[],
): string | undefined {
  if (args.length % 2 !== 0) return "Each effort setting row must contain a key and a value.";
  const seen = new Set<string>();
  for (let index = 0; index < args.length; index += 2) {
    const key = args[index];
    const value = args[index + 1]?.trim();
    if (!value) return `Value for ${key || "the setting"} must not be empty.`;
    if (seen.has(key)) return `Duplicate setting key: ${key}.`;
    seen.add(key);

    const modelKey = provider === "codex" ? "-m" : "--model";
    if (key === modelKey) continue;

    if (key === "--effort") {
      if (provider !== "antigravity" && provider !== "claude") {
        return `Setting key ${key} is not allowed for ${provider}.`;
      }
      if (!PROVIDER_NATIVE_EFFORT_VALUES[provider].includes(value)) {
        return `Native effort value ${value} is not supported for ${provider}.`;
      }
      continue;
    }

    if (key === "-c" && provider === "codex") {
      const nativeEffort = parseCodexReasoningEffort(value);
      if (!nativeEffort || !PROVIDER_NATIVE_EFFORT_VALUES.codex.includes(nativeEffort)) {
        return `Native effort value in ${value} is not supported for codex.`;
      }
      continue;
    }

    return `Setting key ${key} is not allowed for ${provider}; use ${modelKey} for model selection.`;
  }
  return undefined;
}

function parseCodexReasoningEffort(value: string): string | undefined {
  const trimmed = value.trim();
  const prefix = "model_reasoning_effort";
  if (!trimmed.startsWith(prefix)) return undefined;
  const remainder = trimmed.slice(prefix.length).trimStart();
  if (!remainder.startsWith("=")) return undefined;
  const raw = remainder.slice(1).trim();
  if (!raw) return undefined;
  if (raw.startsWith('"')) {
    if (!raw.endsWith('"') || raw.length < 2) return undefined;
    const quoted = raw.slice(1, -1).trim();
    return quoted && !quoted.includes('"') ? quoted : undefined;
  }
  return raw.includes('"') ? undefined : raw;
}

export function effortLevelLabel(level: EffortLevel): string {
  return level[0].toUpperCase() + level.slice(1);
}
