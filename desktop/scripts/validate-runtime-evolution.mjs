import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { resolve } from "node:path";

// Run from any directory. Continue after failures so the report shows each
// component independently, but never return success for an incomplete run.
const root = fileURLToPath(new URL("../../", import.meta.url));
const real = process.argv.includes("--real");
if (process.argv.slice(2).some((arg) => arg !== "--real")) {
  throw new Error("Usage: node desktop/scripts/validate-runtime-evolution.mjs [--real]");
}
if (real && (!process.env.PEDELEC_TEST_DENO || !existsSync(process.env.PEDELEC_TEST_DENO))) {
  throw new Error("--real requires PEDELEC_TEST_DENO pointing to the pinned managed Deno executable; real execution must not silently skip.");
}
if (real) {
  const smoke = spawnSync(process.execPath, [
    resolve(root, "desktop/scripts/verify-deno-runtime.mjs"), "--binary", process.env.PEDELEC_TEST_DENO,
  ], { stdio: "inherit", windowsHide: true });
  if (smoke.status !== 0) throw new Error("Pinned managed-runtime Sandbox smoke failed; acceptance cannot continue.");
}
const checks = [
  ["SDK tests (includes SDK/Extension integration)", "sdk", "npm run test"],
  ["SDK type compatibility", "sdk", "npm run type-test"],
  ["SDK build", "sdk", "npm run build"],
  ["Extension grants and Popup", ".", "node --test extension/background.test.js extension/localization.test.js extension/popup.test.js"],
  ...["core", "ipc", "runtime", "deno", "agent", "native-host", "cli", "shared"].map((name) =>
    [`Rust ${name}`, "desktop", `cargo test -p pedelec-${name}`]),
  ["Release metadata and cross-platform helper staging", "desktop", "npm run test:release-metadata"],
  ["Documentation build", "dev-docs", "npm run build"],
  ["Demo types", "demo/solid-sdk-demo", "npm run typecheck"],
  ["Demo build", "demo/solid-sdk-demo", "npm run build"],
];
const results = [];
for (const [label, dir, command] of checks) {
  console.log(`\n${label}: ${command}`);
  // Commands are a fixed allowlist above; no user text is interpolated.
  const result = spawnSync(command, { cwd: resolve(root, dir), shell: true, stdio: "inherit", windowsHide: true });
  results.push({ check: label, result: result.status === 0 ? "PASS" : "FAIL" });
  if (result.error) console.error(result.error.message);
}
console.table(results);
console.log(real
  ? "Real managed-runtime fixtures enabled. Live Chrome/Native and installed release checks remain manual."
  : "Real execution is conditional on an available managed runtime. Use --real with PEDELEC_TEST_DENO for required execution. Live Chrome/Native and installed release checks remain manual.");
process.exitCode = results.some((result) => result.result === "FAIL") ? 1 : 0;
