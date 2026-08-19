import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { applyEffortWizardSettings, normalizeHomeReminder } from "./effortWizardApi";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));

const invokeMock = vi.mocked(invoke);

describe("effort wizard API adapter", () => {
  beforeEach(() => vi.clearAllMocks());

  it("maps internal Keep Current decisions to the backend camelCase enum", async () => {
    invokeMock.mockResolvedValue({
      state: { runId: "run", status: "completed", selectedProviders: [], providers: [] },
      bootstrap: { providers: [], homeReminder: null },
    });
    await applyEffortWizardSettings({
      runId: "run",
      providers: [{ provider: "codex", tiers: { low: "update", default: "keep_current", high: "keep_current" } }],
    });
    expect(invokeMock).toHaveBeenCalledWith("apply_effort_wizard_settings", {
      input: {
        runId: "run",
        providers: [{ provider: "codex", tiers: { low: "update", default: "keepCurrent", high: "keepCurrent" } }],
      },
    });
  });

  it("normalizes backend reminder enum shapes", () => {
    expect(normalizeHomeReminder("initialSetup")).toEqual({ type: "initial_setup" });
    expect(normalizeHomeReminder({ presetUpdate: { providers: ["codex"] } })).toEqual({
      type: "preset_update",
      providers: ["codex"],
    });
  });
});

