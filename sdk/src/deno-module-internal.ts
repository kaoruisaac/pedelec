/**
 * Browser-safe private interop shared by the root SDK entry and the Vite
 * build-time plugin.  This file is intentionally not re-exported from the
 * package root: application code authors the small DenoModuleDefinition only.
 */

export const PREPARED_DENO_MODULE_ARTIFACT_PROPERTY = "__pedelecArtifact";

export type PreparedDenoModuleArtifact = {
  readonly format: "esm";
  readonly runtimeSource: string;
  readonly typesSource: string;
  readonly contentHash?: string;
};

export function getPreparedDenoModuleArtifact(
  value: unknown,
): PreparedDenoModuleArtifact | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;

  const artifact = (value as Record<string, unknown>)[PREPARED_DENO_MODULE_ARTIFACT_PROPERTY];
  if (!artifact || typeof artifact !== "object" || Array.isArray(artifact)) return null;

  const candidate = artifact as Record<string, unknown>;
  if (
    candidate.format !== "esm" ||
    typeof candidate.runtimeSource !== "string" ||
    candidate.runtimeSource.trim().length === 0 ||
    typeof candidate.typesSource !== "string" ||
    candidate.typesSource.trim().length === 0
  ) {
    return null;
  }

  return {
    format: "esm",
    runtimeSource: candidate.runtimeSource,
    typesSource: candidate.typesSource,
    ...(typeof candidate.contentHash === "string" ? { contentHash: candidate.contentHash } : {}),
  };
}

const DENO_MODULE_SEGMENT_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

/**
 * Keep this deliberately narrower than arbitrary URL/path syntax.  Core has
 * the corresponding contract in Rust; both sides accept ordinary bare and
 * scoped package-like specifiers only.
 */
export function isValidDenoModuleName(value: unknown): value is string {
  if (typeof value !== "string" || value.length === 0 || value.trim() !== value) return false;
  if (value === "." || value === ".." || value.includes("\\") || value.includes("\0")) return false;
  if (value.includes(":") || value.startsWith("/") || value.endsWith("/")) return false;

  const parts = value.split("/");
  if (parts.length === 1) return DENO_MODULE_SEGMENT_PATTERN.test(parts[0]);
  if (parts.length !== 2 || !parts[0].startsWith("@") || parts[0].length === 1) return false;

  const scope = parts[0].slice(1);
  const packageName = parts[1];
  return DENO_MODULE_SEGMENT_PATTERN.test(scope) && DENO_MODULE_SEGMENT_PATTERN.test(packageName);
}
