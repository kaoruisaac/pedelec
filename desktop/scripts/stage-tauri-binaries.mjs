import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { chmod, copyFile, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { inflateRawSync } from "node:zlib";

import {
  assertSha256,
  denoArtifactForTarget,
  platformExecutableName,
  publicHelperBinaryNames,
  resolveDenoTarget,
} from "./deno-release.mjs";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const desktopDir = join(scriptDir, "..");
const tauriDir = join(desktopDir, "tauri");
const resourceDir = join(tauriDir, "binaries");
const placeholderPath = join(resourceDir, ".placeholder");
const helperTarget = process.env.PEDELEC_HELPER_TARGET || "";
const signingIdentity = process.env.APPLE_SIGNING_IDENTITY || "";

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

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

async function acquireVerifiedArchive(metadata) {
  let bytes;
  const localArchive = process.env.PEDELEC_DENO_ARCHIVE;
  if (localArchive) {
    console.log(`Using PEDELEC_DENO_ARCHIVE: ${localArchive}`);
    bytes = await readFile(localArchive);
  } else {
    console.log(`Downloading pinned Deno ${metadata.target} artifact.`);
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 120_000);
    try {
      const response = await fetch(metadata.url, {
        signal: controller.signal,
        headers: { "user-agent": "pedelec-tauri-stager" },
      });
      if (!response.ok) {
        throw new Error(`HTTP ${response.status} ${response.statusText}`);
      }
      bytes = Buffer.from(await response.arrayBuffer());
    } catch (error) {
      throw new Error(`Could not acquire Deno artifact ${metadata.url}: ${error}`);
    } finally {
      clearTimeout(timeout);
    }
  }

  const actualSha256 = sha256(bytes);
  assertSha256(metadata.archiveSha256, actualSha256);
  return bytes;
}

function readZipEntry(archive, expectedName) {
  const minimumEndRecordSize = 22;
  const maximumCommentSize = 0xffff;
  const searchStart = Math.max(
    0,
    archive.length - minimumEndRecordSize - maximumCommentSize,
  );
  let endRecordOffset = -1;
  for (let offset = archive.length - minimumEndRecordSize; offset >= searchStart; offset -= 1) {
    if (archive.readUInt32LE(offset) === 0x06054b50) {
      endRecordOffset = offset;
      break;
    }
  }
  if (endRecordOffset < 0) {
    throw new Error("Deno artifact is not a supported ZIP archive.");
  }

  const centralDirectorySize = archive.readUInt32LE(endRecordOffset + 12);
  const centralDirectoryOffset = archive.readUInt32LE(endRecordOffset + 16);
  const centralDirectoryEnd = centralDirectoryOffset + centralDirectorySize;
  if (centralDirectoryEnd > archive.length) {
    throw new Error("Deno artifact ZIP central directory is truncated.");
  }

  const entries = [];
  let cursor = centralDirectoryOffset;
  while (cursor < centralDirectoryEnd) {
    if (archive.readUInt32LE(cursor) !== 0x02014b50) {
      throw new Error("Deno artifact ZIP has an invalid central directory entry.");
    }
    const flags = archive.readUInt16LE(cursor + 8);
    const compressionMethod = archive.readUInt16LE(cursor + 10);
    const compressedSize = archive.readUInt32LE(cursor + 20);
    const uncompressedSize = archive.readUInt32LE(cursor + 24);
    const nameLength = archive.readUInt16LE(cursor + 28);
    const extraLength = archive.readUInt16LE(cursor + 30);
    const commentLength = archive.readUInt16LE(cursor + 32);
    const localHeaderOffset = archive.readUInt32LE(cursor + 42);
    const nameStart = cursor + 46;
    const nameEnd = nameStart + nameLength;
    const entryEnd = nameEnd + extraLength + commentLength;
    if (entryEnd > centralDirectoryEnd) {
      throw new Error("Deno artifact ZIP central directory entry is truncated.");
    }
    entries.push({
      flags,
      compressionMethod,
      compressedSize,
      uncompressedSize,
      localHeaderOffset,
      name: archive.subarray(nameStart, nameEnd).toString("utf8"),
    });
    cursor = entryEnd;
  }

  const matches = entries.filter((entry) => entry.name === expectedName);
  if (matches.length !== 1) {
    throw new Error(
      `Deno artifact must contain exactly one root-level ${expectedName} executable; found ${matches.length}.`,
    );
  }
  const entry = matches[0];
  if ((entry.flags & 0x1) !== 0) {
    throw new Error("Encrypted Deno artifacts are not supported.");
  }
  if (entry.compressionMethod !== 0 && entry.compressionMethod !== 8) {
    throw new Error(
      `Unsupported compression method ${entry.compressionMethod} for ${expectedName}.`,
    );
  }

  const localHeaderOffset = entry.localHeaderOffset;
  if (
    localHeaderOffset + 30 > archive.length ||
    archive.readUInt32LE(localHeaderOffset) !== 0x04034b50
  ) {
    throw new Error(`Deno artifact local header is invalid for ${expectedName}.`);
  }
  const localNameLength = archive.readUInt16LE(localHeaderOffset + 26);
  const localExtraLength = archive.readUInt16LE(localHeaderOffset + 28);
  const dataStart = localHeaderOffset + 30 + localNameLength + localExtraLength;
  const dataEnd = dataStart + entry.compressedSize;
  if (dataEnd > archive.length) {
    throw new Error(`Deno artifact data is truncated for ${expectedName}.`);
  }

  const compressed = archive.subarray(dataStart, dataEnd);
  const extracted =
    entry.compressionMethod === 0 ? compressed : inflateRawSync(compressed);
  if (extracted.length !== entry.uncompressedSize) {
    throw new Error(`Deno artifact size verification failed for ${expectedName}.`);
  }
  return extracted;
}

async function makeExecutable(path) {
  if (process.platform !== "win32") {
    await chmod(path, 0o755);
  }
}

function signExecutable(path, label) {
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

async function stage() {
  const target = resolveDenoTarget({
    helperTarget,
    platform: process.platform,
    arch: process.arch,
  });
  const denoMetadata = denoArtifactForTarget(target);
  const helperBinaryNames = publicHelperBinaryNames(process.platform);
  const rawDenoName = platformExecutableName(process.platform);

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
  await writeFile(placeholderPath, "");
  try {
    runCommand(process.platform === "win32" ? "cargo.exe" : "cargo", cargoArgs, {
      cwd: desktopDir,
    });

    const profileDir = helperTarget
      ? join(desktopDir, "target", helperTarget, "release")
      : join(desktopDir, "target", "release");

    for (const name of helperBinaryNames) {
      const sourcePath = join(profileDir, name);
      const destinationPath = join(resourceDir, name);
      await copyFile(sourcePath, destinationPath);
      await makeExecutable(destinationPath);
      signExecutable(destinationPath, name);
    }

    const archive = await acquireVerifiedArchive(denoMetadata);
    const rawDenoBytes = readZipEntry(archive, denoMetadata.executable);
    const rawDenoPath = join(resourceDir, rawDenoName);
    await writeFile(rawDenoPath, rawDenoBytes);
    await makeExecutable(rawDenoPath);
    signExecutable(rawDenoPath, rawDenoName);

    console.log(
      `Staged Pedelec helpers and Deno ${denoMetadata.target} ${rawDenoName} in ${resourceDir}.`,
    );
  } finally {
    await rm(placeholderPath, { force: true });
  }
}

try {
  await stage();
} catch (error) {
  console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
}
