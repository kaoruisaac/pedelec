import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { publicHelperBinaryNames } from "./deno-release.mjs";
import {
  copyHelperBinaries,
  removeStaleRawDeno,
} from "./stage-tauri-binaries.mjs";

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
