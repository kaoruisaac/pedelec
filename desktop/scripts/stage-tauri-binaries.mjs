import { copyFile, mkdir, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const desktopDir = join(scriptDir, "..");
const tauriDir = join(desktopDir, "tauri");
const resourceDir = join(tauriDir, "binaries");
const placeholderPath = join(resourceDir, ".placeholder");
const helperTarget = process.env.PEDELEC_HELPER_TARGET || "";
const cargoArgs = [
  "build",
  "--release",
  "--bin",
  "pedelec-cli",
  "--bin",
  "pedelec-native-host",
];

if (helperTarget) {
  cargoArgs.push("--target", helperTarget);
}

await mkdir(resourceDir, { recursive: true });
await writeFile(placeholderPath, "");

const cargo = spawnSync(process.platform === "win32" ? "cargo.exe" : "cargo", cargoArgs, {
  cwd: tauriDir,
  stdio: "inherit",
});

if (cargo.error) {
  console.error(cargo.error.message);
  process.exit(1);
}

if (cargo.status !== 0) {
  process.exit(cargo.status ?? 1);
}

const exe = process.platform === "win32" ? ".exe" : "";
const profileDir = helperTarget
  ? join(tauriDir, "target", helperTarget, "release")
  : join(tauriDir, "target", "release");

for (const name of ["pedelec-cli", "pedelec-native-host"]) {
  await copyFile(join(profileDir, `${name}${exe}`), join(resourceDir, `${name}${exe}`));
}

await rm(placeholderPath, { force: true });
