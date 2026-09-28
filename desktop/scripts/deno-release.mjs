/**
 * Pinned Deno release metadata shared by release tooling.
 *
 * `desktop/deno-runtime-manifest.json` is the only authoritative copy.
 * Desktop provisioning parses that same file at compile time.
 */

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
export const DENO_RELEASE_MANIFEST_PATH = join(scriptDir, "..", "deno-runtime-manifest.json");

export const SUPPORTED_DENO_TARGETS = Object.freeze([
  "x86_64-pc-windows-msvc",
  "aarch64-pc-windows-msvc",
  "aarch64-apple-darwin",
  "x86_64-apple-darwin",
  "x86_64-unknown-linux-gnu",
]);

const TARGET_PLATFORMS = Object.freeze({
  "x86_64-pc-windows-msvc": "win32",
  "aarch64-pc-windows-msvc": "win32",
  "aarch64-apple-darwin": "darwin",
  "x86_64-apple-darwin": "darwin",
  "x86_64-unknown-linux-gnu": "linux",
});

export const PUBLIC_HELPER_BINARY_STEMS = Object.freeze([
  "pedelec-cli",
  "pedelec-deno",
  "pedelec-agent",
  "pedelec-native-host",
]);
export const RAW_DENO_BINARY_STEM = "deno";
export const RAW_DENO_STAGED_NAMES = Object.freeze(["deno", "deno.exe"]);

const manifest = JSON.parse(readFileSync(DENO_RELEASE_MANIFEST_PATH, "utf8"));
assertValidDenoReleaseManifest(manifest);

export const DENO_VERSION = manifest.version;
export const DENO_RELEASE_TAG = `v${DENO_VERSION}`;
export const DENO_NOTICE_RESOURCE = manifest.noticeResource;
export const DENO_RELEASE_ARTIFACTS = Object.freeze(
  Object.fromEntries(
    Object.entries(manifest.artifacts).map(([target, artifact]) => [
      target,
      Object.freeze({ ...artifact }),
    ]),
  ),
);

export function assertValidDenoReleaseManifest(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("Deno release manifest must be an object.");
  }
  if (!/^\d+\.\d+\.\d+$/.test(value.version) || value.version === "latest") {
    throw new Error(`Deno release version must be a pinned semver, got "${value.version}".`);
  }
  if (value.noticeResource !== "third-party-notices/DENO-LICENSE.txt") {
    throw new Error(`Unexpected Deno notice resource "${value.noticeResource}".`);
  }
  if (value.executableStem !== RAW_DENO_BINARY_STEM) {
    throw new Error(`Unexpected Deno executable stem "${value.executableStem}".`);
  }
  const targets = Object.keys(value.artifacts ?? {});
  if (targets.length !== SUPPORTED_DENO_TARGETS.length ||
    SUPPORTED_DENO_TARGETS.some((target) => !value.artifacts[target])) {
    throw new Error(
      `Deno release manifest must contain exactly: ${SUPPORTED_DENO_TARGETS.join(", ")}.`,
    );
  }
  for (const target of SUPPORTED_DENO_TARGETS) {
    const artifact = value.artifacts[target];
    const expectedPlatform = TARGET_PLATFORMS[target];
    if (!artifact || artifact.platform !== expectedPlatform) {
      throw new Error(
        `Deno target "${target}" must declare platform "${expectedPlatform}".`,
      );
    }
    if (artifact.artifact !== `deno-${target}.zip`) {
      throw new Error(`Unexpected artifact name for ${target}: "${artifact.artifact}".`);
    }
    const expectedUrl =
      `https://github.com/denoland/deno/releases/download/v${value.version}/${artifact.artifact}`;
    if (artifact.url !== expectedUrl) {
      throw new Error(`Deno artifact URL for ${target} does not match the pinned release.`);
    }
    if (!/^[0-9a-f]{64}$/.test(artifact.archiveSha256 ?? "")) {
      throw new Error(`Invalid expected Deno artifact SHA-256 for ${target}.`);
    }
    if (!Number.isInteger(artifact.archiveSizeBytes) || artifact.archiveSizeBytes <= 0) {
      throw new Error(`Deno artifact size for ${target} must be a positive integer.`);
    }
  }
}

function inferredTarget(platform, arch) {
  const targetByPlatform = {
    win32: {
      x64: "x86_64-pc-windows-msvc",
      arm64: "aarch64-pc-windows-msvc",
    },
    darwin: {
      arm64: "aarch64-apple-darwin",
      x64: "x86_64-apple-darwin",
    },
    linux: { x64: "x86_64-unknown-linux-gnu" },
  };
  return targetByPlatform[platform]?.[arch];
}

/**
 * Resolve the exact Rust/Tauri helper target used by the current build.
 * PEDELEC_HELPER_TARGET is authoritative when supplied by release.yml.
 */
export function resolveDenoTarget({
  helperTarget = process.env.PEDELEC_HELPER_TARGET || "",
  platform = process.platform,
  arch = process.arch,
} = {}) {
  const target = helperTarget || inferredTarget(platform, arch);
  if (!target || !DENO_RELEASE_ARTIFACTS[target]) {
    const supplied = helperTarget || `${platform}/${arch}`;
    throw new Error(
      `Unsupported Deno build target "${supplied}". ` +
        `Supported targets: ${SUPPORTED_DENO_TARGETS.join(", ")}`,
    );
  }
  if (DENO_RELEASE_ARTIFACTS[target].platform !== platform) {
    throw new Error(
      `Deno target "${target}" does not match host platform "${platform}".`,
    );
  }
  return target;
}

export function denoArtifactForTarget(target) {
  const metadata = DENO_RELEASE_ARTIFACTS[target];
  if (!metadata) {
    throw new Error(
      `Unsupported Deno build target "${target}". ` +
        `Supported targets: ${SUPPORTED_DENO_TARGETS.join(", ")}`,
    );
  }
  return {
    target,
    ...metadata,
    executable: platformExecutableName(metadata.platform),
  };
}

export function platformExecutableName(platform = process.platform) {
  if (platform !== "win32" && platform !== "darwin" && platform !== "linux") {
    throw new Error(`Unsupported Deno executable platform "${platform}".`);
  }
  return `${RAW_DENO_BINARY_STEM}${platform === "win32" ? ".exe" : ""}`;
}

export function publicHelperBinaryNames(platform = process.platform) {
  const extension = platform === "win32" ? ".exe" : "";
  return PUBLIC_HELPER_BINARY_STEMS.map((stem) => `${stem}${extension}`);
}

export function assertSha256(expected, actual) {
  const normalizedExpected = String(expected).trim().toLowerCase();
  const normalizedActual = String(actual).trim().toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(normalizedExpected)) {
    throw new Error(`Invalid expected Deno artifact SHA-256: "${expected}".`);
  }
  if (normalizedExpected !== normalizedActual) {
    throw new Error(
      `Deno artifact checksum mismatch: expected ${normalizedExpected}, got ${normalizedActual}.`,
    );
  }
}
