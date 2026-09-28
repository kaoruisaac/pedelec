import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { publicHelperBinaryNames } from "./deno-release.mjs";
import {
  assertHelpersPresent,
  assertNoRawDeno,
  assertReleaseDirectory,
  assertWindowsNsisPayload,
  assertWindowsUpdaterBundle,
  assertWindowsUpdaterManifest,
  assertWindowsUpdaterPayload,
  findLocalWindowsNsisInstaller,
  payloadPathsFromSevenZipListing,
  windowsUpdaterArtifactNames,
} from "./verify-release-bundle.mjs";

test("release verification rejects raw Deno and accepts helper binaries", () => {
  assert.throws(
    () => assertNoRawDeno(["binaries/deno.exe", "binaries/pedelec-deno.exe"]),
    /raw Deno must not be bundled/,
  );
  assert.throws(
    () => assertNoRawDeno(["Contents/MacOS/deno"]),
    /raw Deno must not be bundled/,
  );
  assert.doesNotThrow(() =>
    assertNoRawDeno([
      "binaries/pedelec-cli.exe",
      "binaries/pedelec-deno.exe",
      "binaries/pedelec-agent.exe",
      "binaries/pedelec-native-host.exe",
    ]),
  );
  assert.throws(
    () => assertHelpersPresent(publicHelperBinaryNames("win32"), ["binaries/pedelec-deno.exe"]),
    /missing staged Pedelec helper: pedelec-cli.exe/,
  );
});

test("a directory containing raw Deno fails release verification", async () => {
  const directory = await mkdtemp(join(tmpdir(), "pedelec-bundle-"));
  try {
    const binaries = join(directory, "binaries");
    await mkdir(binaries, { recursive: true });
    for (const name of publicHelperBinaryNames(process.platform)) {
      await writeFile(join(binaries, name), "helper");
    }
    await assertReleaseDirectory(directory, { requireHelpers: true });
    await writeFile(join(binaries, process.platform === "win32" ? "deno.exe" : "deno"), "raw");
    await assert.rejects(
      assertReleaseDirectory(directory, { requireHelpers: true }),
      /raw Deno must not be bundled/,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

function releaseAssetUrl(name) {
  return `https://github.com/example/releases/download/v0.4.5/${name}`;
}

test("realistic tauri-action Windows latest.json selects each generic NSIS artifact once", () => {
  const nsis = "Pedelec-0.4.5-windows-x86_64-nsis.exe";
  const names = windowsUpdaterArtifactNames({
    platforms: {
      "windows-x86_64": { url: releaseAssetUrl(nsis), signature: "generic" },
      "windows-x86_64-nsis": { url: releaseAssetUrl(nsis), signature: "nsis" },
      "windows-x86_64-msi": {
        url: releaseAssetUrl("Pedelec-0.4.5-windows-x86_64-msi.msi"),
        signature: "msi",
      },
      "darwin-aarch64": { url: releaseAssetUrl("Pedelec.app.tar.gz") },
      "darwin-aarch64-app": { url: releaseAssetUrl("Pedelec.app.tar.gz") },
      "linux-x86_64": { url: releaseAssetUrl("Pedelec.AppImage.tar.gz") },
      "linux-x86_64-appimage": { url: releaseAssetUrl("Pedelec.AppImage.tar.gz") },
    },
  });
  assert.deepEqual(names, [nsis]);
  assert.equal(new Set(names).size, names.length);
  assert.deepEqual(assertWindowsUpdaterManifest({
    platforms: {
      "windows-x86_64": { url: releaseAssetUrl(nsis), signature: "generic" },
      "windows-x86_64-nsis": { url: releaseAssetUrl(nsis), signature: "nsis" },
      "windows-x86_64-msi": {
        url: releaseAssetUrl("Pedelec-0.4.5-windows-x86_64-msi.msi"),
        signature: "msi",
      },
      "darwin-aarch64": { url: releaseAssetUrl("Pedelec.app.tar.gz") },
      "linux-x86_64": { url: releaseAssetUrl("Pedelec.AppImage.tar.gz") },
    },
  }), names);
});

test("generic Windows updater entry pointing at MSI fails", () => {
  assert.throws(
    () => windowsUpdaterArtifactNames({
      platforms: {
        "windows-x86_64": {
          url: releaseAssetUrl("Pedelec-0.4.5-windows-x86_64-msi.msi"),
        },
        "windows-x86_64-nsis": {
          url: releaseAssetUrl("Pedelec-0.4.5-windows-x86_64-nsis.exe"),
        },
      },
    }),
    /NSIS installer/,
  );
});

test("installer-specific Windows entries without a generic updater entry fail", () => {
  assert.throws(
    () => windowsUpdaterArtifactNames({
      platforms: {
        "windows-x86_64-nsis": {
          url: releaseAssetUrl("Pedelec-0.4.5-windows-x86_64-nsis.exe"),
        },
        "windows-x86_64-msi": {
          url: releaseAssetUrl("Pedelec-0.4.5-windows-x86_64-msi.msi"),
        },
      },
    }),
    /generic Windows updater entry/,
  );
});

test("each Windows architecture selects its NSIS updater artifact once", () => {
  const x64 = "Pedelec-0.4.5-windows-x86_64-nsis.exe";
  const arm = "Pedelec-0.4.5-windows-aarch64-nsis.exe";
  const names = windowsUpdaterArtifactNames({
    platforms: {
      "windows-x86_64": { url: releaseAssetUrl(x64) },
      "windows-x86_64-nsis": { url: releaseAssetUrl(x64) },
      "windows-x86_64-msi": {
        url: releaseAssetUrl("Pedelec-0.4.5-windows-x86_64-msi.msi"),
      },
      "darwin-x86_64": { url: releaseAssetUrl("Pedelec-x64.app.tar.gz") },
      "linux-x86_64": { url: releaseAssetUrl("Pedelec-x64.AppImage.tar.gz") },
      "windows-aarch64": { url: releaseAssetUrl(arm) },
      "windows-aarch64-nsis": { url: releaseAssetUrl(arm) },
      "windows-aarch64-msi": {
        url: releaseAssetUrl("Pedelec-0.4.5-windows-aarch64-msi.msi"),
      },
    },
  });
  assert.deepEqual(names, [x64, arm]);
});

test("windows-arch-nsis must reference the same installer as the generic entry", () => {
  assert.throws(
    () => windowsUpdaterArtifactNames({
      platforms: {
        "windows-x86_64": {
          url: releaseAssetUrl("Pedelec-0.4.5-windows-x86_64-nsis.exe"),
        },
        "windows-x86_64-nsis": {
          url: releaseAssetUrl("Pedelec-0.4.5-windows-x86_64-other-nsis.exe"),
        },
      },
    }),
    /same NSIS installer/,
  );
});

test("Windows updater payload listing rejects raw Deno inside the installer", () => {
  const archivePath = "C:\\bundle\\nsis\\Pedelec-0.4.5-windows-x86_64-nsis.exe";
  const listing = [
    "Path = C:\\bundle\\nsis\\Pedelec-0.4.5-windows-x86_64-nsis.exe",
    "Type = Nsis",
    "Path = pedelec-app.exe",
    "Path = binaries\\pedelec-cli.exe",
    "Path = binaries\\pedelec-deno.exe",
    "Path = binaries\\pedelec-agent.exe",
    "Path = binaries\\pedelec-native-host.exe",
  ].join("\n");
  const clean = payloadPathsFromSevenZipListing(listing, archivePath);
  assert.deepEqual(clean, [
    "pedelec-app.exe",
    "binaries\\pedelec-cli.exe",
    "binaries\\pedelec-deno.exe",
    "binaries\\pedelec-agent.exe",
    "binaries\\pedelec-native-host.exe",
  ]);
  assert.doesNotThrow(() => assertWindowsUpdaterPayload(clean));

  const dirty = payloadPathsFromSevenZipListing(
    `${listing}\nPath = binaries\\deno.exe\nPath = deno.exe`,
    archivePath,
  );
  assert.throws(() => assertWindowsUpdaterPayload(dirty), /raw Deno must not be bundled/);
  assert.throws(() => assertWindowsUpdaterPayload(["Pedelec-setup.exe"]), /could not be listed/);
});

function portable(relativePath) {
  return relativePath.replaceAll("\\", "/");
}

function renamedNsisLatestJson() {
  const uploaded = "Pedelec-0.4.5-windows-x64-nsis.exe";
  return {
    uploaded,
    latestJson: {
      platforms: {
        "windows-x86_64": { url: releaseAssetUrl(uploaded), signature: "generic" },
        "windows-x86_64-nsis": { url: releaseAssetUrl(uploaded), signature: "nsis" },
        "windows-x86_64-msi": {
          url: releaseAssetUrl("Pedelec-0.4.5-windows-x86_64-msi.msi"),
          signature: "msi",
        },
      },
    },
  };
}

function cleanInstallerListing(archivePath) {
  return [
    `Path = ${archivePath}`,
    "Path = binaries\\pedelec-cli.exe",
    "Path = binaries\\pedelec-deno.exe",
    "Path = binaries\\pedelec-agent.exe",
    "Path = binaries\\pedelec-native-host.exe",
  ].join("\n");
}

async function writeNsisBundle(directory, installers) {
  const nsis = join(directory, "nsis");
  const msi = join(directory, "msi");
  await mkdir(nsis, { recursive: true });
  await mkdir(msi, { recursive: true });
  await writeFile(join(msi, "Pedelec_0.4.5_x64_en-US.msi"), "msi");
  await writeFile(join(nsis, "Pedelec_0.4.5_x64-setup.exe.sig"), "sig");
  await writeFile(join(nsis, "Pedelec_0.4.5_x64-setup.nsis.zip"), "zip");
  await writeFile(join(nsis, "Pedelec_0.4.5_x64-setup.nsis.zip.sig"), "zipsig");
  for (const name of installers) {
    await writeFile(join(nsis, name), "installer");
  }
}

test("uploaded latest.json NSIS name can differ from the local Tauri installer", async () => {
  const directory = await mkdtemp(join(tmpdir(), "pedelec-nsis-"));
  const localName = "Pedelec_0.4.5_x64-setup.exe";
  try {
    await writeNsisBundle(directory, [localName]);
    const { uploaded, latestJson } = renamedNsisLatestJson();
    assert.deepEqual(assertWindowsUpdaterManifest(latestJson), [uploaded]);
    assert.equal(portable(await findLocalWindowsNsisInstaller(directory)), `nsis/${localName}`);

    const latestPath = join(directory, "latest.json");
    await writeFile(latestPath, JSON.stringify(latestJson));
    let inspected = null;
    const found = await assertWindowsUpdaterBundle(directory, {
      latestJsonPath: latestPath,
      listArchive: async (archivePath) => {
        inspected = archivePath;
        return cleanInstallerListing(archivePath);
      },
    });
    assert.equal(portable(found), `nsis/${localName}`);
    assert.equal(portable(inspected).split("/").pop(), localName);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("local NSIS discovery ignores updater sidecars and fails without an installer", async () => {
  const directory = await mkdtemp(join(tmpdir(), "pedelec-nsis-"));
  try {
    await writeNsisBundle(directory, []);
    await assert.rejects(findLocalWindowsNsisInstaller(directory), /local Windows NSIS installer was not found/);

    const localName = "Pedelec_0.4.5_x64-setup.exe";
    await writeFile(join(directory, "nsis", localName), "installer");
    assert.equal(portable(await findLocalWindowsNsisInstaller(directory)), `nsis/${localName}`);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("multiple local NSIS installers are ambiguous", async () => {
  const directory = await mkdtemp(join(tmpdir(), "pedelec-nsis-"));
  try {
    await writeNsisBundle(directory, [
      "Pedelec_0.4.5_x64-setup.exe",
      "Pedelec_0.4.5_arm64-setup.exe",
    ]);
    await assert.rejects(
      findLocalWindowsNsisInstaller(directory),
      /ambiguous local Windows NSIS installers: Pedelec_0\.4\.5_arm64-setup\.exe, Pedelec_0\.4\.5_x64-setup\.exe/,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("generic Windows updater pointing at MSI fails even when a local NSIS installer exists", async () => {
  const directory = await mkdtemp(join(tmpdir(), "pedelec-nsis-"));
  try {
    await writeNsisBundle(directory, ["Pedelec_0.4.5_x64-setup.exe"]);
    const latestPath = join(directory, "latest.json");
    await writeFile(latestPath, JSON.stringify({
      platforms: {
        "windows-x86_64": {
          url: releaseAssetUrl("Pedelec-0.4.5-windows-x86_64-msi.msi"),
        },
        "windows-x86_64-nsis": {
          url: releaseAssetUrl("Pedelec-0.4.5-windows-x86_64-nsis.exe"),
        },
      },
    }));
    let listed = false;
    await assert.rejects(
      assertWindowsUpdaterBundle(directory, {
        latestJsonPath: latestPath,
        listArchive: async () => {
          listed = true;
          return "";
        },
      }),
      /NSIS installer/,
    );
    assert.equal(listed, false);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("raw Deno inside the local NSIS payload fails release verification", async () => {
  const directory = await mkdtemp(join(tmpdir(), "pedelec-nsis-"));
  const localName = "Pedelec_0.4.5_x64-setup.exe";
  try {
    await writeNsisBundle(directory, [localName]);
    const { latestJson } = renamedNsisLatestJson();
    const latestPath = join(directory, "latest.json");
    await writeFile(latestPath, JSON.stringify(latestJson));
    const archivePath = join(directory, "nsis", localName);
    await assert.rejects(
      assertWindowsNsisPayload(archivePath, async () => [
        `Path = ${archivePath}`,
        "Path = binaries\\pedelec-deno.exe",
        "Path = binaries\\deno.exe",
      ].join("\n")),
      /raw Deno must not be bundled/,
    );
    await assert.rejects(
      assertWindowsUpdaterBundle(directory, {
        latestJsonPath: latestPath,
        listArchive: async () => "Path = installer.exe",
      }),
      /could not be listed/,
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
