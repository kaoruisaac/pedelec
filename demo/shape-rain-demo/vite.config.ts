import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig, normalizePath } from "vite";
import solid from "vite-plugin-solid";

const rootDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(rootDir, "../..");
const sdkSourceDir = resolve(repoRoot, "sdk/src");
const devExtensionIdEnv = "PEDELEC_DEV_CHROME_EXTENSION_ID";

export default defineConfig(({ command }) => ({
  plugins: [
    solid(),
    command === "serve" ? pedelecDevExtensionIdPlugin() : null,
  ].filter(Boolean),
  resolve: {
    alias: {
      "@kaoruisaac/pedelec": resolve(sdkSourceDir, "index.ts"),
    },
  },
  server: {
    host: "127.0.0.1",
    port: 5174,
  },
}));

function pedelecDevExtensionIdPlugin() {
  const devExtensionId = readDevExtensionId();
  if (!devExtensionId) return null;

  const indexModuleId = normalizePath(resolve(sdkSourceDir, "index.ts"));
  const virtualModuleId = "\0pedelec-dev-extension-id";

  return {
    name: "pedelec-dev-extension-id",
    enforce: "pre" as const,
    resolveId(id: string, importer?: string) {
      if (id === "./extension-id.js" && importer && normalizePath(importer) === indexModuleId) {
        return virtualModuleId;
      }
      return null;
    },
    load(id: string) {
      if (id !== virtualModuleId) return null;
      return `export const PEDELEC_EXTENSION_ID = ${JSON.stringify(devExtensionId)};\n`;
    },
  };
}

function readDevExtensionId(): string | null {
  const fromEnv = validateChromeExtensionId(process.env[devExtensionIdEnv]);
  if (fromEnv) return fromEnv;

  try {
    const contents = readFileSync(resolve(repoRoot, ".env.local"), "utf8");
    return validateChromeExtensionId(readDotenvValue(contents, devExtensionIdEnv));
  } catch (_err) {
    return null;
  }
}

function readDotenvValue(contents: string, key: string): string | null {
  for (const rawLine of contents.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;

    const separator = line.indexOf("=");
    if (separator === -1) continue;

    const currentKey = line.slice(0, separator).trim();
    if (currentKey !== key) continue;

    const value = line.slice(separator + 1).trim();
    return (
      stripQuotes(value, '"') ??
      stripQuotes(value, "'") ??
      value
    );
  }
  return null;
}

function stripQuotes(value: string, quote: string): string | null {
  return value.startsWith(quote) && value.endsWith(quote)
    ? value.slice(1, -1)
    : null;
}

function validateChromeExtensionId(value: string | null | undefined): string | null {
  const extensionId = value?.trim();
  return extensionId && /^[a-p]{32}$/.test(extensionId) ? extensionId : null;
}
