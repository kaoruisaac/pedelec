import { execFile } from "node:child_process";
import { access, readdir, readFile, stat } from "node:fs/promises";
import { basename, dirname, join, relative, resolve } from "node:path";
import { promisify } from "node:util";

import { publicHelperBinaryNames, RAW_DENO_STAGED_NAMES } from "./deno-release.mjs";

const execFileAsync = promisify(execFile);

const rawDenoNames = new Set(RAW_DENO_STAGED_NAMES);

export function findRawDenoEntries(relativePaths) {
  return relativePaths.filter((entry) => rawDenoNames.has(entry.split(/[\\/]/).pop()));
}

export function assertNoRawDeno(relativePaths) {
  const hits = findRawDenoEntries(relativePaths);
  if (hits.length > 0) {
    throw new Error(`raw Deno must not be bundled: ${hits.join(", ")}`);
  }
}

export function assertHelpersPresent(requiredNames, relativePaths) {
  const present = new Set(relativePaths.map((entry) => entry.split(/[\\/]/).pop()));
  for (const name of requiredNames) {
    if (!present.has(name)) {
      throw new Error(`missing staged Pedelec helper: ${name}`);
    }
  }
}

export async function listFiles(directory) {
  const root = resolve(directory);
  const files = [];
  async function walk(current) {
    const entries = await readdir(current, { withFileTypes: true });
    for (const entry of entries) {
      const path = join(current, entry.name);
      if (entry.isDirectory()) {
        await walk(path);
      } else if (entry.isFile()) {
        files.push(relative(root, path));
      }
    }
  }
  const info = await stat(root);
  if (!info.isDirectory()) {
    throw new Error(`release path is not a directory: ${root}`);
  }
  await walk(root);
  return files;
}

export async function assertReleaseDirectory(directory, { requireHelpers = false } = {}) {
  const files = await listFiles(directory);
  assertNoRawDeno(files);
  if (requireHelpers) {
    assertHelpersPresent(publicHelperBinaryNames(process.platform), files);
  }
  return files;
}

export function parseSevenZipTechnicalListing(listing) {
  const paths = [];
  for (const line of listing.split(/\r?\n/)) {
    const match = /^Path = (.*)$/.exec(line);
    if (!match) continue;
    const value = match[1].trim();
    if (!value) continue;
    paths.push(value);
  }
  return paths;
}

export function payloadPathsFromSevenZipListing(listing, archivePath) {
  const archiveName = basename(archivePath).toLowerCase();
  const normalizedArchive = archivePath.replaceAll("\\", "/").toLowerCase();
  return parseSevenZipTechnicalListing(listing).filter((entry) => {
    const normalized = entry.replaceAll("\\", "/");
    if (normalized.toLowerCase() === normalizedArchive) return false;
    const name = normalized.split("/").pop()?.toLowerCase();
    if (!name || name === archiveName) return false;
    return true;
  });
}

export function assertWindowsUpdaterPayload(relativePaths) {
  const unpacked = relativePaths.some((entry) => /[\\/]/.test(entry));
  if (!unpacked) {
    throw new Error("Windows updater installer payload could not be listed");
  }
  assertNoRawDeno(relativePaths);
}

const WINDOWS_UPDATER_INSTALLERS = new Set(["nsis", "msi"]);

function windowsUpdaterPlatform(key) {
  if (!key.startsWith("windows-")) return null;
  const remainder = key.slice("windows-".length);
  const splitAt = remainder.lastIndexOf("-");
  if (splitAt > 0) {
    const installer = remainder.slice(splitAt + 1);
    if (WINDOWS_UPDATER_INSTALLERS.has(installer)) {
      const arch = remainder.slice(0, splitAt);
      return arch ? { arch, installer } : null;
    }
  }
  return remainder ? { arch: remainder, installer: null } : null;
}

function updaterArtifactName(key, platform) {
  if (!platform || typeof platform.url !== "string") {
    throw new Error(`latest.json platform ${key} has no updater url`);
  }
  const name = decodeURIComponent(new URL(platform.url).pathname.split("/").pop() ?? "");
  if (!name) {
    throw new Error(`latest.json platform ${key} has no updater url`);
  }
  return name;
}

function isNsisUpdaterArtifact(name) {
  return /nsis/i.test(name) && name.toLowerCase().endsWith(".exe");
}

export function windowsUpdaterArtifactNames(latestJson) {
  const platforms = latestJson?.platforms;
  if (!platforms || typeof platforms !== "object") {
    throw new Error("latest.json has no platforms");
  }

  const architectures = new Map();
  for (const [key, platform] of Object.entries(platforms)) {
    const parsed = windowsUpdaterPlatform(key);
    if (!parsed) continue;
    let entry = architectures.get(parsed.arch);
    if (!entry) {
      entry = { generic: null, nsis: null };
      architectures.set(parsed.arch, entry);
    }
    if (parsed.installer === "msi") continue;
    if (parsed.installer === "nsis") entry.nsis = { key, platform };
    else entry.generic = { key, platform };
  }

  if (architectures.size === 0) {
    throw new Error("latest.json has no generic Windows updater entry");
  }

  const names = [];
  const seen = new Set();
  for (const [arch, entry] of architectures) {
    if (!entry.generic) {
      throw new Error(`latest.json has no generic Windows updater entry for windows-${arch}`);
    }
    const genericName = updaterArtifactName(entry.generic.key, entry.generic.platform);
    if (!isNsisUpdaterArtifact(genericName)) {
      throw new Error(`Windows updater artifact must be the NSIS installer, found ${genericName}`);
    }
    if (entry.nsis) {
      const nsisName = updaterArtifactName(entry.nsis.key, entry.nsis.platform);
      if (nsisName !== genericName) {
        throw new Error(
          `windows-${arch}-nsis must reference the same NSIS installer as windows-${arch}, found ${nsisName}`,
        );
      }
    }
    if (seen.has(genericName)) continue;
    seen.add(genericName);
    names.push(genericName);
  }
  return names;
}

export function assertWindowsUpdaterManifest(latestJson) {
  return windowsUpdaterArtifactNames(latestJson);
}

export async function findLocalWindowsNsisInstaller(bundleDirectory) {
  const nsisDirectory = join(resolve(bundleDirectory), "nsis");
  let entries;
  try {
    entries = await readdir(nsisDirectory, { withFileTypes: true });
  } catch (error) {
    if (error?.code === "ENOENT" || error?.code === "ENOTDIR") {
      throw new Error("local Windows NSIS installer was not found");
    }
    throw error;
  }

  const installers = entries
    .filter((entry) => entry.isFile() && entry.name.toLowerCase().endsWith(".exe"))
    .map((entry) => entry.name)
    .sort();
  if (installers.length === 0) {
    throw new Error("local Windows NSIS installer was not found");
  }
  if (installers.length > 1) {
    throw new Error(`ambiguous local Windows NSIS installers: ${installers.join(", ")}`);
  }
  return join("nsis", installers[0]);
}

async function findLatestJson(bundleDirectory, explicitPath) {
  if (explicitPath) return resolve(explicitPath);
  const starts = [resolve(bundleDirectory), resolve(process.cwd())];
  const seen = new Set();
  for (const start of starts) {
    let current = start;
    for (let depth = 0; depth < 6; depth += 1) {
      if (seen.has(current)) break;
      seen.add(current);
      const candidate = join(current, "latest.json");
      try {
        await access(candidate);
        return candidate;
      } catch {
        // Keep walking toward the repository root.
      }
      const parent = dirname(current);
      if (parent === current) break;
      current = parent;
    }
  }
  throw new Error("latest.json was not found for the Windows updater");
}

async function resolveSevenZip() {
  if (process.env.PEDELEC_7ZIP) return process.env.PEDELEC_7ZIP;
  const fixed = [
    "C:\\Program Files\\7-Zip\\7z.exe",
    "C:\\Program Files (x86)\\7-Zip\\7z.exe",
  ];
  for (const candidate of fixed) {
    try {
      await access(candidate);
      return candidate;
    } catch {
      // Try the next install location.
    }
  }
  try {
    const { stdout } = await execFileAsync("where.exe", ["7z"], { windowsHide: true });
    const found = stdout.split(/\r?\n/).map((line) => line.trim()).find(Boolean);
    if (found) return found;
  } catch {
    // 7-Zip is not on PATH.
  }
  throw new Error("7-Zip is required to inspect the Windows updater installer payload");
}

async function listArchiveWithSevenZip(archivePath) {
  const sevenZip = await resolveSevenZip();
  const { stdout } = await execFileAsync(sevenZip, ["l", "-slt", archivePath], {
    windowsHide: true,
    maxBuffer: 32 * 1024 * 1024,
  });
  return stdout;
}

export async function assertWindowsNsisPayload(localInstallerPath, listArchive) {
  const listing = await (listArchive ?? listArchiveWithSevenZip)(localInstallerPath);
  const payload = payloadPathsFromSevenZipListing(listing, localInstallerPath);
  assertWindowsUpdaterPayload(payload);
  return payload;
}

export async function assertWindowsUpdaterBundle(bundleDirectory, { latestJsonPath, listArchive } = {}) {
  const latestPath = await findLatestJson(bundleDirectory, latestJsonPath);
  const latestJson = JSON.parse(await readFile(latestPath, "utf8"));
  assertWindowsUpdaterManifest(latestJson);
  const relativeInstaller = await findLocalWindowsNsisInstaller(bundleDirectory);
  const archivePath = join(resolve(bundleDirectory), relativeInstaller);
  await assertWindowsNsisPayload(archivePath, listArchive);
  console.log(`Verified Windows updater payload has no raw Deno: ${archivePath}`);
  return relativeInstaller;
}

function usage() {
  return "usage: node scripts/verify-release-bundle.mjs --dir <path> [--require-helpers] [--windows-updater <bundle-dir>] [--latest-json <path>]";
}

function parseArgs(argv) {
  const directories = [];
  const updaterBundles = [];
  let latestJson = null;
  let requireHelpers = false;
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--require-helpers") {
      requireHelpers = true;
      continue;
    }
    if (arg === "--dir" || arg === "--windows-updater" || arg === "--latest-json") {
      const value = argv[index + 1];
      if (!value) throw new Error(usage());
      if (arg === "--dir") directories.push(value);
      else if (arg === "--windows-updater") updaterBundles.push(value);
      else latestJson = value;
      index += 1;
      continue;
    }
    throw new Error(`unknown argument "${arg}"`);
  }
  if (directories.length === 0 && updaterBundles.length === 0) {
    throw new Error(usage());
  }
  return { directories, requireHelpers, updaterBundles, latestJson };
}

const isDirectExecution = process.argv[1]
  && basename(process.argv[1]) === "verify-release-bundle.mjs";

if (isDirectExecution) {
  try {
    const { directories, requireHelpers, updaterBundles, latestJson } = parseArgs(process.argv.slice(2));
    for (const [index, directory] of directories.entries()) {
      await assertReleaseDirectory(directory, {
        requireHelpers: requireHelpers && index === 0,
      });
      console.log(`Verified release path has no raw Deno: ${directory}`);
    }
    for (const bundleDirectory of updaterBundles) {
      await assertWindowsUpdaterBundle(bundleDirectory, { latestJsonPath: latestJson });
    }
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 1;
  }
}
