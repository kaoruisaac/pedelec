import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtemp, mkdir, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

import {
  DENO_VERSION,
  platformExecutableName,
} from "./deno-release.mjs";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const desktopDir = resolve(scriptDir, "..");
const defaultBinary = join(
  desktopDir,
  "tauri",
  "binaries",
  platformExecutableName(process.platform),
);

function parseBinaryArg(argv) {
  if (argv.length === 0) {
    return defaultBinary;
  }
  if (argv.length === 2 && argv[0] === "--binary") {
    return resolve(argv[1]);
  }
  throw new Error("usage: node scripts/verify-deno-runtime.mjs [--binary <path>]");
}

function run(binary, workspace, script, scriptArgs = [], expectedExitCode = 0) {
  const cacheDir = join(workspace, ".pedelec-runtime", "deno");
  const result = spawnSync(
    binary,
    [
      "run",
      "--no-prompt",
      "--no-config",
      "--no-remote",
      "--cached-only",
      "--no-npm",
      `--allow-read=${workspace}`,
      `--allow-write=${workspace}`,
      "--deny-net",
      "--deny-env",
      "--deny-run",
      "--deny-ffi",
      "--deny-sys",
      "--",
      script,
      ...scriptArgs,
    ],
    {
      cwd: workspace,
      env: {
        ...process.env,
        DENO_DIR: cacheDir,
        DENO_NO_UPDATE_CHECK: "1",
        NO_COLOR: "1",
      },
      encoding: "utf8",
      timeout: 15_000,
      windowsHide: true,
    },
  );
  if (result.error) {
    throw new Error(`Deno invocation failed: ${result.error.message}`);
  }
  assert.equal(
    result.status,
    expectedExitCode,
    `Unexpected Deno exit code.\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
  );
  return result;
}

function verifyVersion(binary) {
  const result = spawnSync(binary, ["--version"], {
    encoding: "utf8",
    timeout: 10_000,
    windowsHide: true,
  });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, new RegExp(`^deno ${DENO_VERSION.replaceAll(".", "\\.")}\\b`, "m"));
}

async function verifyRuntime(binary) {
  verifyVersion(binary);
  const createdRoot = await mkdtemp(join(tmpdir(), "pedelec-deno-smoke-"));
  await mkdir(join(createdRoot, "workspace"), { recursive: true });
  // Temporary roots can be exposed through a filesystem alias (on macOS
  // `/var` while the real path is `/private/var`).  Deno compares permission
  // prefixes against canonical paths, so the whole smoke test - permission
  // flags, cwd, script paths and output assertions - must use one canonical
  // workspace root.
  const root = await realpath(createdRoot);
  const workspace = await realpath(join(root, "workspace"));
  const outside = join(root, "outside.txt");
  await writeFile(join(workspace, "input.txt"), "workspace input\n");
  await writeFile(outside, "outside input\n");
  await writeFile(
    join(workspace, "local.ts"),
    'export const localMarker: string = "local-import-ok";\n',
  );
  await writeFile(
    join(workspace, "basic.js"),
    'console.log(`js-ok:${Deno.args.join("|")}`);\n',
  );
  await writeFile(
    join(workspace, "workspace.ts"),
    [
      'import { localMarker } from "./local.ts";',
      'const input = await Deno.readTextFile("input.txt");',
      'await Deno.writeTextFile("output.txt", input.toUpperCase());',
      'console.log(`${localMarker}:${input.trim()}:${Deno.args.join("|")}`);',
    ].join("\n"),
  );
  await writeFile(
    join(workspace, "denied.ts"),
    [
      `const outside = ${JSON.stringify(outside)};`,
      'const denied = async (action) => { try { await action(); return false; } catch { return true; } };',
      "const results = {",
      '  readOutside: await denied(() => Deno.readTextFile(outside)),',
      '  writeOutside: await denied(() => Deno.writeTextFile(outside, "blocked")),',
      '  network: await denied(() => fetch("http://127.0.0.1:9")),',
      '  env: await denied(() => Deno.env.get("PATH")),',
      '  process: await denied(() => new Deno.Command(Deno.execPath()).output()),',
      '  ffi: await denied(() => Deno.dlopen(outside, {})),',
      '  sys: await denied(() => Deno.systemMemoryInfo()),',
      "};",
      'if (!Object.values(results).every(Boolean)) { console.error(JSON.stringify(results)); Deno.exit(1); }',
      'console.log("denied-capabilities-ok");',
    ].join("\n"),
  );
  await writeFile(
    join(workspace, "remote.ts"),
    'import "https://example.com/pedelec-deno-smoke-never-fetch.ts";\n',
  );

  try {
    const js = run(binary, workspace, join(workspace, "basic.js"), ["--allow-all"]);
    assert.match(js.stdout, /js-ok:--allow-all/);

    const ts = run(binary, workspace, join(workspace, "workspace.ts"), ["--", "value"]);
    assert.match(ts.stdout, /local-import-ok:workspace input:--\|value/);
    assert.equal(await readFile(join(workspace, "output.txt"), "utf8"), "WORKSPACE INPUT\n");

    const denied = run(binary, workspace, join(workspace, "denied.ts"));
    assert.match(denied.stdout, /denied-capabilities-ok/);

    run(binary, workspace, join(workspace, "remote.ts"), [], 1);
  } finally {
    await rm(createdRoot, { recursive: true, force: true });
  }
}

try {
  await verifyRuntime(parseBinaryArg(process.argv.slice(2)));
  console.log(`Verified Deno ${DENO_VERSION} runtime policy and local JS/TS execution.`);
} catch (error) {
  console.error(error instanceof Error ? error.stack || error.message : error);
  process.exitCode = 1;
}
