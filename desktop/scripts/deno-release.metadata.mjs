import { createHash } from "node:crypto";
import test from "node:test";
import assert from "node:assert/strict";

import {
  DENO_NOTICE_RESOURCE,
  DENO_RELEASE_ARTIFACTS,
  DENO_VERSION,
  PUBLIC_HELPER_BINARY_STEMS,
  RAW_DENO_BINARY_STEM,
  assertSha256,
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

test("release targets map to exact official artifacts and checksums", () => {
  const targets = [
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
  ];
  for (const target of targets) {
    const metadata = denoArtifactForTarget(target);
    assert.match(metadata.url, new RegExp(`/v${DENO_VERSION}/`));
    assert.match(metadata.archiveSha256, /^[0-9a-f]{64}$/);
    assert.ok(metadata.artifact.endsWith(".zip"));
    assert.ok(DENO_RELEASE_ARTIFACTS[target]);
  }
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

