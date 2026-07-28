import { copyFile, cp, mkdir, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const desktopDir = resolve(scriptDir, "..");
const tauriDir = join(desktopDir, "tauri");
const targetDir = join(desktopDir, "target");
const releaseDir = join(targetDir, "release");
const storeBundleDir = join(releaseDir, "bundle", "msix-store");
const storeStageRoot = join(releaseDir, "msix-store");

const packageIdentity = {
  name: "IsaacLin.Pedelec",
  publisher: "CN=3454FF61-E514-4B82-96E1-91D9913D0394",
  publisherDisplayName: "Isaac Lin",
  displayName: "Pedelec",
  applicationId: "Pedelec",
};

const architectures = [
  {
    msixArch: "x64",
    rustTarget: "x86_64-pc-windows-msvc",
    windowsKitArch: "x64",
  },
  {
    msixArch: "arm64",
    rustTarget: "aarch64-pc-windows-msvc",
    windowsKitArch: "arm64",
  },
];

if (process.platform !== "win32") {
  console.error("npm run pack is only supported on Windows.");
  process.exit(1);
}

const packageJson = JSON.parse(await readFile(join(desktopDir, "package.json"), "utf8"));
const packageVersion = toMsixVersion(packageJson.version);
const makeAppx = await findLatestWindowsSdkTool("makeappx.exe", "x64");

assertRequiredToolchain();

console.log(`Using MakeAppx: ${makeAppx}`);
console.log(`Packaging ${packageIdentity.displayName} ${packageVersion}`);

await rm(storeBundleDir, { recursive: true, force: true });
await rm(storeStageRoot, { recursive: true, force: true });
await mkdir(storeBundleDir, { recursive: true });

const msixFiles = [];

for (const architecture of architectures) {
  await buildArchitecture(architecture);
  const packageStageDir = await stageArchitecture(architecture);
  const msixPath = join(
    storeBundleDir,
    `${packageIdentity.displayName}_${packageVersion}_${architecture.msixArch}.msix`,
  );

  run(makeAppx, ["pack", "/o", "/d", packageStageDir, "/p", msixPath], desktopDir);
  msixFiles.push({ architecture, path: msixPath });
}

const bundleMapPath = join(storeBundleDir, "bundle-map.txt");
await writeFile(bundleMapPath, createBundleMap(msixFiles), "utf8");

const bundlePath = join(storeBundleDir, `${packageIdentity.displayName}_${packageVersion}.msixbundle`);
run(makeAppx, ["bundle", "/o", "/bv", packageVersion, "/f", bundleMapPath, "/p", bundlePath], desktopDir);

const uploadPath = join(storeBundleDir, `${packageIdentity.displayName}_${packageVersion}.msixupload`);
await createMsixUpload(bundlePath, uploadPath);

console.log("");
console.log("MSIX Store package outputs:");
for (const { path } of msixFiles) {
  console.log(`- ${path}`);
}
console.log(`- ${bundlePath}`);
console.log(`- ${uploadPath}`);

async function buildArchitecture(architecture) {
  console.log("");
  console.log(`Building ${architecture.msixArch} (${architecture.rustTarget})...`);
  runNpm(["run", "tauri", "--", "build", "--target", architecture.rustTarget, "--no-bundle", "--ci"], {
    ...process.env,
    PEDELEC_HELPER_TARGET: architecture.rustTarget,
  });
}

async function stageArchitecture(architecture) {
  console.log(`Staging ${architecture.msixArch} MSIX content...`);

  const stageDir = join(storeStageRoot, architecture.msixArch);
  const targetReleaseDir = join(targetDir, architecture.rustTarget, "release");
  const assetsDir = join(stageDir, "Assets");
  const binariesDir = join(stageDir, "binaries");

  await rm(stageDir, { recursive: true, force: true });
  await mkdir(assetsDir, { recursive: true });
  await mkdir(binariesDir, { recursive: true });

  await copyFile(join(targetReleaseDir, "pedelec-app.exe"), join(stageDir, "pedelec-app.exe"));

  for (const binaryName of ["pedelec-cli.exe", "pedelec-agent.exe", "pedelec-native-host.exe"]) {
    await copyFile(join(targetReleaseDir, binaryName), join(binariesDir, binaryName));
  }

  for (const assetName of [
    "StoreLogo.png",
    "Square44x44Logo.png",
    "Square150x150Logo.png",
    "Square310x310Logo.png",
  ]) {
    await copyFile(join(tauriDir, "icons", assetName), join(assetsDir, assetName));
  }

  await cp(join(tauriDir, "icons", "32x32.png"), join(assetsDir, "32x32.png"));
  await writeFile(
    join(stageDir, "AppxManifest.xml"),
    createManifest(architecture.msixArch, packageVersion),
    "utf8",
  );

  return stageDir;
}

function createManifest(processorArchitecture, version) {
  return `<?xml version="1.0" encoding="utf-8"?>
<Package
  xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
  xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10"
  xmlns:uap10="http://schemas.microsoft.com/appx/manifest/uap/windows10/10"
  xmlns:rescap="http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities"
  IgnorableNamespaces="uap10 rescap">
  <Identity
    Name="${escapeXml(packageIdentity.name)}"
    Publisher="${escapeXml(packageIdentity.publisher)}"
    Version="${version}"
    ProcessorArchitecture="${processorArchitecture}" />
  <Properties>
    <DisplayName>${escapeXml(packageIdentity.displayName)}</DisplayName>
    <PublisherDisplayName>${escapeXml(packageIdentity.publisherDisplayName)}</PublisherDisplayName>
    <Logo>Assets\\StoreLogo.png</Logo>
  </Properties>
  <Resources>
    <Resource Language="en-US" />
  </Resources>
  <Dependencies>
    <TargetDeviceFamily Name="Windows.Desktop" MinVersion="10.0.19041.0" MaxVersionTested="10.0.26100.0" />
  </Dependencies>
  <Capabilities>
    <rescap:Capability Name="runFullTrust" />
  </Capabilities>
  <Applications>
    <Application
      Id="${escapeXml(packageIdentity.applicationId)}"
      Executable="pedelec-app.exe"
      uap10:RuntimeBehavior="packagedClassicApp"
      uap10:TrustLevel="mediumIL">
      <uap:VisualElements
        DisplayName="${escapeXml(packageIdentity.displayName)}"
        Description="${escapeXml(packageIdentity.displayName)}"
        Square150x150Logo="Assets\\Square150x150Logo.png"
        Square44x44Logo="Assets\\Square44x44Logo.png"
        BackgroundColor="transparent" />
    </Application>
  </Applications>
</Package>
`;
}

function createBundleMap(msixFiles) {
  const lines = ["[Files]"];
  for (const { architecture, path } of msixFiles) {
    lines.push(`"${path}" "${packageIdentity.displayName}_${packageVersion}_${architecture.msixArch}.msix"`);
  }
  return `${lines.join("\r\n")}\r\n`;
}

async function createMsixUpload(bundlePath, uploadPath) {
  console.log("Creating MSIX upload package...");
  const uploadStageDir = join(storeStageRoot, "upload");
  await rm(uploadStageDir, { recursive: true, force: true });
  await mkdir(uploadStageDir, { recursive: true });
  await copyFile(bundlePath, join(uploadStageDir, dirnameFile(bundlePath)));
  await rm(uploadPath, { force: true });

  run(
    "powershell.exe",
    [
      "-NoProfile",
      "-ExecutionPolicy",
      "Bypass",
      "-Command",
      "Compress-Archive -Path * -DestinationPath $env:PEDELEC_MSIXUPLOAD -CompressionLevel Optimal -Force",
    ],
    uploadStageDir,
    {
      ...process.env,
      PEDELEC_MSIXUPLOAD: uploadPath,
    },
  );
}

async function findLatestWindowsSdkTool(toolName, arch) {
  const kitsBinDir = process.env.WindowsSdkVerBinPath
    ? process.env.WindowsSdkVerBinPath
    : "C:\\Program Files (x86)\\Windows Kits\\10\\bin";

  const candidates = [];
  const entries = await readdir(kitsBinDir, { withFileTypes: true }).catch(() => []);
  for (const entry of entries) {
    if (!entry.isDirectory() || !/^\d+\.\d+\.\d+\.\d+$/.test(entry.name)) {
      continue;
    }
    const toolPath = join(kitsBinDir, entry.name, arch, toolName);
    if (existsSync(toolPath)) {
      candidates.push({ version: entry.name, toolPath });
    }
  }

  candidates.sort((left, right) => compareDottedVersion(right.version, left.version));
  const latest = candidates[0]?.toolPath ?? join(kitsBinDir, arch, toolName);

  if (!existsSync(latest)) {
    throw new Error(`Could not find ${toolName} in the Windows SDK. Install the Windows 10/11 SDK.`);
  }

  return latest;
}

function toMsixVersion(version) {
  const parts = version.split(".");
  if (parts.length > 4 || parts.some((part) => !/^\d+$/.test(part))) {
    throw new Error(`Package version "${version}" is not compatible with MSIX dotted-quad versions.`);
  }

  while (parts.length < 4) {
    parts.push("0");
  }

  for (const part of parts) {
    const value = Number(part);
    if (value < 0 || value > 65535) {
      throw new Error(`MSIX version part "${part}" must be between 0 and 65535.`);
    }
  }

  return parts.join(".");
}

function compareDottedVersion(left, right) {
  const leftParts = left.split(".").map(Number);
  const rightParts = right.split(".").map(Number);
  for (let index = 0; index < Math.max(leftParts.length, rightParts.length); index += 1) {
    const diff = (leftParts[index] ?? 0) - (rightParts[index] ?? 0);
    if (diff !== 0) {
      return diff;
    }
  }
  return 0;
}

function assertRequiredToolchain() {
  if (!architectures.some((architecture) => architecture.msixArch === "arm64")) {
    return;
  }

  if (process.env.CC?.toLowerCase().includes("clang-cl") || commandExists("clang-cl.exe")) {
    return;
  }

  console.error(
    [
      "arm64 packaging requires clang-cl.exe for the aws-lc-sys dependency.",
      "Install the Visual Studio Build Tools component: C++ Clang Compiler for Windows.",
      "Alternatively, run from a developer environment where CC points to a working clang-cl.",
    ].join("\n"),
  );
  process.exit(1);
}

function commandExists(command) {
  const result = spawnSync("where.exe", [command], {
    stdio: "ignore",
  });
  return result.status === 0;
}

function dirnameFile(path) {
  return path.split(/[\\/]/).at(-1);
}

function escapeXml(value) {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

function runNpm(args, env) {
  if (process.platform === "win32") {
    run(process.env.ComSpec ?? "cmd.exe", ["/d", "/s", "/c", "npm", ...args], desktopDir, env);
    return;
  }

  run("npm", args, desktopDir, env);
}

function run(command, args, cwd, env = process.env) {
  const result = spawnSync(command, args, {
    cwd,
    env,
    stdio: "inherit",
  });

  if (result.error) {
    throw result.error;
  }

  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}
