import { describe, expect, it } from "vitest";
import {
  buildAutomaticDefaultProviderSettings,
  findFirstAvailableCliProvider,
} from "./providerInitialization";
import { canSaveSettings } from "./settingsValidation";
import { Provider, Settings } from "./types";
import { emptyEffortsArgs } from "./effortSettings";

const provider = (overrides: Partial<Provider>): Provider => ({
  code: "codex",
  name: "Codex",
  scanned: true,
  available: false,
  ...overrides,
});

const settings = (overrides: Partial<Settings> = {}): Settings => ({
  defaultProvider: null,
  providerSettings: {
    codex: { effortsArgs: emptyEffortsArgs() },
    antigravity: { effortsArgs: emptyEffortsArgs() },
    opencode: { effortsArgs: emptyEffortsArgs() },
    cursor: { effortsArgs: emptyEffortsArgs() },
    claude: { effortsArgs: emptyEffortsArgs() },
    ollama: {
      baseUrl: "http://127.0.0.1:11434",
      timeoutMs: 120_000,
      apiKey: "",
      tavilyApiKey: "",
      effortsArgs: emptyEffortsArgs(),
    },
  },
  ...overrides,
});

describe("findFirstAvailableCliProvider", () => {
  it("selects the first available provider in the Core-provided order", () => {
    expect(findFirstAvailableCliProvider([
      provider({ code: "codex", available: true }),
      provider({ code: "antigravity", name: "Antigravity" }),
      provider({ code: "opencode", name: "OpenCode", available: true }),
    ])?.code).toBe("codex");
  });

  it("selects Antigravity when it is the only available CLI provider", () => {
    expect(findFirstAvailableCliProvider([
      provider({ code: "codex" }),
      provider({ code: "antigravity", name: "Antigravity", available: true }),
    ])?.code).toBe("antigravity");
  });

  it("does not select providers whose scan has not completed", () => {
    expect(findFirstAvailableCliProvider([
      provider({ scanned: false, available: true }),
    ])).toBeUndefined();
  });

  it("never selects Ollama even if Rust reports it as available", () => {
    expect(findFirstAvailableCliProvider([
      provider({ code: "ollama", name: "Ollama", available: true }),
    ])).toBeUndefined();
  });

  it("returns undefined when no CLI provider is available", () => {
    expect(findFirstAvailableCliProvider([provider({ code: "cursor", name: "Cursor" })])).toBeUndefined();
  });

  it("uses the input order without a frontend priority list", () => {
    expect(findFirstAvailableCliProvider([
      provider({ code: "opencode", name: "OpenCode", available: true }),
      provider({ code: "codex", available: true }),
    ])?.code).toBe("opencode");
  });

  it("only changes the default provider during automatic initialization", () => {
    const initialSettings = settings();

    const nextSettings = buildAutomaticDefaultProviderSettings(initialSettings, "codex");

    expect(nextSettings.defaultProvider).toBe("codex");
    expect(nextSettings.providerSettings.codex.effortsArgs).toEqual({ default: [], low: [], high: [] });
  });

  it("preserves explicitly saved Ollama effort profiles during automatic initialization", () => {
    const initialSettings = settings({
      providerSettings: {
        ...settings().providerSettings,
        ollama: {
          ...settings().providerSettings.ollama,
          effortsArgs: { default: ["--model", "qwen3:8b"], low: [], high: [] },
        },
      },
    });

    const nextSettings = buildAutomaticDefaultProviderSettings(initialSettings, "codex");

    expect(nextSettings.providerSettings.ollama.effortsArgs.default).toEqual(["--model", "qwen3:8b"]);
  });

  it("fails closed if Ollama bypasses the automatic provider type", () => {
    expect(() => buildAutomaticDefaultProviderSettings(
      settings(),
      "ollama" as never,
    )).toThrow(
      "Ollama cannot be selected by automatic default provider initialization.",
    );
  });
});

describe("canSaveSettings", () => {
  it("rejects Ollama without a model", () => {
    expect(canSaveSettings(settings(), provider({ code: "ollama", available: true }))).toBe(false);
  });

  it("allows Ollama after the user has applied an API key and model", () => {
    expect(canSaveSettings(
      settings({
        providerSettings: {
          ...settings().providerSettings,
          ollama: {
            baseUrl: "http://127.0.0.1:11434",
            timeoutMs: 120_000,
            apiKey: "ollama",
            tavilyApiKey: "",
            effortsArgs: { default: ["--model", "qwen3:8b"], low: [], high: [] },
          },
        },
      }),
      provider({ code: "ollama", available: true }),
    )).toBe(true);
  });

  it("allows a non-Ollama provider when the unused Ollama API key is empty", () => {
    expect(canSaveSettings(
      settings({ defaultProvider: "codex" }),
      provider({ code: "codex", available: true }),
    )).toBe(true);
  });
});
