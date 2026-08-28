import { describe, expect, it } from "vitest";
import {
  cloneEffortsArgs,
  configuredEffortLevels,
  effortArgsToText,
  effortSettingsPlaceholder,
  effortTextToArgs,
  EFFORT_LEVELS,
  ollamaModelsToEffortsArgs,
  validateOllamaDefaultModelSelection,
  validateEffortArgs,
} from "./effortSettings";

describe("effort settings", () => {
  it("converts each textarea row into a key/value argv pair", () => {
    expect(effortTextToArgs('-c model_reasoning_effort="high"\n-m gpt-5-sol')).toEqual([
      "-c",
      'model_reasoning_effort="high"',
      "-m",
      "gpt-5-sol",
    ]);
  });

  it("reconstructs persisted argv pairs as textarea rows", () => {
    expect(effortArgsToText(["-c", 'model_reasoning_effort="high"', "-m", "gpt-5-sol"])).toBe(
      '-c model_reasoning_effort="high"\n-m gpt-5-sol',
    );
  });

  it("rejects malformed rows and unsafe provider keys", () => {
    expect(() => effortTextToArgs("--model")).toThrow();
    expect(validateEffortArgs("codex", ["--sandbox", "danger-full-access"])).toContain("not allowed");
    expect(validateEffortArgs("codex", ["-c", "model_reasoning_effort=high"])).toBeUndefined();
  });

  it("keeps the public effort order stable", () => {
    expect(EFFORT_LEVELS).toEqual(["default", "low", "high"]);
  });

  it("keeps all effort tiers independent during conversion", () => {
    const original = {
      default: ["--model", "model-default"],
      low: ["--model", "model-low"],
      high: [],
    };
    const cloned = cloneEffortsArgs(original);
    cloned.low.push("changed");
    expect(cloned).toEqual({
      default: ["--model", "model-default"],
      low: ["--model", "model-low", "changed"],
      high: [],
    });
    expect(original).toEqual({
      default: ["--model", "model-default"],
      low: ["--model", "model-low"],
      high: [],
    });
  });

  it("summarizes only configured tiers in default/low/high order", () => {
    expect(configuredEffortLevels({ default: [], low: ["--model", "low"], high: ["--model", "high"] }))
      .toEqual(["low", "high"]);
    expect(configuredEffortLevels({ default: [], low: [], high: [] })).toEqual([]);
  });

  it("serializes Ollama models as independent direct model pairs", () => {
    expect(ollamaModelsToEffortsArgs({
      default: "model-default",
      low: "model-low",
      high: "",
    })).toEqual({
      default: ["--model", "model-default"],
      low: ["--model", "model-low"],
      high: [],
    });
  });

  it("requires a default Ollama model only for the selected default provider", () => {
    const empty = { default: "", low: "model-low", high: "" };
    expect(validateOllamaDefaultModelSelection(empty, true)).toContain("Default Ollama model");
    expect(validateOllamaDefaultModelSelection(empty, false)).toBeUndefined();
  });

  it("uses provider-specific effort settings placeholders", () => {
    expect(effortSettingsPlaceholder("codex")).toBe('-m model-name\n-c model_reasoning_effort="high"');
    expect(effortSettingsPlaceholder("antigravity")).toBe("--model model-name\n--effort high");
    expect(effortSettingsPlaceholder("claude")).toBe("--model model-name\n--effort high");
    expect(effortSettingsPlaceholder("opencode")).toBe("--model provider/model-name");
    expect(effortSettingsPlaceholder("cursor")).toBe("--model model-name");
    expect(effortSettingsPlaceholder("ollama")).toBe("");
  });

  it("mirrors provider-native effort value policy", () => {
    expect(validateEffortArgs("codex", ["-m", "gpt-5", "-c", 'model_reasoning_effort="xhigh"'])).toBeUndefined();
    expect(validateEffortArgs("codex", ["-m", "gpt-5", "-c", 'model_reasoning_effort="max"'])).toBeUndefined();
    expect(validateEffortArgs("codex", ["-c", "model_reasoning_effort=banana"])).toContain("not supported");
    expect(validateEffortArgs("codex", ["-c", "skills.include_instructions=true"])).toContain("not supported");
    expect(validateEffortArgs("antigravity", ["--effort", "high"])).toBeUndefined();
    expect(validateEffortArgs("antigravity", ["--effort", "xhigh"])).toContain("not supported");
    expect(validateEffortArgs("claude", ["--effort", "max"])).toBeUndefined();
    expect(validateEffortArgs("claude", ["--effort", "banana"])).toContain("not supported");
    expect(validateEffortArgs("opencode", ["--effort", "high"])).toContain("not allowed");
    expect(validateEffortArgs("cursor", ["--effort", "high"])).toContain("not allowed");
    expect(validateEffortArgs("ollama", ["--effort", "high"])).toContain("not allowed");
  });

  it("deep clones all independent effort tiers", () => {
    const original = { default: ["-m", "a"], low: [], high: ["-m", "c"] };
    const cloned = cloneEffortsArgs(original);
    cloned.high.push("changed");
    expect(original.high).toEqual(["-m", "c"]);
  });
});
