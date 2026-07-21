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
const helperBinaryNames = [
  "pedelec-cli",
  "pedelec-agent",
  "pedelec-native-host",
];

function runCommand(command, args, options = {}) {
  const result = spawnSync(command, args, {
    stdio: "inherit",
    ...options,
  });

  if (result.error) {
    console.error(`Failed to start command: ${command} ${args.join(" ")}`);
    console.error(result.error.message);
    process.exit(1);
  }

  if (result.status !== 0) {
    console.error(
      `Command failed with exit code ${result.status}: ` +
        `${command} ${args.join(" ")}`,
    );
    process.exit(result.status ?? 1);
  }
}

const cargoArgs = [
  "build",
  "--release",
  ...helperBinaryNames.flatMap((name) => ["--bin", name]),
];

if (helperTarget) {
  cargoArgs.push("--target", helperTarget);
}

await mkdir(resourceDir, { recursive: true });
await writeFile(placeholderPath, "");

runCommand(process.platform === "win32" ? "cargo.exe" : "cargo", cargoArgs, {
  cwd: tauriDir,
});

const exe = process.platform === "win32" ? ".exe" : "";
const profileDir = helperTarget
  ? join(tauriDir, "target", helperTarget, "release")
  : join(tauriDir, "target", "release");

for (const name of helperBinaryNames) {
  const sourcePath = join(profileDir, `${name}${exe}`);
  const destinationPath = join(resourceDir, `${name}${exe}`);

  await copyFile(sourcePath, destinationPath);

  if (process.platform === "darwin") {
    runCommand("codesign", ["--force", "--sign", "-", destinationPath]);
    runCommand("codesign", ["--verify", "--verbose=2", destinationPath]);
  }
}

await rm(placeholderPath, { force: true });
