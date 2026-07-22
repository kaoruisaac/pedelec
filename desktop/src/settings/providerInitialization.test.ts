import { describe, expect, it } from "vitest";
import { findFirstAvailableCliProvider } from "./providerInitialization";
import { Provider } from "./types";

const provider = (overrides: Partial<Provider>): Provider => ({
  code: "codex",
  name: "Codex",
  scanned: true,
  available: false,
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
});
