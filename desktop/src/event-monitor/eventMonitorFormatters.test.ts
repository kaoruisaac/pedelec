import { describe, expect, it } from "vitest";
import { errorTitle } from "./eventMonitorFormatters";

describe("errorTitle", () => {
  it("identifies provider errors", () => {
    expect(errorTitle({ source: "provider", provider: "codex", error: { message: "provider command failed" } }))
      .toBe("[provider / codex] provider command failed");
  });

  it("identifies core errors", () => {
    expect(errorTitle({ source: "core", error: { message: "Pedelec internal operation failed" } }))
      .toBe("[core] Pedelec internal operation failed");
  });

  it("keeps legacy error titles unchanged", () => {
    expect(errorTitle({ error: { message: "legacy error" } })).toBe("legacy error");
  });
});
