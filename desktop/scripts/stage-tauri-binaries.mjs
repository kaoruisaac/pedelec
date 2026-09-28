import { spawnSync } from "node:child_process";
import { chmod, copyFile, mkdir, rm, writeFile } from "node:fs/promises";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  publicHelperBinaryNames,
  RAW_DENO_STAGED_NAMES,
  resolveDenoTarget,
} from "./deno-release.mjs";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const desktopDir = join(scriptDir, "..");
const tauriDir = join(desktopDir, "tauri");
const resourceDir = join(tauriDir, "binaries");

function runCommand(command, args, options = {}) {
  const result = spawnSync(command, args, {
    stdio: "inherit",
    ...options,
  });

  if (result.error) {
    throw new Error(
      `Failed to start command: ${command} ${args.join(" ")}\n${result.error.message}`,
    );
  }

  if (result.status !== 0) {
    throw new Error(
      `Command failed with exit code ${result.status}: ${command} ${args.join(" ")}`,
    );
  }
}

export async function removeStaleRawDeno(directory) {
  for (const name of RAW_DENO_STAGED_NAMES) {
    await rm(join(directory, name), { force: true });
  }
}

export async function copyHelperBinaries(profileDir, directory, names) {
  await mkdir(directory, { recursive: true });
  for (const name of names) {
    const destinationPath = join(directory, name);
    await copyFile(join(profileDir, name), destinationPath);
    await makeExecutable(destinationPath);
  }
}

async function makeExecutable(path) {
  if (process.platform !== "win32") {
    await chmod(path, 0o755);
  }
}

function signExecutable(path, label) {
  const signingIdentity = process.env.APPLE_SIGNING_IDENTITY || "";
  if (process.platform !== "darwin") {
    return;
  }

  if (signingIdentity) {
    console.log(`Signing ${label} with Developer ID signing.`);
    runCommand("codesign", [
      "--force",
      "--timestamp",
      "--options",
      "runtime",
      "--sign",
      signingIdentity,
      path,
    ]);
    runCommand("codesign", ["--verify", "--strict", "--verbose=2", path]);

    const signingInfo = spawnSync("codesign", ["-dv", "--verbose=4", path], {
      encoding: "utf8",
    });
    const signingOutput = `${signingInfo.stdout}${signingInfo.stderr}`;
    if (
      signingInfo.status !== 0 ||
      !signingOutput.includes("Authority=Developer ID Application:") ||
      !/flags=.*\bruntime\b/.test(signingOutput)
    ) {
      throw new Error(
        `Developer ID or Hardened Runtime verification failed for ${label}.`,
      );
    }
  } else {
    console.log(`Signing ${label} with ad-hoc fallback for local development.`);
    runCommand("codesign", ["--force", "--sign", "-", path]);
    runCommand("codesign", ["--verify", "--verbose=2", path]);
  }
}

export async function stageHelperBinaries({
  desktopDir,
  resourceDir,
  helperTarget = process.env.PEDELEC_HELPER_TARGET || "",
  platform = process.platform,
  arch = process.arch,
  run = runCommand,
}) {
  resolveDenoTarget({ helperTarget, platform, arch });
  const helperBinaryNames = publicHelperBinaryNames(platform);
  const cargoArgs = [
    "build",
    "--manifest-path",
    join(desktopDir, "Cargo.toml"),
    "--release",
    ...helperBinaryNames.flatMap((name) => ["--package", name.replace(/\.exe$/, "")]),
  ];
  if (helperTarget) {
    cargoArgs.push("--target", helperTarget);
  }

  await mkdir(resourceDir, { recursive: true });
  await removeStaleRawDeno(resourceDir);
  const placeholderPath = join(resourceDir, ".placeholder");
  await writeFile(placeholderPath, "");
  try {
    run(platform === "win32" ? "cargo.exe" : "cargo", cargoArgs, {
      cwd: desktopDir,
    });

    const profileDir = helperTarget
      ? join(desktopDir, "target", helperTarget, "release")
      : join(desktopDir, "target", "release");
    await copyHelperBinaries(profileDir, resourceDir, helperBinaryNames);
    for (const name of helperBinaryNames) {
      signExecutable(join(resourceDir, name), name);
    }
    await removeStaleRawDeno(resourceDir);
  } finally {
    await rm(placeholderPath, { force: true });
  }
}

async function stage() {
  await stageHelperBinaries({
    desktopDir,
    resourceDir,
    helperTarget: process.env.PEDELEC_HELPER_TARGET || "",
    platform: process.platform,
    arch: process.arch,
  });
  console.log(`Staged Pedelec helper binaries in ${resourceDir}.`);
}

const isDirectExecution = process.argv[1]
  && basename(process.argv[1]) === "stage-tauri-binaries.mjs";

if (isDirectExecution) {
  try {
    await stage();
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 1;
  }
}
