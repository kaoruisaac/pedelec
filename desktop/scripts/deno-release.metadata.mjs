import { createHash } from "node:crypto";
import test from "node:test";
import assert from "node:assert/strict";

import {
  DENO_NOTICE_RESOURCE,
  DENO_RELEASE_ARTIFACTS,
  DENO_VERSION,
  PUBLIC_HELPER_BINARY_STEMS,
  RAW_DENO_BINARY_STEM,
  SUPPORTED_DENO_TARGETS,
  assertSha256,
  assertValidDenoReleaseManifest,
  denoArtifactForTarget,
  platformExecutableName,
  publicHelperBinaryNames,
  resolveDenoTarget,
} from "./deno-release.mjs";

test("Deno release metadata is pinned to a concrete version", () => {
  assert.match(DENO_VERSION, /^\d+\.\d+\.\d+$/);
  assert.notEqual(DENO_VERSION, "latest");
  assert.equal(RAW_DENO_BINARY_STEM, "deno");
  assert.equal(DENO_NOTICE_RESOURCE, "third-party-notices/DENO-LICENSE.txt");
});

test("release targets map to the runtime CDN, official fallback, and checksums", () => {
  const targets = [
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
  ];
  for (const target of targets) {
    const metadata = denoArtifactForTarget(target);
    assert.equal(metadata.url, undefined);
    assert.match(metadata.primaryUrl, new RegExp(`/v${DENO_VERSION}/`));
    assert.match(metadata.fallbackUrl, new RegExp(`/v${DENO_VERSION}/`));
    assert.match(metadata.archiveSha256, /^[0-9a-f]{64}$/);
    assert.equal(typeof metadata.archiveSizeBytes, "number");
    assert.ok(Number.isInteger(metadata.archiveSizeBytes));
    assert.ok(metadata.archiveSizeBytes > 0);
    assert.equal(
      metadata.primaryUrl,
      `https://runtime.pedelec.cc/deno/v${DENO_VERSION}/${metadata.artifact}`,
    );
    assert.equal(
      metadata.fallbackUrl,
      `https://github.com/denoland/deno/releases/download/v${DENO_VERSION}/${metadata.artifact}`,
    );
    assert.notEqual(metadata.primaryUrl, metadata.fallbackUrl);
    assert.ok(metadata.artifact.endsWith(".zip"));
    assert.ok(DENO_RELEASE_ARTIFACTS[target]);
  }
  assert.deepEqual(Object.keys(DENO_RELEASE_ARTIFACTS).sort(), [...SUPPORTED_DENO_TARGETS].sort());
});

test("manifest validation rejects unpinned or incomplete metadata", () => {
  const base = {
    version: DENO_VERSION,
    noticeResource: DENO_NOTICE_RESOURCE,
    executableStem: RAW_DENO_BINARY_STEM,
    artifacts: Object.fromEntries(
      SUPPORTED_DENO_TARGETS.map((target) => {
        const artifact = denoArtifactForTarget(target);
        return [target, {
          platform: artifact.platform,
          artifact: artifact.artifact,
          primaryUrl: artifact.primaryUrl,
          fallbackUrl: artifact.fallbackUrl,
          archiveSha256: artifact.archiveSha256,
          archiveSizeBytes: artifact.archiveSizeBytes,
        }];
      }),
    ),
  };
  assert.doesNotThrow(() => assertValidDenoReleaseManifest(structuredClone(base)));
  assert.throws(
    () => assertValidDenoReleaseManifest({ ...base, version: "latest" }),
    /pinned semver/,
  );
  const badSha = structuredClone(base);
  badSha.artifacts["x86_64-pc-windows-msvc"].archiveSha256 = "zz";
  assert.throws(() => assertValidDenoReleaseManifest(badSha), /SHA-256/);
  const badSize = structuredClone(base);
  badSize.artifacts["aarch64-apple-darwin"].archiveSizeBytes = 0;
  assert.throws(() => assertValidDenoReleaseManifest(badSize), /positive integer/);
  const badPlatform = structuredClone(base);
  badPlatform.artifacts["x86_64-unknown-linux-gnu"].platform = "win32";
  assert.throws(() => assertValidDenoReleaseManifest(badPlatform), /platform/);
  const missing = structuredClone(base);
  delete missing.artifacts["x86_64-apple-darwin"];
  assert.throws(() => assertValidDenoReleaseManifest(missing), /exactly/);
  const badPrimary = structuredClone(base);
  badPrimary.artifacts["x86_64-pc-windows-msvc"].primaryUrl =
    "https://example.invalid/deno/v2.9.5/deno-x86_64-pc-windows-msvc.zip";
  assert.throws(() => assertValidDenoReleaseManifest(badPrimary), /primary URL/);
  const badPrimaryVersion = structuredClone(base);
  badPrimaryVersion.artifacts["aarch64-pc-windows-msvc"].primaryUrl =
    `https://runtime.pedelec.cc/deno/v9.9.9/${badPrimaryVersion.artifacts["aarch64-pc-windows-msvc"].artifact}`;
  assert.throws(() => assertValidDenoReleaseManifest(badPrimaryVersion), /primary URL/);
  const badFallback = structuredClone(base);
  badFallback.artifacts["aarch64-apple-darwin"].fallbackUrl =
    "https://runtime.pedelec.cc/deno/v2.9.5/deno-aarch64-apple-darwin.zip";
  assert.throws(() => assertValidDenoReleaseManifest(badFallback), /fallback URL/);
  const badFallbackArtifact = structuredClone(base);
  badFallbackArtifact.artifacts["x86_64-unknown-linux-gnu"].fallbackUrl =
    "https://github.com/denoland/deno/releases/download/v2.9.5/deno-other.zip";
  assert.throws(() => assertValidDenoReleaseManifest(badFallbackArtifact), /fallback URL/);
  const legacyUrl = structuredClone(base);
  legacyUrl.artifacts["x86_64-apple-darwin"].url =
    legacyUrl.artifacts["x86_64-apple-darwin"].fallbackUrl;
  assert.throws(() => assertValidDenoReleaseManifest(legacyUrl), /primaryUrl and fallbackUrl/);
});

test("the release matrix uses the supplied helper target", () => {
  assert.equal(
    resolveDenoTarget({
      helperTarget: "aarch64-apple-darwin",
      platform: "darwin",
      arch: "x64",
    }),
    "aarch64-apple-darwin",
  );
  assert.equal(
    resolveDenoTarget({ helperTarget: "", platform: "linux", arch: "x64" }),
    "x86_64-unknown-linux-gnu",
  );
});

test("unsupported targets fail explicitly", () => {
  assert.throws(
    () =>
      resolveDenoTarget({
        helperTarget: "riscv64gc-unknown-linux-gnu",
        platform: "linux",
        arch: "riscv64",
      }),
    /Unsupported Deno build target/,
  );
  assert.throws(
    () =>
      resolveDenoTarget({ helperTarget: "", platform: "freebsd", arch: "x64" }),
    /Unsupported Deno build target/,
  );
});

test("checksum verification fails on mismatch", () => {
  const digest = createHash("sha256").update("pedelec").digest("hex");
  assert.doesNotThrow(() => assertSha256(digest, digest.toUpperCase()));
  assert.throws(
    () => assertSha256(digest, "0".repeat(64)),
    /Deno artifact checksum mismatch/,
  );
});

test("public helper names contain pedelec-deno but never raw deno", () => {
  assert.deepEqual(publicHelperBinaryNames("win32"), [
    "pedelec-cli.exe",
    "pedelec-deno.exe",
    "pedelec-agent.exe",
    "pedelec-native-host.exe",
  ]);
  assert.ok(PUBLIC_HELPER_BINARY_STEMS.includes("pedelec-deno"));
  assert.ok(!PUBLIC_HELPER_BINARY_STEMS.includes(RAW_DENO_BINARY_STEM));
  assert.equal(platformExecutableName("win32"), "deno.exe");
});

