import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { mkdirSync, writeFileSync } from "node:fs";

import { publicHelperBinaryNames } from "./deno-release.mjs";
import {
  copyHelperBinaries,
  removeStaleRawDeno,
  stageHelperBinaries,
} from "./stage-tauri-binaries.mjs";

test("release staging builds both runtime binaries from their shared crate on every target", async () => {
  for (const [platform, arch, target] of [
    ["win32", "x64", "x86_64-pc-windows-msvc"],
    ["win32", "arm64", "aarch64-pc-windows-msvc"],
    ["darwin", "arm64", "aarch64-apple-darwin"],
    ["darwin", "x64", "x86_64-apple-darwin"],
    ["linux", "x64", "x86_64-unknown-linux-gnu"],
  ]) {
    const root = await mkdtemp(join(tmpdir(), "pedelec-build-"));
    const resourceDir = join(root, "binaries");
    const helpers = publicHelperBinaryNames(platform);
    try {
      await stageHelperBinaries({
        desktopDir: root, resourceDir, platform, arch, helperTarget: target,
        sign() {},
        run(_program, args) {
          assert.ok(args.includes("--bins"));
          assert.ok(args.includes("pedelec-deno"));
          assert.ok(!args.includes("pedelec-ts-runtime"), "the canonical binary shares the legacy crate");
          const profileDir = join(root, "target", target, "release");
          mkdirSync(profileDir, { recursive: true });
          for (const name of helpers) writeFileSync(join(profileDir, name), `built:${name}`);
        },
      });
      for (const name of helpers) {
        assert.equal(await readFile(join(resourceDir, name), "utf8"), `built:${name}`);
      }
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  }
});

test("staging removes stale raw Deno without removing helper binaries", async () => {
  const directory = await mkdtemp(join(tmpdir(), "pedelec-stage-"));
  try {
    const helpers = publicHelperBinaryNames(process.platform);
    for (const name of helpers) {
      await writeFile(join(directory, name), `helper:${name}`);
    }
    await writeFile(join(directory, "deno"), "stale-deno");
    await writeFile(join(directory, "deno.exe"), "stale-deno-exe");

    await removeStaleRawDeno(directory);

    for (const name of helpers) {
      assert.equal(await readFile(join(directory, name), "utf8"), `helper:${name}`);
    }
    await assert.rejects(readFile(join(directory, "deno")));
    await assert.rejects(readFile(join(directory, "deno.exe")));
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("helper staging copies the public helper binaries", async () => {
  const root = await mkdtemp(join(tmpdir(), "pedelec-helpers-"));
  const profileDir = join(root, "profile");
  const resourceDir = join(root, "binaries");
  try {
    const { mkdir } = await import("node:fs/promises");
    await mkdir(profileDir, { recursive: true });
    await mkdir(resourceDir, { recursive: true });
    const helpers = publicHelperBinaryNames(process.platform);
    for (const name of helpers) {
      await writeFile(join(profileDir, name), `built:${name}`);
    }
    await writeFile(join(resourceDir, "deno.exe"), "must-not-survive");

    await copyHelperBinaries(profileDir, resourceDir, helpers);
    await removeStaleRawDeno(resourceDir);

    for (const name of helpers) {
      assert.equal(await readFile(join(resourceDir, name), "utf8"), `built:${name}`);
    }
    await assert.rejects(readFile(join(resourceDir, "deno.exe")));
    await assert.rejects(readFile(join(resourceDir, "deno")));
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
