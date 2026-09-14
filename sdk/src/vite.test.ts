import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import * as ts from "typescript";
import { build, createServer, type Plugin, type ViteDevServer } from "vite";
import { afterEach, describe, expect, it } from "vitest";

import { pedelecVitePlugin } from "./vite";

const repoRoot = resolve(import.meta.dirname, "../..");
const sdkDist = resolve(repoRoot, "sdk/dist");
const sdkRootEntry = resolve(import.meta.dirname, "index.ts");
const temporaryDirectories: string[] = [];

afterEach(async () => {
  await Promise.all(temporaryDirectories.splice(0).map((directory) => rm(directory, { recursive: true, force: true })));
});

type Artifact = {
  format: "esm";
  runtimeSource: string;
  typesSource: string;
  contentHash: string;
};

async function createFixture(): Promise<string> {
  const directory = await mkdtemp(join(tmpdir(), "pedelec-vite-"));
  temporaryDirectories.push(directory);
  await mkdir(join(directory, "agent"), { recursive: true });
  await writeFile(
    join(directory, "agent", "dependency.ts"),
    `export const suffix = "!";\nexport type Direction = "front" | "back";\n`,
  );
  await writeFile(
    join(directory, "agent", "module.ts"),
    `import { suffix } from "./dependency";\nexport interface PreviewOptions {\n  /** Integer multiplier from 1 through 8. */\n  scale?: number;\n  direction: "front" | "back";\n}\nexport function preview(value: string, options: PreviewOptions): string { return value + suffix + options.direction; }\n`,
  );
  await writeFile(
    join(directory, "main.ts"),
    `import { defineDenoModule } from "@kaoruisaac/pedelec";\nexport const module = defineDenoModule({ name: "sprite-tools", description: "Preview sprites.", entry: "./agent/module.ts", usage: \`import { preview } from "sprite-tools";\` });\n`,
  );
  return directory;
}

async function createTypedPackageFixture(directory: string): Promise<void> {
  const packageDirectory = join(directory, "node_modules", "typed-dependency");
  await mkdir(packageDirectory, { recursive: true });
  await writeFile(join(packageDirectory, "package.json"), `{"name":"typed-dependency","main":"index.js","types":"index.d.ts"}`);
  await writeFile(join(packageDirectory, "index.js"), `export const packageDirection = "front";\n`);
  await writeFile(
    join(packageDirectory, "index.d.ts"),
    `export type PackageDirection = "front" | "back";\nexport interface PackageOptions { direction: PackageDirection; }\nexport declare const packageDirection: PackageDirection;\n`,
  );
  await writeFile(
    join(directory, "agent", "package-module.ts"),
    `import { packageDirection, type PackageOptions as Options } from "typed-dependency";\nexport function usePackage(options: Options): PackageDirection { return options.direction || packageDirection; }\n`,
  );
  await writeFile(
    join(directory, "package-main.ts"),
    `import { defineDenoModule } from "@kaoruisaac/pedelec";\nexport const module = defineDenoModule({ name: "package-tools", description: "Package utilities.", entry: "typed-dependency", usage: "import { usePackage } from \\\"package-tools\\\";" });\n`,
  );
}

async function createServerFor(directory: string, alias: Record<string, string> = {}): Promise<ViteDevServer> {
  return createServer({
    configFile: false,
    root: directory,
    plugins: [pedelecVitePlugin()],
    resolve: { alias: { "@kaoruisaac/pedelec": join(sdkDist, "index.js"), ...alias } },
    server: { middlewareMode: true },
  });
}

async function withServer<T>(
  directory: string,
  run: (server: ViteDevServer) => Promise<T>,
  alias: Record<string, string> = {},
): Promise<T> {
  const server = await createServerFor(directory, alias);
  try {
    return await run(server);
  } finally {
    await server.close();
  }
}

function sliceJsonValue(source: string, start: number): string {
  let depth = 0;
  let inString = false;
  let escaped = false;
  for (let index = start; index < source.length; index += 1) {
    const character = source[index];
    if (inString) {
      if (escaped) escaped = false;
      else if (character === "\\") escaped = true;
      else if (character === '"') inString = false;
      continue;
    }
    if (character === '"') inString = true;
    else if (character === "{") depth += 1;
    else if (character === "}") {
      depth -= 1;
      if (depth === 0) return source.slice(start, index + 1);
    }
  }
  throw new Error("unterminated artifact JSON");
}

function extractArtifacts(code: string): Artifact[] {
  const artifacts: Artifact[] = [];
  const needle = "Object.freeze(";
  let searchFrom = 0;
  while (searchFrom < code.length) {
    const freezeAt = code.indexOf(needle, searchFrom);
    if (freezeAt < 0) break;
    const valueStart = code.indexOf("{", freezeAt);
    if (valueStart < 0) break;
    const objectText = sliceJsonValue(code, valueStart);
    try {
      artifacts.push(Function(`"use strict"; return (${objectText});`)() as Artifact);
    } catch {
      searchFrom = valueStart + 1;
      continue;
    }
    searchFrom = valueStart + objectText.length;
  }
  return artifacts;
}

function extractArtifact(code: string): Artifact {
  const artifacts = extractArtifacts(code);
  if (artifacts.length !== 1) throw new Error(`expected 1 artifact, received ${artifacts.length}`);
  return artifacts[0];
}

function standaloneDeclarationDiagnostics(source: string): string[] {
  const options: ts.CompilerOptions = {
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.ESNext,
    skipLibCheck: true,
    noEmit: true,
    noResolve: true,
    types: [],
    typeRoots: [],
    strict: true,
  };
  const host = ts.createCompilerHost(options);
  const fileName = resolve(host.getCurrentDirectory(), "__pedelec_artifact__.d.ts");
  const originalGetSourceFile = host.getSourceFile.bind(host);
  const originalReadFile = host.readFile?.bind(host);
  host.fileExists = (name) => name.includes("__pedelec_artifact__") || ts.sys.fileExists(name);
  host.readFile = (name) => (name.includes("__pedelec_artifact__") ? source : originalReadFile ? originalReadFile(name) : ts.sys.readFile(name));
  host.getSourceFile = (name, languageVersion, onError, shouldCreateNewSourceFile) => {
    if (name.includes("__pedelec_artifact__")) return ts.createSourceFile(fileName, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
    return originalGetSourceFile(name, languageVersion, onError, shouldCreateNewSourceFile);
  };
  const program = ts.createProgram([fileName], options, host);
  return ts
    .getPreEmitDiagnostics(program)
    .filter((item) => item.category === ts.DiagnosticCategory.Error)
    .filter((item) => !item.file || !/[\\/]typescript[\\/]lib[\\/]/i.test(item.file.fileName))
    .map((item) => ts.flattenDiagnosticMessageText(item.messageText, "\n"));
}

function rootExportNames(source: string): Set<string> {
  const sourceFile = ts.createSourceFile("index.d.ts", source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
  const names = new Set<string>();
  for (const statement of sourceFile.statements) {
    const exported = (ts.getCombinedModifierFlags(statement as ts.Declaration) & ts.ModifierFlags.Export) !== 0;
    if (ts.isImportEqualsDeclaration(statement) && exported) names.add(statement.name.text);
    if (ts.isExportDeclaration(statement) && statement.exportClause && ts.isNamedExports(statement.exportClause)) {
      for (const element of statement.exportClause.elements) names.add(element.name.text);
    }
    if (exported) {
      if (
        ts.isFunctionDeclaration(statement) ||
        ts.isClassDeclaration(statement) ||
        ts.isInterfaceDeclaration(statement) ||
        ts.isTypeAliasDeclaration(statement)
      ) {
        if (statement.name) names.add(statement.name.text);
      }
    }
  }
  return names;
}

async function transformFile(server: ViteDevServer, fileName: string): Promise<string> {
  const transformed = await server.transformRequest(fileName);
  return transformed?.code ?? "";
}

function pluginHook(server: ViteDevServer): Plugin {
  const plugin = server.config.plugins.find((item) => item && item.name === "pedelec-deno-module");
  if (!plugin) throw new Error("pedelec Vite plugin was not registered");
  return plugin;
}

async function triggerHotUpdate(server: ViteDevServer, file: string) {
  const plugin = pluginHook(server);
  const hook = plugin.handleHotUpdate;
  const handler = typeof hook === "function" ? hook : hook?.handler;
  if (!handler) throw new Error("handleHotUpdate is missing");
  return handler.call(
    {
      environment: server.environments.client,
    },
    {
      file,
      timestamp: Date.now(),
      modules: [...(server.moduleGraph.getModulesByFile(file) ?? [])],
      read: () => readFile(file, "utf8"),
      server,
      type: "update",
      isSelfAccepting: false,
    } as never,
  );
}

describe("pedelecVitePlugin", () => {
  it("finds the SDK declaration and embeds deterministic runtime and declaration artifacts", async () => {
    const directory = await createFixture();
    await withServer(directory, async (server) => {
      const code = await transformFile(server, join(directory, "main.ts"));
      const artifact = extractArtifact(code);
      expect(code).toContain("__pedelecArtifact");
      expect(code).toContain("entry = undefined");
      expect(code).not.toContain("./agent/module.ts");
      expect(artifact.runtimeSource.startsWith('// @ts-self-types="./index.d.ts"\n')).toBe(true);
      expect(artifact.typesSource).toContain("PreviewOptions");
      expect(artifact.typesSource).toContain("Integer multiplier from 1 through 8");
      expect(artifact.typesSource).toContain("front");
      expect(artifact.runtimeSource).toContain("!");
      expect(standaloneDeclarationDiagnostics(artifact.typesSource)).toEqual([]);
    });
  });

  it("rejects dynamic entries and unrelated same-named functions", async () => {
    const directory = await createFixture();
    await writeFile(
      join(directory, "main.ts"),
      `function defineDenoModule(value: unknown) { return value; }\nconst entry = "./agent/module.ts";\nexport const unrelated = defineDenoModule({ entry });\n`,
    );
    await withServer(directory, async (server) => {
      const transformed = await transformFile(server, join(directory, "main.ts"));
      expect(transformed).toContain("defineDenoModule({ entry })");
    });

    await writeFile(
      join(directory, "main.ts"),
      `import { defineDenoModule as module } from "@kaoruisaac/pedelec";\nconst entry = "./agent/module.ts";\nexport const declared = module({ name: "bad", description: "bad", entry, usage: "bad" });\n`,
    );
    await withServer(directory, async (server) => {
      await expect(server.transformRequest(join(directory, "main.ts"))).rejects.toThrow("entry must be a string literal");
    });
  });

  it("transforms an aliased defineDenoModule import on a valid declaration", async () => {
    const directory = await createFixture();
    await writeFile(
      join(directory, "main.ts"),
      `import { defineDenoModule as declareModule } from "@kaoruisaac/pedelec";\nexport const module = declareModule({ name: "sprite-tools", description: "Preview sprites.", entry: "./agent/module.ts", usage: "import { preview } from \\"sprite-tools\\";" });\n`,
    );
    await withServer(directory, async (server) => {
      const artifact = extractArtifact(await transformFile(server, join(directory, "main.ts")));
      expect(artifact.typesSource).toContain("preview");
      expect(artifact.runtimeSource).toContain("!");
    });
  });

  it("transforms multiple declarations in one source independently", async () => {
    const directory = await createFixture();
    await writeFile(join(directory, "agent", "other.ts"), `export function other(): "other" { return "other"; }\n`);
    await writeFile(
      join(directory, "main.ts"),
      `import { defineDenoModule } from "@kaoruisaac/pedelec";\nexport const first = defineDenoModule({ name: "sprite-tools", description: "Preview sprites.", entry: "./agent/module.ts", usage: "first" });\nexport const second = defineDenoModule({ name: "other-tools", description: "Other utilities.", entry: "./agent/other.ts", usage: "second" });\n`,
    );
    await withServer(directory, async (server) => {
      const artifacts = extractArtifacts(await transformFile(server, join(directory, "main.ts")));
      expect(artifacts).toHaveLength(2);
      expect(artifacts[0]?.typesSource).toContain("preview");
      expect(artifacts[0]?.typesSource).not.toContain("function other");
      expect(artifacts[1]?.typesSource).toContain("other");
      expect(artifacts[0]?.contentHash).not.toBe(artifacts[1]?.contentHash);
    });
  });

  it("does not transform imports from another package", async () => {
    const directory = await createFixture();
    await writeFile(join(directory, "other.ts"), "export function defineDenoModule(value: unknown) { return value; }\n");
    await writeFile(
      join(directory, "main.ts"),
      `import { defineDenoModule } from "./other";\nexport const module = defineDenoModule({ name: "x", description: "x", entry: "./agent/module.ts", usage: "x" });\n`,
    );
    await withServer(directory, async (server) => {
      const transformed = await transformFile(server, join(directory, "main.ts"));
      expect(transformed).toContain("defineDenoModule");
      expect(transformed).not.toContain("__pedelecArtifact");
    });
  });

  it("leaves locally shadowed defineDenoModule calls untouched", async () => {
    const directory = await createFixture();
    await writeFile(
      join(directory, "main.ts"),
      `import { defineDenoModule } from "@kaoruisaac/pedelec";\nexport const real = defineDenoModule({ name: "sprite-tools", description: "Preview sprites.", entry: "./agent/module.ts", usage: "real" });\nexport function wrap() {\n  const defineDenoModule = (value: unknown) => value;\n  return defineDenoModule({ name: "shadowed", description: "shadowed", entry: "./missing.ts", usage: "shadowed" });\n}\n`,
    );
    await withServer(directory, async (server) => {
      const code = await transformFile(server, join(directory, "main.ts"));
      expect(extractArtifacts(code)).toHaveLength(1);
      expect(code).toContain('entry: "./missing.ts"');
      expect(code).toContain("__pedelecArtifact");
    });
  });

  it("resolves a relative entry from the declaring source file", async () => {
    const directory = await createFixture();
    await mkdir(join(directory, "nested"), { recursive: true });
    await writeFile(
      join(directory, "nested", "main.ts"),
      `import { defineDenoModule } from "@kaoruisaac/pedelec";\nexport const module = defineDenoModule({ name: "sprite-tools", description: "Preview sprites.", entry: "../agent/module.ts", usage: "nested" });\n`,
    );
    await withServer(directory, async (server) => {
      const artifact = extractArtifact(await transformFile(server, join(directory, "nested", "main.ts")));
      expect(artifact.runtimeSource).toContain("!");
      expect(artifact.typesSource).toContain("PreviewOptions");
    });
  });

  it("resolves a Vite alias entry", async () => {
    const directory = await createFixture();
    await writeFile(
      join(directory, "main.ts"),
      `import { defineDenoModule } from "@kaoruisaac/pedelec";\nexport const module = defineDenoModule({ name: "sprite-tools", description: "Preview sprites.", entry: "@agent/module.ts", usage: "alias" });\n`,
    );
    await withServer(
      directory,
      async (server) => {
        const artifact = extractArtifact(await transformFile(server, join(directory, "main.ts")));
        expect(artifact.runtimeSource).toContain("!");
        expect(artifact.typesSource).toContain("PreviewOptions");
      },
      { "@agent": join(directory, "agent") },
    );
  });

  it("rejects an unresolved relative entry with logical name, entry, and importer context", async () => {
    const directory = await createFixture();
    await writeFile(
      join(directory, "main.ts"),
      `import { defineDenoModule } from "@kaoruisaac/pedelec";\nexport const module = defineDenoModule({ name: "missing-tools", description: "Missing.", entry: "./agent/missing.ts", usage: "missing" });\n`,
    );
    await withServer(directory, async (server) => {
      await expect(server.transformRequest(join(directory, "main.ts"))).rejects.toThrow(
        /could not resolve entry "\.\/agent\/missing\.ts" for Deno module "missing-tools" from ".*main\.ts"/,
      );
    });
  });

  it("rejects an unresolved bare package entry clearly", async () => {
    const directory = await createFixture();
    await writeFile(
      join(directory, "main.ts"),
      `import { defineDenoModule } from "@kaoruisaac/pedelec";\nexport const module = defineDenoModule({ name: "missing-pkg", description: "Missing.", entry: "definitely-not-installed-pedelec-pkg", usage: "missing" });\n`,
    );
    await withServer(directory, async (server) => {
      await expect(server.transformRequest(join(directory, "main.ts"))).rejects.toThrow(
        /could not resolve entry "definitely-not-installed-pedelec-pkg" for Deno module "missing-pkg" from ".*main\.ts"/,
      );
    });
  });

  it("rejects remote entries with the logical module and source entry in the diagnostic", async () => {
    const directory = await createFixture();
    await writeFile(
      join(directory, "main.ts"),
      `import { defineDenoModule } from "@kaoruisaac/pedelec";\nexport const module = defineDenoModule({ name: "remote", description: "remote", entry: "https://example.test/module.ts", usage: "remote" });\n`,
    );
    await withServer(directory, async (server) => {
      await expect(server.transformRequest(join(directory, "main.ts"))).rejects.toThrow(
        /Deno module "remote" entry "https:\/\/example\.test\/module\.ts".*remote and scheme URLs/,
      );
    });
  });

  it("includes a transitive local runtime dependency in the single bundle", async () => {
    const directory = await createFixture();
    await writeFile(join(directory, "agent", "deep.ts"), `export const deep = "deep-value";\n`);
    await writeFile(
      join(directory, "agent", "dependency.ts"),
      `import { deep } from "./deep";\nexport const suffix = deep;\n`,
    );
    await withServer(directory, async (server) => {
      const artifact = extractArtifact(await transformFile(server, join(directory, "main.ts")));
      expect(artifact.runtimeSource).toContain("deep-value");
      expect(artifact.runtimeSource).not.toMatch(/\bfrom\s*["']\./);
    });
  });

  it("includes a transitive package implementation dependency in the single bundle", async () => {
    const directory = await createFixture();
    const packageDirectory = join(directory, "node_modules", "impl-dep");
    await mkdir(packageDirectory, { recursive: true });
    await writeFile(join(packageDirectory, "package.json"), `{"name":"impl-dep","type":"module","main":"index.js"}`);
    await writeFile(join(packageDirectory, "index.js"), `export const tag = "pkg-impl";\nexport function mark(value) { return value + tag; }\n`);
    await writeFile(
      join(directory, "agent", "module.ts"),
      `import { mark } from "impl-dep";\nexport function preview(value: string): string { return mark(value); }\n`,
    );
    await withServer(directory, async (server) => {
      const artifact = extractArtifact(await transformFile(server, join(directory, "main.ts")));
      expect(artifact.runtimeSource).toContain("pkg-impl");
      expect(artifact.runtimeSource).not.toContain("impl-dep");
    });
  });

  it("rejects an unresolved runtime dependency", async () => {
    const directory = await createFixture();
    await writeFile(
      join(directory, "agent", "module.ts"),
      `import { missing } from "definitely-not-installed-runtime-pkg";\nexport function preview(): unknown { return missing; }\n`,
    );
    await withServer(directory, async (server) => {
      await expect(server.transformRequest(join(directory, "main.ts"))).rejects.toThrow(
        /runtime bundling failed|unresolved runtime dependency/,
      );
    });
  });

  it("rejects unsupported dynamic import output instead of dropping secondary chunks", async () => {
    const directory = await createFixture();
    await writeFile(join(directory, "agent", "lazy.ts"), `export const lazy = "lazy";\n`);
    await writeFile(
      join(directory, "agent", "module.ts"),
      `const spec = "./lazy.ts";\nexport function preview() { return import(spec); }\n`,
    );
    await withServer(directory, async (server) => {
      await expect(server.transformRequest(join(directory, "main.ts"))).rejects.toThrow(
        /runtime bundling failed|dynamic import|unresolved/,
      );
    });
  });

  it("keeps the self-types directive at the top after minification", async () => {
    const directory = await createFixture();
    await withServer(directory, async (server) => {
      const artifact = extractArtifact(await transformFile(server, join(directory, "main.ts")));
      expect(artifact.runtimeSource.startsWith('// @ts-self-types="./index.d.ts"')).toBe(true);
    });
  });

  it("bundles local imported, aliased, namespace, and re-exported declaration types", async () => {
    const directory = await createFixture();
    await writeFile(
      join(directory, "agent", "shapes.ts"),
      `/** A drawable shape. */\nexport interface Shape {\n  kind: "circle" | "square";\n}\nexport function createShape(): Shape { return { kind: "circle" }; }\n`,
    );
    await writeFile(
      join(directory, "agent", "internal.ts"),
      `export function helper(): string { return "secret-helper"; }\nexport function publicHelper(): string { return helper(); }\n`,
    );
    await writeFile(
      join(directory, "agent", "module.ts"),
      `import type * as Shapes from "./shapes";\nimport type { Shape as NamedShape } from "./shapes";\nexport type { NamedShape };\nexport type { Shape } from "./shapes";\nexport { publicHelper } from "./internal";\n/** Inspect a shape. */\nexport function inspect(shape: Shapes.Shape): NamedShape { return shape; }\n`,
    );
    await withServer(directory, async (server) => {
      const artifact = extractArtifact(await transformFile(server, join(directory, "main.ts")));
      expect(artifact.typesSource).toContain("Shapes.Shape");
      expect(artifact.typesSource).toContain("NamedShape");
      expect(artifact.typesSource).toContain("circle");
      expect(artifact.typesSource).toContain("A drawable shape");
      expect(artifact.typesSource).toContain("Inspect a shape");
      expect(artifact.typesSource).not.toMatch(/\bfrom\s*["']\./);
      expect(rootExportNames(artifact.typesSource).has("inspect")).toBe(true);
      expect(rootExportNames(artifact.typesSource).has("publicHelper")).toBe(true);
      expect(rootExportNames(artifact.typesSource).has("helper")).toBe(false);
      expect(standaloneDeclarationDiagnostics(artifact.typesSource)).toEqual([]);
    });
  });

  it("bundles a typed package entry and inlines its public declaration types", async () => {
    const directory = await createFixture();
    await createTypedPackageFixture(directory);
    await withServer(directory, async (server) => {
      const artifact = extractArtifact(await transformFile(server, join(directory, "package-main.ts")));
      expect(artifact.runtimeSource).toContain("front");
      expect(artifact.typesSource).toContain("PackageOptions");
      expect(artifact.typesSource).not.toContain('from "typed-dependency"');
      expect(standaloneDeclarationDiagnostics(artifact.typesSource)).toEqual([]);
    });
  });

  it("resolves a typed package whose declarations are exposed through exports", async () => {
    const directory = await createFixture();
    const packageDirectory = join(directory, "node_modules", "exports-dep");
    await mkdir(join(packageDirectory, "js"), { recursive: true });
    await mkdir(join(packageDirectory, "types"), { recursive: true });
    await writeFile(
      join(packageDirectory, "package.json"),
      JSON.stringify({
        name: "exports-dep",
        type: "module",
        exports: {
          ".": {
            types: "./types/index.d.ts",
            import: "./js/index.js",
            default: "./js/index.js",
          },
          "./extra": {
            types: "./types/extra.d.ts",
            import: "./js/extra.js",
            default: "./js/extra.js",
          },
        },
      }),
    );
    await writeFile(join(packageDirectory, "js", "index.js"), `export const label = "exports-root";\n`);
    await writeFile(join(packageDirectory, "js", "extra.js"), `export const extraLabel = "exports-extra";\n`);
    await writeFile(
      join(packageDirectory, "types", "index.d.ts"),
      `export type Label = "exports-root";\nexport declare const label: Label;\n`,
    );
    await writeFile(
      join(packageDirectory, "types", "extra.d.ts"),
      `export type ExtraLabel = "exports-extra";\nexport declare const extraLabel: ExtraLabel;\n`,
    );
    await writeFile(
      join(directory, "agent", "module.ts"),
      `import { label, type Label } from "exports-dep";\nimport { extraLabel, type ExtraLabel } from "exports-dep/extra";\nexport function describe(value: Label | ExtraLabel): string { return label + ":" + extraLabel + ":" + value; }\n`,
    );
    await withServer(directory, async (server) => {
      const artifact = extractArtifact(await transformFile(server, join(directory, "main.ts")));
      expect(artifact.runtimeSource).toContain("exports-root");
      expect(artifact.runtimeSource).toContain("exports-extra");
      expect(artifact.typesSource).toContain("exports-root");
      expect(artifact.typesSource).toContain("exports-extra");
      expect(artifact.typesSource).not.toContain("exports-dep");
      expect(standaloneDeclarationDiagnostics(artifact.typesSource)).toEqual([]);
    });
  });

  it("keeps the artifact hash independent of the absolute fixture directory", async () => {
    const first = await createFixture();
    const second = await createFixture();
    const firstCode = await withServer(first, (server) => transformFile(server, join(first, "main.ts")));
    const secondCode = await withServer(second, (server) => transformFile(server, join(second, "main.ts")));
    const firstArtifact = extractArtifact(firstCode);
    const secondArtifact = extractArtifact(secondCode);
    expect(firstArtifact.contentHash).toBe(secondArtifact.contentHash);
    expect(firstCode).not.toContain(first);
    expect(secondCode).not.toContain(second);
  });

  it("changes the hash when runtime, public types, or public JSDoc change, and keeps it stable otherwise", async () => {
    const directory = await createFixture();
    const original = await withServer(directory, (server) => transformFile(server, join(directory, "main.ts")));
    const originalHash = extractArtifact(original).contentHash;

    const identical = await withServer(directory, (server) => transformFile(server, join(directory, "main.ts")));
    expect(extractArtifact(identical).contentHash).toBe(originalHash);

    await writeFile(
      join(directory, "agent", "dependency.ts"),
      `export const suffix = "?";\nexport type Direction = "front" | "back";\n`,
    );
    const runtimeChanged = await withServer(directory, (server) => transformFile(server, join(directory, "main.ts")));
    const runtimeHash = extractArtifact(runtimeChanged).contentHash;
    expect(runtimeHash).not.toBe(originalHash);

    await writeFile(
      join(directory, "agent", "dependency.ts"),
      `export const suffix = "!";\nexport type Direction = "front" | "back";\n`,
    );
    await writeFile(
      join(directory, "agent", "module.ts"),
      `import { suffix } from "./dependency";\nexport interface PreviewOptions {\n  /** Integer multiplier from 1 through 8. */\n  scale?: number;\n  direction: "left" | "right";\n}\nexport function preview(value: string, options: PreviewOptions): string { return value + suffix + options.direction; }\n`,
    );
    const typeChanged = await withServer(directory, (server) => transformFile(server, join(directory, "main.ts")));
    const typeHash = extractArtifact(typeChanged).contentHash;
    expect(typeHash).not.toBe(originalHash);
    expect(typeHash).not.toBe(runtimeHash);

    await writeFile(
      join(directory, "agent", "module.ts"),
      `import { suffix } from "./dependency";\nexport interface PreviewOptions {\n  /** Updated multiplier docs. */\n  scale?: number;\n  direction: "left" | "right";\n}\nexport function preview(value: string, options: PreviewOptions): string { return value + suffix + options.direction; }\n`,
    );
    const jsdocChanged = await withServer(directory, (server) => transformFile(server, join(directory, "main.ts")));
    expect(extractArtifact(jsdocChanged).contentHash).not.toBe(typeHash);
  });

  it("invalidates owners when a runtime or type-only dependency changes, but not for unrelated files", async () => {
    const directory = await createFixture();
    await writeFile(
      join(directory, "agent", "types-only.ts"),
      `/** Public mode docs. */\nexport type Mode = "fast" | "slow";\n`,
    );
    await writeFile(
      join(directory, "agent", "module.ts"),
      `import type { Mode } from "./types-only";\nimport { suffix } from "./dependency";\nexport function preview(value: string, mode: Mode): string { return value + suffix + mode; }\n`,
    );
    await writeFile(join(directory, "unrelated.ts"), `export const unused = 1;\n`);

    await withServer(directory, async (server) => {
      const main = join(directory, "main.ts");
      const first = extractArtifact(await transformFile(server, main));

      await writeFile(join(directory, "agent", "dependency.ts"), `export const suffix = "?";\n`);
      const runtimeOwners = await triggerHotUpdate(server, join(directory, "agent", "dependency.ts"));
      expect(runtimeOwners && Array.isArray(runtimeOwners) && runtimeOwners.length > 0).toBe(true);
      const afterRuntime = extractArtifact(await transformFile(server, main));
      expect(afterRuntime.contentHash).not.toBe(first.contentHash);

      await writeFile(
        join(directory, "agent", "types-only.ts"),
        `/** Changed public mode docs. */\nexport type Mode = "fast" | "slow";\n`,
      );
      const typeOwners = await triggerHotUpdate(server, join(directory, "agent", "types-only.ts"));
      expect(typeOwners && Array.isArray(typeOwners) && typeOwners.length > 0).toBe(true);
      const afterType = extractArtifact(await transformFile(server, main));
      expect(afterType.contentHash).not.toBe(afterRuntime.contentHash);
      expect(afterType.typesSource).toContain("Changed public mode docs");

      const unrelatedOwners = await triggerHotUpdate(server, join(directory, "unrelated.ts"));
      expect(unrelatedOwners === undefined || (Array.isArray(unrelatedOwners) && unrelatedOwners.length === 0)).toBe(true);
      const afterUnrelated = extractArtifact(await transformFile(server, main));
      expect(afterUnrelated.contentHash).toBe(afterType.contentHash);
    });
  });

  it("prepares modules during a production Vite build", async () => {
    const directory = await createFixture();
    const output = await build({
      configFile: false,
      root: directory,
      plugins: [pedelecVitePlugin()],
      resolve: { alias: { "@kaoruisaac/pedelec": join(sdkDist, "index.js") } },
      build: {
        write: false,
        rollupOptions: { input: join(directory, "main.ts") },
      },
    });
    const outputs = Array.isArray(output) ? output.flatMap((item) => item.output) : output.output;
    const code = outputs.find((item) => item.type === "chunk")?.code ?? "";
    expect(code).toContain("__pedelecArtifact");
    expect(code).toContain("@ts-self-types");
  });

  it("does not pull Vite, TypeScript, or Node built-ins into a root SDK application graph", async () => {
    const directory = await createFixture();
    await writeFile(
      join(directory, "app.ts"),
      `import { Pedelec, defineDenoModule } from "@kaoruisaac/pedelec";\nexport { Pedelec, defineDenoModule };\n`,
    );
    const moduleIds: string[] = [];
    const output = await build({
      configFile: false,
      root: directory,
      plugins: [
        {
          name: "collect-module-ids",
          moduleParsed(info) {
            moduleIds.push(info.id);
          },
        },
      ],
      resolve: { alias: { "@kaoruisaac/pedelec": sdkRootEntry } },
      build: {
        write: false,
        rollupOptions: { input: join(directory, "app.ts") },
      },
    });
    const outputs = Array.isArray(output) ? output.flatMap((item) => item.output) : output.output;
    const code = outputs.filter((item) => item.type === "chunk").map((item) => item.code).join("\n");
    expect(moduleIds.some((id) => /(?:^|[\\/])vite\.(?:ts|js)$/i.test(id))).toBe(false);
    expect(moduleIds.some((id) => /[\\/]typescript[\\/]/i.test(id))).toBe(false);
    expect(code).not.toContain("pedelec-deno-module");
    expect(code).not.toContain('from "vite"');
    expect(code).not.toContain('from "typescript"');
    expect(code).not.toContain("node:fs");
    expect(code).not.toContain("node:path");
    expect(code).not.toContain("node:crypto");
    expect(code).not.toContain("node:module");
  });
});
