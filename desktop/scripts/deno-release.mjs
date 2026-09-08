/**
 * Pinned Deno release metadata used by build/staging and release verification.
 *
 * Keep this file free of runtime provisioning logic: the Desktop application
 * receives the raw executable as a Tauri resource and never downloads it.
 */

export const DENO_VERSION = "2.9.5";
export const DENO_RELEASE_TAG = `v${DENO_VERSION}`;
export const DENO_RELEASE_BASE_URL =
  `https://github.com/denoland/deno/releases/download/${DENO_RELEASE_TAG}`;

const DENO_ARTIFACTS = Object.freeze({
  "x86_64-pc-windows-msvc": Object.freeze({
    platform: "win32",
    artifact: "deno-x86_64-pc-windows-msvc.zip",
    archiveSha256:
      "171efab55ac6b9881fd53ee4c20f8bf3bb1340ffc618483746909014db12216a",
  }),
  "aarch64-pc-windows-msvc": Object.freeze({
    platform: "win32",
    artifact: "deno-aarch64-pc-windows-msvc.zip",
    archiveSha256:
      "73f20b3566a0a6e3f6912fd7bf5b3a7ccd04d68414baedea3b397437bdec6472",
  }),
  "aarch64-apple-darwin": Object.freeze({
    platform: "darwin",
    artifact: "deno-aarch64-apple-darwin.zip",
    archiveSha256:
      "b796aadd131f6930560c1ee040cf0d6f53933fbb987464e9ff46bd7ea4830615",
  }),
  "x86_64-apple-darwin": Object.freeze({
    platform: "darwin",
    artifact: "deno-x86_64-apple-darwin.zip",
    archiveSha256:
      "c1b8b89a81e91b2a8b3f96def3195d08cfe3a105651da7908d53061f7140510d",
  }),
  "x86_64-unknown-linux-gnu": Object.freeze({
    platform: "linux",
    artifact: "deno-x86_64-unknown-linux-gnu.zip",
    archiveSha256:
      "8b010a3b1a4a0188a67cdb8a7a27348b2a501af78aec7fc74f2ace167368d530",
  }),
});

export const DENO_RELEASE_ARTIFACTS = DENO_ARTIFACTS;
export const PUBLIC_HELPER_BINARY_STEMS = Object.freeze([
  "pedelec-cli",
  "pedelec-deno",
  "pedelec-agent",
  "pedelec-native-host",
]);
export const RAW_DENO_BINARY_STEM = "deno";
export const DENO_NOTICE_RESOURCE = "third-party-notices/DENO-LICENSE.txt";

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
  if (!target || !DENO_ARTIFACTS[target]) {
    const supplied = helperTarget || `${platform}/${arch}`;
    throw new Error(
      `Unsupported Deno build target "${supplied}". ` +
        `Supported targets: ${Object.keys(DENO_ARTIFACTS).join(", ")}`,
    );
  }
  if (DENO_ARTIFACTS[target].platform !== platform) {
    throw new Error(
      `Deno target "${target}" does not match host platform "${platform}".`,
    );
  }
  return target;
}

export function denoArtifactForTarget(target) {
  const metadata = DENO_ARTIFACTS[target];
  if (!metadata) {
    throw new Error(
      `Unsupported Deno build target "${target}". ` +
        `Supported targets: ${Object.keys(DENO_ARTIFACTS).join(", ")}`,
    );
  }
  return {
    target,
    ...metadata,
    url: `${DENO_RELEASE_BASE_URL}/${metadata.artifact}`,
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
