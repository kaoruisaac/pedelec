import { createHash } from "node:crypto";
import { builtinModules, createRequire } from "node:module";
import { dirname, extname, isAbsolute, normalize, resolve as resolvePath } from "node:path";

import * as ts from "typescript";
import { build, type HmrContext, type Plugin, type ResolvedConfig } from "vite";
import {
  PREPARED_DENO_MODULE_ARTIFACT_PROPERTY,
  type PreparedDenoModuleArtifact,
} from "./deno-module-internal.js";

const PACKAGE_NAME = "@kaoruisaac/pedelec";
const DEFINE_DENO_MODULE_NAME = "defineDenoModule";
const ARTIFACT_HASH_VERSION = "pedelec-deno-module-artifact-v1";
const SCRIPT_EXTENSIONS = new Set([".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"]);
const DECLARATION_LIKE_EXTENSIONS = new Set([".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".d.ts", ".json"]);
const LEGACY_NODE_BUILTIN_MODULES = new Set(builtinModules.filter((specifier) => !specifier.startsWith("node:")));
const NODE_BUILTIN_MODULES = new Set(builtinModules.map((specifier) => specifier.replace(/^node:/, "")));
const nodeRequire = createRequire(import.meta.url);
type HostResolver = ReturnType<ResolvedConfig["createResolver"]>;
const ssrResolvers = new WeakMap<ResolvedConfig, HostResolver>();

type Declaration = {
  name: string;
  description?: string;
  entry: string;
  usage?: string;
  callStart: number;
  callEnd: number;
  objectStart: number;
  objectEnd: number;
  entryStart: number;
  entryEnd: number;
};

type PreparedDeclaration = {
  artifact: PreparedDenoModuleArtifact;
  dependencies: Set<string>;
};

type DeclarationBundle = {
  source: string;
  files: Set<string>;
};

type DeclarationQueueItem = {
  fileName: string;
  packageBoundary: string | null;
};

type RuntimeBundle = {
  source: string;
  files: Set<string>;
};

type ResolveModule = (
  source: string,
  importer: string,
  options?: { skipSelf?: boolean },
) => Promise<{ id: string; external?: boolean | "absolute" } | null>;

/**
 * Vite build-time support for the static Deno module authoring API.
 *
 * The plugin deliberately keeps the generated module inside the transformed
 * browser module. The SDK can consume the non-enumerable private artifact in
 * a later transport phase without giving the runtime the original entry path.
 */
export function pedelecVitePlugin(): Plugin {
  const ownersByDependency = new Map<string, Set<string>>();
  const dependenciesByOwner = new Map<string, Set<string>>();
  let resolvedConfig: ResolvedConfig | undefined;

  return {
    name: "pedelec-deno-module",
    enforce: "pre",

    configResolved(config) {
      resolvedConfig = config;
    },

    async transform(code, id) {
      const sourceId = stripQueryAndHash(id);
      if (!isTransformableScript(sourceId)) return null;

      const sourceFile = parseSourceFile(code, sourceId);
      const importedNames = findDenoModuleImportNames(sourceFile);
      if (importedNames.size === 0) return null;

      const declarations = findDeclarations(sourceFile, importedNames);
      if (declarations.length === 0) return null;

      const prepared: Array<{ declaration: Declaration; result: PreparedDeclaration }> = [];
      const dependencies = new Set<string>();
      for (const declaration of declarations) {
        const result = await prepareDeclaration(
          declaration,
          sourceId,
          this.resolve.bind(this),
          resolvedConfig,
        );
        prepared.push({ declaration, result });
        for (const dependency of result.dependencies) dependencies.add(dependency);
      }

      registerOwner(sourceId, dependencies, ownersByDependency, dependenciesByOwner);

      const replacements = prepared.map(({ declaration, result }) => {
        const objectSource = replaceSourceRange(
          code.slice(declaration.objectStart, declaration.objectEnd),
          declaration.entryStart - declaration.objectStart,
          declaration.entryEnd - declaration.objectStart,
          "undefined",
        );
        const artifactSource = JSON.stringify(result.artifact);
        const replacement = [
          "(() => {",
          `const __pedelecModule = ${objectSource};`,
          // The authoring entry is a source-tree path. It is intentionally
          // removed before the object can reach any SDK transport code.
          "__pedelecModule.entry = undefined;",
          `Object.defineProperty(__pedelecModule, ${JSON.stringify(PREPARED_DENO_MODULE_ARTIFACT_PROPERTY)}, {`,
          `value: Object.freeze(${artifactSource}), enumerable: false, writable: false, configurable: false`,
          "});",
          "return __pedelecModule;",
          "})()",
        ].join("\n");
        return {
          start: declaration.callStart,
          end: declaration.callEnd,
          replacement,
        };
      });

      let transformed = code;
      for (const replacement of replacements.sort((a, b) => b.start - a.start)) {
        transformed = transformed.slice(0, replacement.start) + replacement.replacement + transformed.slice(replacement.end);
      }

      return {
        code: transformed,
        map: null,
      };
    },

    handleHotUpdate(context) {
      return invalidateOwners(context, ownersByDependency);
    },
  };
}

async function prepareDeclaration(
  declaration: Declaration,
  importer: string,
  resolveModule: ResolveModule,
  config: ResolvedConfig | undefined,
): Promise<PreparedDeclaration> {
  const entry = await resolveEntry(declaration, importer, resolveModule, config);
  const diagnosticPrefix = `Deno module "${declaration.name}" entry "${declaration.entry}" imported by "${importer}"`;

  let runtimeBundle: RuntimeBundle;
  try {
    runtimeBundle = await bundleRuntime(entry, resolveModule, config);
  } catch (error) {
    throw new Error(`${diagnosticPrefix}: runtime bundling failed: ${formatError(error)}`);
  }

  let declarationBundle: DeclarationBundle;
  try {
    declarationBundle = await bundleDeclarations(
      entry,
      declaration.entry,
      importer,
      diagnosticPrefix,
      resolveModule,
      config,
    );
  } catch (error) {
    throw new Error(`${diagnosticPrefix}: declaration generation failed: ${formatError(error)}`);
  }

  if (!hasMeaningfulDeclarationSurface(declarationBundle.source)) {
    throw new Error(
      `${diagnosticPrefix}: declaration generation produced no meaningful public exports`,
    );
  }

  const normalizedRuntime = ensureSelfTypesDirective(stripBundlerLocationComments(runtimeBundle.source));
  const normalizedTypes = normalizeDeclarationSource(declarationBundle.source);
  validateRolledUpDeclaration(normalizedTypes, diagnosticPrefix);
  const contentHash = createContentHash(normalizedRuntime, normalizedTypes);
  const dependencies = new Set<string>([...runtimeBundle.files, ...declarationBundle.files, entry]);

  return {
    artifact: {
      format: "esm",
      runtimeSource: normalizedRuntime,
      typesSource: normalizedTypes,
      contentHash,
    },
    dependencies: new Set([...dependencies].map(normalizeFile)),
  };
}

async function resolveEntry(
  declaration: Declaration,
  importer: string,
  resolveModule: ResolveModule,
  config: ResolvedConfig | undefined,
): Promise<string> {
  if (isRemoteOrSchemeEntry(declaration.entry)) {
    throw new Error(
      `Deno module "${declaration.name}" entry "${declaration.entry}" is not a local or package specifier; remote and scheme URLs are not supported`,
    );
  }

  let result: { id: string; external?: boolean | "absolute" } | null;
  try {
    result = await resolveHostModule(declaration.entry, importer, resolveModule, config);
  } catch (error) {
    throw new Error(
      `could not resolve entry "${declaration.entry}" for Deno module "${declaration.name}" from "${importer}": ${formatError(error)}`,
    );
  }

  if (!result || result.external || result.id.startsWith("\0")) {
    throw new Error(
      `could not resolve entry "${declaration.entry}" for Deno module "${declaration.name}" from "${importer}"`,
    );
  }

  return stripQueryAndHash(result.id);
}

async function bundleRuntime(
  entry: string,
  resolveModule: ResolveModule,
  config: ResolvedConfig | undefined,
): Promise<RuntimeBundle> {
  const resolverPlugin: Plugin = {
    name: "pedelec-deno-module-host-resolver",
    enforce: "pre",
    async resolveId(source, importer) {
      const builtin = canonicalBuiltinSpecifier(source);
      if (builtin) return { id: builtin, external: true };
      if (!importer || source.startsWith("\0")) return null;
      const resolved = await resolveHostModule(source, importer, resolveModule, config);
      if (!resolved) {
        throw new Error(`unresolved runtime dependency "${source}" from "${importer}"`);
      }
      if (resolved.external) {
        throw new Error(`runtime dependency "${source}" from "${importer}" resolved as external`);
      }
      return resolved.id;
    },
  };

  const result = await build({
    configFile: false,
    root: config?.root ?? dirname(entry),
    publicDir: false,
    logLevel: "silent",
    plugins: [resolverPlugin],
    resolve: {
      // The outer Vite project owns aliases and plugin resolution. The small
      // resolver plugin above forwards those decisions into this nested build.
      conditions: config?.resolve.conditions,
      mainFields: config?.resolve.mainFields,
    },
    ssr: {
      noExternal: true,
      target: "node",
    },
    build: {
      write: false,
      emptyOutDir: false,
      minify: "oxc",
      target: "esnext",
      sourcemap: false,
      cssCodeSplit: false,
      ssr: true,
      rolldownOptions: {
        input: entry,
        output: {
          format: "es",
          codeSplitting: false,
          entryFileNames: "index.mjs",
          chunkFileNames: "index-[hash].mjs",
          assetFileNames: "index-[hash][extname]",
        },
      },
    },
  });

  const outputBundles = Array.isArray(result) ? result : [result];
  const bundles = outputBundles.filter((item): item is Extract<typeof item, { output: unknown }> => "output" in item);
  if (bundles.length !== outputBundles.length) {
    throw new Error("bundler returned a watcher instead of an output bundle");
  }
  const outputs = bundles.flatMap((item) => item.output);
  const chunks = outputs.filter((item) => item.type === "chunk");
  const extraOutputs = outputs.filter((item) => item.type !== "chunk");
  if (extraOutputs.length > 0) {
    throw new Error(
      `runtime bundle produced additional outputs besides a single ESM chunk: ${extraOutputs
        .map((item) => "fileName" in item ? String(item.fileName) : "unknown")
        .join(", ")}`,
    );
  }
  if (chunks.length !== 1) {
    throw new Error(
      `runtime bundle produced ${chunks.length} chunks; code-splitting and secondary runtime chunks are not supported`,
    );
  }

  const entryChunk = chunks[0];
  if (!entryChunk.isEntry) {
    throw new Error("bundler did not produce an ESM entry chunk");
  }
  const invalidImports = entryChunk.imports.filter((specifier) => !isCanonicalBuiltinSpecifier(specifier));
  if (invalidImports.length > 0) {
    throw new Error(
      `runtime bundle left unresolved external dependencies: ${invalidImports.join(", ")}`,
    );
  }
  const invalidDynamicImports = entryChunk.dynamicImports.filter((specifier) => !isCanonicalBuiltinSpecifier(specifier));
  if (invalidDynamicImports.length > 0) {
    throw new Error(
      `runtime bundle left unresolved dynamic imports: ${invalidDynamicImports.join(", ")}`,
    );
  }
  const leftoverModules = leftoverRuntimeModuleSpecifiers(entryChunk.code).filter(
    (specifier) => !isCanonicalBuiltinSpecifier(specifier),
  );
  if (leftoverModules.length > 0) {
    throw new Error(
      `runtime bundle left unresolved module specifiers: ${leftoverModules.join(", ")}`,
    );
  }

  const files = new Set<string>();
  for (const id of entryChunk.moduleIds) {
    const file = stripQueryAndHash(id);
    if (!isWatchableSourceFile(file)) continue;
    files.add(normalizeFile(file));
  }

  return { source: entryChunk.code, files };
}

async function resolveHostModule(
  source: string,
  importer: string,
  resolveModule: ResolveModule,
  config: ResolvedConfig | undefined,
): Promise<{ id: string; external?: boolean | "absolute" } | null> {
  const resolved = await resolveModule(source, importer, { skipSelf: true });
  const resolvedFile = resolved ? stripQueryAndHash(resolved.id) : null;
  const needsPackageFallback = Boolean(
    isBareSpecifier(source) &&
      resolved &&
      (isOptimizedDependencyPath(resolved.id) || resolved.external || !ts.sys.fileExists(resolvedFile!)),
  );
  if (!needsPackageFallback) return resolved;

  if (config) {
    try {
      let resolver = ssrResolvers.get(config);
      if (!resolver) {
        resolver = config.createResolver();
        ssrResolvers.set(config, resolver);
      }
      const serverResolved = await resolver(source, importer, false, true);
      if (serverResolved) return { id: serverResolved };
    } catch {
      // Fall through to the legacy Node resolver. Custom Vite plugins can still
      // return a usable outer resolution even when Vite's internal SSR resolver
      // cannot resolve the specifier.
    }
  }

  try {
    return { id: nodeRequire.resolve(source, { paths: [dirname(importer)] }) };
  } catch {
    return resolved;
  }
}

async function bundleDeclarations(
  runtimeEntry: string,
  specifier: string,
  importer: string,
  diagnosticPrefix: string,
  resolveModule: ResolveModule,
  config: ResolvedConfig | undefined,
): Promise<DeclarationBundle> {
  const aliasOptions = createTypeScriptAliasOptions(config);
  const configFile = ts.findConfigFile(dirname(runtimeEntry), ts.sys.fileExists, "tsconfig.json");
  const baseOptions = configFile
    ? ts.parseJsonConfigFileContent(
        ts.readConfigFile(configFile, ts.sys.readFile).config,
        ts.sys,
        dirname(configFile),
      ).options
    : {};
  const options: ts.CompilerOptions = {
    ...baseOptions,
    ...aliasOptions,
    baseUrl: aliasOptions.baseUrl ?? baseOptions.baseUrl,
    paths: { ...baseOptions.paths, ...aliasOptions.paths },
    allowJs: true,
    checkJs: false,
    declaration: true,
    emitDeclarationOnly: true,
    declarationMap: false,
    noEmit: false,
    noEmitOnError: false,
    isolatedDeclarations: false,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    target: ts.ScriptTarget.ES2022,
    skipLibCheck: true,
    types: [],
    typeRoots: [],
    outDir: undefined,
    rootDir: undefined,
  };

  const host = ts.createCompilerHost(options);
  const typesEntry = resolveTypesEntry(runtimeEntry, specifier, importer, options, host);
  const resolutionCache = new Map<string, string>();
  const sourceFiles = new Set<string>([normalizeFile(typesEntry)]);
  // Local/module-owned declarations may reference more module-owned files. Once
  // traversal enters a package, keep it inside that package instead of crawling
  // the declarations of that package's own dependencies.
  const queue: DeclarationQueueItem[] = [{
    fileName: normalizeFile(typesEntry),
    packageBoundary: nodeModulesPackageRoot(typesEntry),
  }];

  while (queue.length > 0) {
    const { fileName, packageBoundary } = queue.pop()!;
    const source = readFileIfAvailable(fileName);
    if (source === null) {
      throw new Error(`${diagnosticPrefix}: could not read declaration source "${fileName}"`);
    }
    const sourceFile = parseSourceFile(source, fileName);
    for (const moduleSpecifier of collectModuleSpecifiers(sourceFile)) {
      if (isBuiltinSpecifier(moduleSpecifier)) continue;
      if (!canResolveDeclarationDependency(moduleSpecifier, packageBoundary)) continue;
      const resolved = await resolveDeclarationDependency(
        moduleSpecifier,
        fileName,
        options,
        host,
        resolveModule,
        config,
      );
      if (!resolved) {
        throw new Error(
          `${diagnosticPrefix}: public declaration still depends on unresolved module "${moduleSpecifier}"`,
        );
      }
      const resolvedPackageBoundary = nodeModulesPackageRoot(resolved);
      if (packageBoundary !== null && resolvedPackageBoundary !== packageBoundary) continue;
      resolutionCache.set(resolutionKey(fileName, moduleSpecifier), resolved);
      if (!sourceFiles.has(resolved)) {
        sourceFiles.add(resolved);
        if (packageBoundary === null || resolvedPackageBoundary === packageBoundary) {
          queue.push({
            fileName: resolved,
            packageBoundary: resolvedPackageBoundary,
          });
        }
      }
    }
  }

  host.resolveModuleNameLiterals = (moduleLiterals, containingFile, redirectedReference, compilerOptions) => {
    return moduleLiterals.map((literal) => {
      const cached = resolutionCache.get(resolutionKey(containingFile, literal.text));
      if (cached) return { resolvedModule: toResolvedModule(cached) };
      const packageBoundary = nodeModulesPackageRoot(containingFile);
      if (isBuiltinSpecifier(literal.text) || !canResolveDeclarationDependency(literal.text, packageBoundary)) {
        return { resolvedModule: undefined };
      }
      const resolved = ts.resolveModuleName(literal.text, containingFile, compilerOptions, host, undefined, redirectedReference);
      if (
        packageBoundary !== null &&
        resolved.resolvedModule &&
        nodeModulesPackageRoot(resolved.resolvedModule.resolvedFileName) !== packageBoundary
      ) {
        return { resolvedModule: undefined };
      }
      return resolved;
    });
  };

  const program = ts.createProgram([typesEntry], options, host);
  const diagnostics = ts
    .getPreEmitDiagnostics(program)
    .filter((item) => item.category === ts.DiagnosticCategory.Error)
    // Node types are not required when an implementation-only import disappears
    // from declaration emit. If it survives, roll-up validation below rejects it.
    .filter((item) => !isDeferredBuiltinResolutionDiagnostic(item));
  if (diagnostics.length > 0) {
    throw new Error(formatDiagnostics(diagnostics));
  }

  const emitted = new Map<string, string>();
  const emitResult = program.emit(
    undefined,
    (fileName, text) => emitted.set(normalizeFile(fileName), text),
    undefined,
    true,
  );
  if (emitResult.diagnostics.some((item) => item.category === ts.DiagnosticCategory.Error)) {
    throw new Error(formatDiagnostics(emitResult.diagnostics));
  }

  const declarationSources = new Map<string, string>();
  for (const sourceFile of program.getSourceFiles()) {
    const fileName = normalizeFile(sourceFile.fileName);
    if (isTypeScriptLib(fileName) || isIgnoredProgramFile(fileName)) continue;
    if (sourceFile.isDeclarationFile) {
      declarationSources.set(fileName, stripDeclarationSourceMap(sourceFile.text));
    }
  }
  for (const [fileName, text] of emitted) {
    if (fileName.endsWith(".d.ts") && text.length > 0) {
      declarationSources.set(fileName, stripDeclarationSourceMap(text));
    }
  }

  const rootDeclaration = lookupDeclarationSourceKey(typesEntry, declarationSources);
  if (!rootDeclaration) {
    throw new Error(`TypeScript did not emit a declaration file for ${typesEntry}`);
  }

  const source = rollupDeclarationModules(
    rootDeclaration,
    declarationSources,
    options,
    host,
    diagnosticPrefix,
  );
  const files = new Set<string>();
  for (const fileName of sourceFiles) files.add(fileName);
  for (const sourceFile of program.getSourceFiles()) {
    const fileName = normalizeFile(sourceFile.fileName);
    if (isTypeScriptLib(fileName) || isIgnoredProgramFile(fileName)) continue;
    if (isWatchableSourceFile(fileName)) files.add(fileName);
  }
  return { source, files };
}

function resolveTypesEntry(
  runtimeEntry: string,
  specifier: string,
  importer: string,
  options: ts.CompilerOptions,
  host: ts.CompilerHost,
): string {
  if (isDeclarationSourcePath(runtimeEntry)) return normalizeFile(runtimeEntry);

  const resolved = ts.resolveModuleName(specifier, importer, options, host);
  const fromTypeScript = resolved.resolvedModule?.resolvedFileName;
  if (fromTypeScript && !isTypeScriptLib(fromTypeScript)) return normalizeFile(fromTypeScript);

  const adjacent = adjacentDeclarationPath(runtimeEntry);
  if (adjacent && ts.sys.fileExists(adjacent)) return normalizeFile(adjacent);

  const fromPackage = typesFromPackageMetadata(runtimeEntry, specifier);
  if (fromPackage) return fromPackage;

  throw new Error(`could not resolve a declaration file for "${specifier}"`);
}

function typesFromPackageMetadata(runtimeEntry: string, specifier: string): string | null {
  if (!isNodeModulesPath(runtimeEntry)) return null;
  let directory = dirname(runtimeEntry);
  while (true) {
    const packageJsonPath = resolvePath(directory, "package.json");
    const contents = readFileIfAvailable(packageJsonPath);
    if (contents) {
      try {
        const parsed = JSON.parse(contents) as {
          name?: unknown;
          types?: unknown;
          typings?: unknown;
          exports?: unknown;
        };
        const subpath = packageSubpath(specifier, typeof parsed.name === "string" ? parsed.name : null);
        const fromExports = subpath === null ? null : typesFromExportsField(parsed.exports, subpath, directory);
        if (fromExports) return fromExports;
        if (subpath === ".") {
          const typeEntry = typeof parsed.types === "string" ? parsed.types : typeof parsed.typings === "string" ? parsed.typings : null;
          if (typeEntry) {
            const declaration = resolvePath(directory, typeEntry);
            if (ts.sys.fileExists(declaration)) return normalizeFile(declaration);
          }
        }
      } catch {
        // Malformed package metadata should not hide a later enclosing package.
      }
    }
    const parent = dirname(directory);
    if (parent === directory) break;
    directory = parent;
  }
  return null;
}

function packageSubpath(specifier: string, packageName: string | null): string | null {
  if (!packageName) return null;
  if (specifier === packageName) return ".";
  const prefix = `${packageName}/`;
  if (specifier.startsWith(prefix)) return `./${specifier.slice(prefix.length)}`;
  return null;
}

function typesFromExportsField(exportsField: unknown, subpath: string, packageDirectory: string): string | null {
  if (typeof exportsField === "string") {
    return subpath === "." ? resolveExportTarget(exportsField, packageDirectory) : null;
  }
  if (!exportsField || typeof exportsField !== "object" || Array.isArray(exportsField)) return null;
  const mapping = exportsField as Record<string, unknown>;
  const target = mapping[subpath] ?? (subpath === "." ? mapping["."] : undefined);
  return resolveExportTarget(target, packageDirectory);
}

function resolveExportTarget(target: unknown, packageDirectory: string): string | null {
  if (typeof target === "string") {
    const file = resolvePath(packageDirectory, target);
    return ts.sys.fileExists(file) ? normalizeFile(file) : null;
  }
  if (Array.isArray(target)) {
    for (const item of target) {
      const resolved = resolveExportTarget(item, packageDirectory);
      if (resolved) return resolved;
    }
    return null;
  }
  if (!target || typeof target !== "object") return null;
  const conditions = target as Record<string, unknown>;
  for (const condition of ["types", "typings", "import", "default"]) {
    if (condition in conditions) {
      const resolved = resolveExportTarget(conditions[condition], packageDirectory);
      if (resolved) {
        if (condition === "types" || condition === "typings" || resolved.endsWith(".d.ts") || isDeclarationSourcePath(resolved)) {
          return resolved;
        }
        const adjacent = adjacentDeclarationPath(resolved);
        if (adjacent && ts.sys.fileExists(adjacent)) return normalizeFile(adjacent);
      }
    }
  }
  return null;
}

async function resolveDeclarationDependency(
  specifier: string,
  importer: string,
  options: ts.CompilerOptions,
  host: ts.CompilerHost,
  resolveModule: ResolveModule,
  config: ResolvedConfig | undefined,
): Promise<string | null> {
  const fromTypeScript = ts.resolveModuleName(specifier, importer, options, host).resolvedModule?.resolvedFileName;
  if (fromTypeScript && !isTypeScriptLib(fromTypeScript) && isDeclarationLikePath(fromTypeScript)) {
    return normalizeFile(fromTypeScript);
  }

  const viteResolved = await resolveHostModule(specifier, importer, resolveModule, config);
  if (!viteResolved || viteResolved.external) return null;
  const resolvedId = stripQueryAndHash(viteResolved.id);
  if (!isDeclarationLikePath(resolvedId)) return null;
  if (isDeclarationSourcePath(resolvedId)) return normalizeFile(resolvedId);

  const typesEntry = (() => {
    try {
      return resolveTypesEntry(resolvedId, specifier, importer, options, host);
    } catch {
      const adjacent = adjacentDeclarationPath(resolvedId);
      return adjacent && ts.sys.fileExists(adjacent) ? normalizeFile(adjacent) : null;
    }
  })();
  return typesEntry;
}

function rollupDeclarationModules(
  rootFile: string,
  sources: Map<string, string>,
  options: ts.CompilerOptions,
  host: ts.CompilerHost,
  diagnosticPrefix: string,
): string {
  const files = collectDeclarationGraph(rootFile, sources, options, host, diagnosticPrefix);
  const namespaceNames = new Map<string, string>();
  let namespaceCount = 0;
  for (const fileName of files) {
    if (fileName === rootFile) continue;
    namespaceNames.set(fileName, `__PedelecDts_${namespaceCount++}`);
  }

  const exportedNames = computeExportedNames(files, sources, options, host, diagnosticPrefix);
  const namespaceBlocks: string[] = [];
  for (const fileName of files) {
    if (fileName === rootFile) continue;
    namespaceBlocks.push(
      emitNamespaceModule(
        fileName,
        namespaceNames.get(fileName)!,
        sources,
        namespaceNames,
        exportedNames,
        options,
        host,
        diagnosticPrefix,
      ),
    );
  }

  const rootSource = emitRootModule(
    rootFile,
    sources,
    namespaceNames,
    exportedNames,
    options,
    host,
    diagnosticPrefix,
  );
  const combined = [...namespaceBlocks.filter(Boolean), rootSource].join("\n").trim();
  assertNoExternalModuleSpecifiers(combined, diagnosticPrefix);
  return combined;
}

function collectDeclarationGraph(
  rootFile: string,
  sources: Map<string, string>,
  options: ts.CompilerOptions,
  host: ts.CompilerHost,
  diagnosticPrefix: string,
): string[] {
  const ordered: string[] = [];
  const visiting = new Set<string>();
  const visited = new Set<string>();

  const visit = (fileName: string) => {
    if (visited.has(fileName) || visiting.has(fileName)) return;
    visiting.add(fileName);
    const source = sources.get(fileName);
    if (source === undefined) {
      throw new Error(`${diagnosticPrefix}: missing declaration source for "${fileName}"`);
    }
    const sourceFile = ts.createSourceFile(fileName, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
    const packageBoundary = nodeModulesPackageRoot(fileName);
    for (const specifier of collectModuleSpecifiers(sourceFile)) {
      if (!canResolveDeclarationDependency(specifier, packageBoundary)) {
        throw new Error(
          `${diagnosticPrefix}: public declaration still depends on unresolved module "${specifier}"`,
        );
      }
      const resolved = resolveExistingDeclaration(specifier, fileName, sources, options, host);
      if (!resolved || packageBoundary !== null && nodeModulesPackageRoot(resolved) !== packageBoundary) {
        throw new Error(
          `${diagnosticPrefix}: public declaration still depends on unresolved module "${specifier}"`,
        );
      }
      visit(resolved);
    }
    visiting.delete(fileName);
    visited.add(fileName);
    ordered.push(fileName);
  };

  visit(rootFile);
  return ordered;
}

function computeExportedNames(
  files: string[],
  sources: Map<string, string>,
  options: ts.CompilerOptions,
  host: ts.CompilerHost,
  diagnosticPrefix: string,
): Map<string, Set<string>> {
  const exportsByFile = new Map<string, Set<string>>();
  const starExports = new Map<string, string[]>();

  for (const fileName of files) {
    const names = new Set<string>();
    const stars: string[] = [];
    const sourceFile = ts.createSourceFile(fileName, sources.get(fileName)!, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
    for (const statement of sourceFile.statements) {
      if (ts.isExportDeclaration(statement)) {
        if (statement.moduleSpecifier && ts.isStringLiteral(statement.moduleSpecifier) && !statement.exportClause) {
          const resolved = resolveExistingDeclaration(statement.moduleSpecifier.text, fileName, sources, options, host);
          if (!resolved) {
            throw new Error(
              `${diagnosticPrefix}: public declaration still depends on unresolved module "${statement.moduleSpecifier.text}"`,
            );
          }
          stars.push(resolved);
          continue;
        }
        if (statement.exportClause && ts.isNamedExports(statement.exportClause)) {
          for (const element of statement.exportClause.elements) names.add(element.name.text);
        }
        continue;
      }
      if (ts.isExportAssignment(statement)) {
        names.add("default");
        continue;
      }
      if (hasExportModifier(statement)) {
        const name = declarationName(statement);
        if (name) names.add(name);
        if (hasDefaultModifier(statement)) names.add("default");
      }
    }
    exportsByFile.set(fileName, names);
    starExports.set(fileName, stars);
  }

  let changed = true;
  while (changed) {
    changed = false;
    for (const fileName of files) {
      const names = exportsByFile.get(fileName)!;
      for (const dependency of starExports.get(fileName) ?? []) {
        for (const name of exportsByFile.get(dependency) ?? []) {
          if (name === "default" || names.has(name)) continue;
          names.add(name);
          changed = true;
        }
      }
    }
  }

  return exportsByFile;
}

function emitNamespaceModule(
  fileName: string,
  namespaceName: string,
  sources: Map<string, string>,
  namespaceNames: Map<string, string>,
  exportedNames: Map<string, Set<string>>,
  options: ts.CompilerOptions,
  host: ts.CompilerHost,
  diagnosticPrefix: string,
): string {
  const body = emitModuleBody(
    fileName,
    sources,
    namespaceNames,
    exportedNames,
    options,
    host,
    diagnosticPrefix,
    true,
  );
  if (!body.trim()) return `declare namespace ${namespaceName} {\n}\n`;
  return `declare namespace ${namespaceName} {\n${indentBlock(body)}\n}\n`;
}

function emitRootModule(
  fileName: string,
  sources: Map<string, string>,
  namespaceNames: Map<string, string>,
  exportedNames: Map<string, Set<string>>,
  options: ts.CompilerOptions,
  host: ts.CompilerHost,
  diagnosticPrefix: string,
): string {
  return emitModuleBody(fileName, sources, namespaceNames, exportedNames, options, host, diagnosticPrefix, false);
}

function emitModuleBody(
  fileName: string,
  sources: Map<string, string>,
  namespaceNames: Map<string, string>,
  exportedNames: Map<string, Set<string>>,
  options: ts.CompilerOptions,
  host: ts.CompilerHost,
  diagnosticPrefix: string,
  inNamespace: boolean,
): string {
  const sourceFile = ts.createSourceFile(fileName, sources.get(fileName)!, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
  const parts: string[] = [];

  for (const statement of sourceFile.statements) {
    if (ts.isImportDeclaration(statement) || ts.isExportDeclaration(statement) && statement.moduleSpecifier) {
      parts.push(
        ...rewriteModuleStatement(
          statement,
          fileName,
          sourceFile,
          sources,
          namespaceNames,
          exportedNames,
          options,
          host,
          diagnosticPrefix,
        ),
      );
      continue;
    }
    if (ts.isImportEqualsDeclaration(statement) && statement.moduleReference && ts.isExternalModuleReference(statement.moduleReference)) {
      throw new Error(`${diagnosticPrefix}: CommonJS import= declarations cannot be bundled into a standalone declaration`);
    }
    if (ts.isExportAssignment(statement) && statement.isExportEquals) {
      throw new Error(`${diagnosticPrefix}: CommonJS export= declarations cannot be bundled into a standalone declaration`);
    }

    const text = statementText(statement, sourceFile);
    if (inNamespace && ts.isExportAssignment(statement) && ts.isIdentifier(statement.expression)) {
      parts.push(`export { ${statement.expression.text} as default };`);
      continue;
    }
    if (inNamespace && hasDefaultModifier(statement)) {
      const name = declarationName(statement);
      if (!name) {
        throw new Error(`${diagnosticPrefix}: anonymous default export cannot be bundled into a standalone declaration`);
      }
      parts.push(adjustDeclarationForNamespace(text));
      parts.push(`export { ${name} as default };`);
      continue;
    }
    parts.push(inNamespace ? adjustDeclarationForNamespace(text) : text);
  }

  return parts.filter((part) => part.trim().length > 0).join("\n");
}

function rewriteModuleStatement(
  statement: ts.ImportDeclaration | ts.ExportDeclaration,
  fileName: string,
  sourceFile: ts.SourceFile,
  sources: Map<string, string>,
  namespaceNames: Map<string, string>,
  exportedNames: Map<string, Set<string>>,
  options: ts.CompilerOptions,
  host: ts.CompilerHost,
  diagnosticPrefix: string,
): string[] {
  const specifier = statement.moduleSpecifier && ts.isStringLiteral(statement.moduleSpecifier)
    ? statement.moduleSpecifier.text
    : null;
  if (!specifier) return [statementText(statement, sourceFile)];

  const resolved = resolveExistingDeclaration(specifier, fileName, sources, options, host);
  if (!resolved) {
    throw new Error(`${diagnosticPrefix}: public declaration still depends on unresolved module "${specifier}"`);
  }
  const namespaceName = namespaceNames.get(resolved);
  if (!namespaceName) {
    throw new Error(`${diagnosticPrefix}: could not inline declaration module "${specifier}"`);
  }

  if (ts.isImportDeclaration(statement)) {
    return rewriteImportDeclaration(statement, namespaceName, diagnosticPrefix);
  }
  return rewriteExportDeclaration(statement, namespaceName, exportedNames.get(resolved) ?? new Set(), diagnosticPrefix);
}

function rewriteImportDeclaration(statement: ts.ImportDeclaration, namespaceName: string, diagnosticPrefix: string): string[] {
  if (!statement.importClause) {
    throw new Error(`${diagnosticPrefix}: side-effect imports cannot be bundled into a standalone declaration`);
  }
  const lines: string[] = [];
  const clause = statement.importClause;
  if (clause.name) {
    lines.push(`import ${clause.name.text} = ${namespaceName}.default;`);
  }
  const bindings = clause.namedBindings;
  if (bindings && ts.isNamespaceImport(bindings)) {
    lines.push(`import ${bindings.name.text} = ${namespaceName};`);
  } else if (bindings && ts.isNamedImports(bindings)) {
    for (const element of bindings.elements) {
      const imported = element.propertyName?.text ?? element.name.text;
      lines.push(`import ${element.name.text} = ${namespaceName}.${imported};`);
    }
  }
  return lines;
}

function rewriteExportDeclaration(
  statement: ts.ExportDeclaration,
  namespaceName: string,
  exported: Set<string>,
  diagnosticPrefix: string,
): string[] {
  if (!statement.exportClause) {
    return [...exported]
      .filter((name) => name !== "default" && isIdentifierName(name))
      .map((name) => `export import ${name} = ${namespaceName}.${name};`);
  }
  if (ts.isNamespaceExport(statement.exportClause)) {
    return [`export import ${statement.exportClause.name.text} = ${namespaceName};`];
  }
  if (!ts.isNamedExports(statement.exportClause)) {
    throw new Error(`${diagnosticPrefix}: unsupported export declaration could not be bundled`);
  }
  return statement.exportClause.elements.map((element) => {
    const imported = element.propertyName?.text ?? element.name.text;
    return `export import ${element.name.text} = ${namespaceName}.${imported};`;
  });
}

function resolveExistingDeclaration(
  specifier: string,
  importer: string,
  sources: Map<string, string>,
  options: ts.CompilerOptions,
  host: ts.CompilerHost,
): string | null {
  const resolved = ts.resolveModuleName(specifier, importer, options, host).resolvedModule?.resolvedFileName;
  if (resolved) {
    const matched = lookupDeclarationSourceKey(resolved, sources);
    if (matched) return matched;
  }
  if (isRelativeSpecifier(specifier)) {
    for (const candidate of declarationCandidates(resolvePath(dirname(importer), specifier))) {
      const matched = lookupDeclarationSourceKey(candidate, sources);
      if (matched) return matched;
    }
  }
  return null;
}

function lookupDeclarationSourceKey(fileName: string, sources: Map<string, string>): string | null {
  const normalized = normalizeFile(fileName);
  if (sources.has(normalized)) return normalized;
  if (normalized.endsWith(".d.ts")) return null;
  const declarationPath = normalizeFile(`${withoutExtension(normalized)}.d.ts`);
  if (sources.has(declarationPath)) return declarationPath;
  const lower = declarationPath.toLowerCase();
  return [...sources.keys()].find((key) => key.toLowerCase() === lower) ?? null;
}

function collectModuleSpecifiers(sourceFile: ts.SourceFile): string[] {
  const specifiers: string[] = [];
  const visit = (node: ts.Node) => {
    if ((ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) && node.moduleSpecifier && ts.isStringLiteral(node.moduleSpecifier)) {
      specifiers.push(node.moduleSpecifier.text);
    } else if (ts.isImportEqualsDeclaration(node) && ts.isExternalModuleReference(node.moduleReference) && node.moduleReference.expression && ts.isStringLiteral(node.moduleReference.expression)) {
      specifiers.push(node.moduleReference.expression.text);
    } else if (ts.isImportTypeNode(node) && ts.isLiteralTypeNode(node.argument) && ts.isStringLiteral(node.argument.literal)) {
      specifiers.push(node.argument.literal.text);
    } else if (ts.isCallExpression(node) && node.expression.kind === ts.SyntaxKind.ImportKeyword) {
      const argument = node.arguments[0];
      if (argument && ts.isStringLiteral(argument)) specifiers.push(argument.text);
    }
    ts.forEachChild(node, visit);
  };
  visit(sourceFile);
  return specifiers;
}

function statementText(statement: ts.Node, sourceFile: ts.SourceFile): string {
  const start = statement.getStart(sourceFile, true);
  return sourceFile.text.slice(start, statement.end).trim();
}

function adjustDeclarationForNamespace(text: string): string {
  return text
    .replace(/^((?:\/\*\*[\s\S]*?\*\/\s*)*)export\s+default\s+/, "$1export ")
    .replace(/^((?:\/\*\*[\s\S]*?\*\/\s*)*)export\s+declare\s+/, "$1export ")
    .replace(/^((?:\/\*\*[\s\S]*?\*\/\s*)*)declare\s+/, "$1");
}

function indentBlock(text: string): string {
  return text
    .split("\n")
    .map((line) => (line.length === 0 ? line : `  ${line}`))
    .join("\n");
}

function hasExportModifier(node: ts.Node): boolean {
  return (ts.getCombinedModifierFlags(node as ts.Declaration) & ts.ModifierFlags.Export) !== 0;
}

function hasDefaultModifier(node: ts.Node): boolean {
  return (ts.getCombinedModifierFlags(node as ts.Declaration) & ts.ModifierFlags.Default) !== 0;
}

function declarationName(statement: ts.Statement): string | null {
  if (
    ts.isFunctionDeclaration(statement) ||
    ts.isClassDeclaration(statement) ||
    ts.isInterfaceDeclaration(statement) ||
    ts.isTypeAliasDeclaration(statement) ||
    ts.isEnumDeclaration(statement) ||
    ts.isModuleDeclaration(statement)
  ) {
    return statement.name && ts.isIdentifier(statement.name) ? statement.name.text : null;
  }
  if (ts.isVariableStatement(statement)) {
    const name = statement.declarationList.declarations[0]?.name;
    return name && ts.isIdentifier(name) ? name.text : null;
  }
  return null;
}

function isIdentifierName(name: string): boolean {
  return /^[A-Za-z_$][\w$]*$/.test(name);
}

function assertNoExternalModuleSpecifiers(source: string, diagnosticPrefix: string): void {
  const sourceFile = ts.createSourceFile("index.d.ts", source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
  const visit = (node: ts.Node) => {
    if ((ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) && node.moduleSpecifier && ts.isStringLiteral(node.moduleSpecifier)) {
      throw new Error(
        `${diagnosticPrefix}: rolled-up declaration still depends on module "${node.moduleSpecifier.text}"`,
      );
    }
    if (ts.isImportEqualsDeclaration(node) && ts.isExternalModuleReference(node.moduleReference) && node.moduleReference.expression && ts.isStringLiteral(node.moduleReference.expression)) {
      throw new Error(
        `${diagnosticPrefix}: rolled-up declaration still depends on module "${node.moduleReference.expression.text}"`,
      );
    }
    if (ts.isImportTypeNode(node) && ts.isLiteralTypeNode(node.argument) && ts.isStringLiteral(node.argument.literal)) {
      throw new Error(
        `${diagnosticPrefix}: rolled-up declaration still depends on module "${node.argument.literal.text}"`,
      );
    }
    ts.forEachChild(node, visit);
  };
  visit(sourceFile);
}

function validateRolledUpDeclaration(source: string, diagnosticPrefix: string): void {
  const options: ts.CompilerOptions = {
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.ESNext,
    // The artifact is itself a .d.ts file; skipLibCheck would skip its semantic check.
    skipLibCheck: false,
    noEmit: true,
    noResolve: true,
    types: [],
    typeRoots: [],
    strict: true,
  };
  const host = ts.createCompilerHost(options);
  const fileName = resolvePath(host.getCurrentDirectory(), "__pedelec_artifact__.d.ts");
  const originalGetSourceFile = host.getSourceFile.bind(host);
  const originalReadFile = host.readFile.bind(host);
  const originalFileExists = host.fileExists.bind(host);
  host.fileExists = (name) => name.includes("__pedelec_artifact__") || originalFileExists(name);
  host.readFile = (name) => (name.includes("__pedelec_artifact__") ? source : originalReadFile(name));
  host.getSourceFile = (name, languageVersion, onError, shouldCreateNewSourceFile) => {
    if (name.includes("__pedelec_artifact__")) {
      return ts.createSourceFile(fileName, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
    }
    return originalGetSourceFile(name, languageVersion, onError, shouldCreateNewSourceFile);
  };

  const program = ts.createProgram([fileName], options, host);
  const diagnostics = ts
    .getPreEmitDiagnostics(program)
    .filter((item) => item.category === ts.DiagnosticCategory.Error)
    .filter((item) => !item.file || !isTypeScriptLib(item.file.fileName));
  if (diagnostics.length > 0) {
    throw new Error(
      `${diagnosticPrefix}: rolled-up declaration is not a valid standalone public declaration: ${formatDiagnostics(diagnostics)}`,
    );
  }
}

function createTypeScriptAliasOptions(config: ResolvedConfig | undefined): Pick<ts.CompilerOptions, "baseUrl" | "paths"> {
  if (!config || !Array.isArray(config.resolve.alias)) return {};
  const aliases = config.resolve.alias;

  const paths: Record<string, string[]> = {};
  for (const alias of aliases) {
    if (typeof alias.find !== "string" || typeof alias.replacement !== "string") continue;
    const replacement = isAbsolute(alias.replacement)
      ? alias.replacement
      : resolvePath(config.root, alias.replacement);
    const exactName = alias.find.endsWith("$") ? alias.find.slice(0, -1) : alias.find;
    paths[exactName] = [replacement];
    if (!alias.find.endsWith("$")) paths[`${exactName}/*`] = [`${replacement}/*`];
  }
  return Object.keys(paths).length === 0 ? {} : { baseUrl: config.root, paths };
}

function isNodeModulesPath(fileName: string): boolean {
  return /[\\/]node_modules[\\/]/.test(fileName);
}

function declarationCandidates(path: string): string[] {
  const normalized = normalizeFile(path);
  const withoutDts = normalized.endsWith(".d.ts") ? normalized.slice(0, -5) : normalized;
  return [
    normalized,
    `${withoutDts}.d.ts`,
    `${withoutDts}.ts`,
    `${withoutDts}.tsx`,
    `${withoutDts}.js`,
    `${withoutDts}.jsx`,
    `${withoutDts}/index.d.ts`,
  ].map(normalizeFile);
}

function findDenoModuleImportNames(sourceFile: ts.SourceFile): Set<string> {
  const names = new Set<string>();
  for (const statement of sourceFile.statements) {
    if (!ts.isImportDeclaration(statement) || !ts.isStringLiteral(statement.moduleSpecifier)) continue;
    if (statement.moduleSpecifier.text !== PACKAGE_NAME || !statement.importClause || statement.importClause.isTypeOnly) continue;
    const namedBindings = statement.importClause.namedBindings;
    if (!namedBindings || !ts.isNamedImports(namedBindings)) continue;
    for (const element of namedBindings.elements) {
      if (element.isTypeOnly || element.propertyName?.text !== DEFINE_DENO_MODULE_NAME && element.name.text !== DEFINE_DENO_MODULE_NAME) continue;
      const importedName = element.propertyName?.text ?? element.name.text;
      if (importedName === DEFINE_DENO_MODULE_NAME) names.add(element.name.text);
    }
  }
  return names;
}

function findDeclarations(sourceFile: ts.SourceFile, importedNames: Set<string>): Declaration[] {
  const declarations: Declaration[] = [];
  const visit = (node: ts.Node) => {
    if (
      ts.isCallExpression(node) &&
      ts.isIdentifier(node.expression) &&
      importedNames.has(node.expression.text) &&
      !isShadowedCall(node, node.expression.text, sourceFile)
    ) {
      declarations.push(readDeclaration(node, sourceFile));
    }
    ts.forEachChild(node, visit);
  };
  ts.forEachChild(sourceFile, visit);
  return declarations;
}

function isShadowedCall(node: ts.CallExpression, name: string, sourceFile: ts.SourceFile): boolean {
  let parent: ts.Node | undefined = node.parent;
  while (parent && parent !== sourceFile) {
    if (ts.isFunctionLike(parent) && parent.parameters.some((parameter) => bindingNames(parameter.name).has(name))) return true;
    if (ts.isCatchClause(parent) && parent.variableDeclaration && bindingNames(parent.variableDeclaration.name).has(name)) return true;
    if (ts.isBlock(parent) && blockBindings(parent).has(name)) return true;
    parent = parent.parent;
  }
  return false;
}

function blockBindings(block: ts.Block): Set<string> {
  const names = new Set<string>();
  for (const statement of block.statements) {
    if (ts.isVariableStatement(statement)) {
      for (const declaration of statement.declarationList.declarations) {
        for (const name of bindingNames(declaration.name)) names.add(name);
      }
    } else if ((ts.isFunctionDeclaration(statement) || ts.isClassDeclaration(statement)) && statement.name) {
      names.add(statement.name.text);
    }
  }
  return names;
}

function bindingNames(name: ts.BindingName): Set<string> {
  const names = new Set<string>();
  const visit = (node: ts.BindingName) => {
    if (ts.isIdentifier(node)) {
      names.add(node.text);
      return;
    }
    ts.forEachChild(node, (child) => {
      if (ts.isBindingName(child)) visit(child);
    });
  };
  visit(name);
  return names;
}

function readDeclaration(node: ts.CallExpression, sourceFile: ts.SourceFile): Declaration {
  const argument = node.arguments[0];
  if (!argument || !ts.isObjectLiteralExpression(argument) || node.arguments.length !== 1) {
    throw declarationError(sourceFile, node, "defineDenoModule() requires one object literal argument");
  }

  const values = new Map<string, ts.Expression>();
  for (const property of argument.properties) {
    if (ts.isShorthandPropertyAssignment(property)) {
      values.set(property.name.text, property.name);
      continue;
    }
    if (!ts.isPropertyAssignment(property)) {
      throw declarationError(sourceFile, property, "defineDenoModule() fields must be statically readable property assignments");
    }
    const name = getPropertyName(property.name);
    if (name) values.set(name, property.initializer);
  }

  const name = readStringField(values, "name", sourceFile, node);
  const description = readOptionalStringField(values, "description", sourceFile, node);
  if (values.has("code")) {
    throw declarationError(sourceFile, values.get("code")!, "defineDenoModule() does not support inline code; use a static entry");
  }
  const entryExpression = values.get("entry");
  if (!entryExpression) throw declarationError(sourceFile, node, "defineDenoModule() requires an entry string literal");
  if (ts.isTemplateExpression(entryExpression)) {
    throw declarationError(sourceFile, entryExpression, "defineDenoModule() entry cannot contain template substitutions");
  }
  const entry = readStaticString(entryExpression);
  if (entry === null) throw declarationError(sourceFile, entryExpression, "defineDenoModule() entry must be a string literal");
  const usage = readOptionalStringField(values, "usage", sourceFile, node);
  return {
    name,
    description,
    entry,
    usage,
    callStart: node.getStart(sourceFile),
    callEnd: node.end,
    objectStart: argument.getStart(sourceFile),
    objectEnd: argument.end,
    entryStart: entryExpression.getStart(sourceFile),
    entryEnd: entryExpression.end,
  };
}

function readOptionalStringField(
  values: Map<string, ts.Expression>,
  field: string,
  sourceFile: ts.SourceFile,
  node: ts.Node,
): string | undefined {
  if (!values.has(field)) return undefined;
  const expression = values.get(field)!;
  const value = readStaticString(expression);
  if (value === null) {
    throw declarationError(sourceFile, expression ?? node, `defineDenoModule() field must be a static string when provided: ${field}`);
  }
  return value;
}

function replaceSourceRange(source: string, start: number, end: number, replacement: string): string {
  return source.slice(0, start) + replacement + source.slice(end);
}

function readStringField(
  values: Map<string, ts.Expression>,
  field: string,
  sourceFile: ts.SourceFile,
  node: ts.Node,
): string {
  const expression = values.get(field);
  const value = expression ? readStaticString(expression) : null;
  if (value === null) throw declarationError(sourceFile, expression ?? node, `defineDenoModule() requires a static string field: ${field}`);
  return value;
}

function readStaticString(expression: ts.Expression): string | null {
  if (ts.isStringLiteral(expression) || ts.isNoSubstitutionTemplateLiteral(expression)) return expression.text;
  return null;
}

function getPropertyName(name: ts.PropertyName): string | null {
  if (ts.isIdentifier(name) || ts.isStringLiteral(name) || ts.isNumericLiteral(name)) return name.text;
  return null;
}

function declarationError(sourceFile: ts.SourceFile, node: ts.Node, message: string): Error {
  const position = sourceFile.getLineAndCharacterOfPosition(node.getStart(sourceFile));
  return new Error(`${sourceFile.fileName}:${position.line + 1}:${position.character + 1}: ${message}`);
}

function parseSourceFile(code: string, id: string): ts.SourceFile {
  const extension = extname(id).toLowerCase();
  const scriptKind = extension === ".tsx" ? ts.ScriptKind.TSX : extension === ".jsx" ? ts.ScriptKind.JSX : extension === ".js" || extension === ".mjs" || extension === ".cjs" ? ts.ScriptKind.JS : ts.ScriptKind.TS;
  return ts.createSourceFile(id, code, ts.ScriptTarget.Latest, true, scriptKind);
}

function isTransformableScript(id: string): boolean {
  return SCRIPT_EXTENSIONS.has(extname(id).toLowerCase());
}

function isRemoteOrSchemeEntry(entry: string): boolean {
  return /^[a-z][a-z\d+.-]*:/i.test(entry) || entry.startsWith("//") || isAbsolute(entry);
}

function stripQueryAndHash(id: string): string {
  return id.split(/[?#]/, 1)[0];
}

function normalizeFile(fileName: string): string {
  return normalize(resolvePath(fileName));
}

function withoutExtension(fileName: string): string {
  return fileName.endsWith(".d.ts") ? fileName.slice(0, -5) : fileName.slice(0, fileName.length - extname(fileName).length);
}

function adjacentDeclarationPath(fileName: string): string | null {
  if (fileName.endsWith(".d.ts")) return fileName;
  if (!/\.(?:[cm]?[jt]sx?)$/i.test(fileName)) return null;
  return `${fileName.replace(/\.[^.]+$/, "")}.d.ts`;
}

function isDeclarationSourcePath(fileName: string): boolean {
  return fileName.endsWith(".d.ts") || /\.[cm]?tsx?$/i.test(fileName);
}

function isDeclarationLikePath(fileName: string): boolean {
  const extension = fileName.endsWith(".d.ts") ? ".d.ts" : extname(fileName).toLowerCase();
  return DECLARATION_LIKE_EXTENSIONS.has(extension);
}

function isTypeScriptLib(fileName: string): boolean {
  return /[\\/]typescript[\\/]lib[\\/]/i.test(fileName);
}

function isIgnoredProgramFile(fileName: string): boolean {
  return /[\\/]@types[\\/]/i.test(fileName);
}

function isWatchableSourceFile(fileName: string): boolean {
  if (!fileName || fileName.startsWith("\0") || fileName.startsWith("virtual:") || fileName.startsWith("data:")) return false;
  if (!isAbsolute(fileName) && !/^[A-Za-z]:[\\/]/.test(fileName)) return false;
  return ts.sys.fileExists(fileName);
}

function isRelativeSpecifier(specifier: string): boolean {
  return specifier.startsWith(".") || specifier.startsWith("/");
}

function isBareSpecifier(specifier: string): boolean {
  return !isRelativeSpecifier(specifier) && !specifier.startsWith("#") && !specifier.startsWith("node:");
}

function isOptimizedDependencyPath(fileName: string): boolean {
  return /[\\/]\.vite[\\/]deps[\\/]/.test(fileName);
}

function nodeModulesPackageRoot(fileName: string): string | null {
  const normalized = normalizeFile(fileName);
  const segments = normalized.split(/[\\/]/);
  const nodeModulesIndex = segments.lastIndexOf("node_modules");
  if (nodeModulesIndex < 0 || nodeModulesIndex + 1 >= segments.length) return null;
  const packageEnd = segments[nodeModulesIndex + 1].startsWith("@") ? nodeModulesIndex + 3 : nodeModulesIndex + 2;
  if (packageEnd > segments.length) return null;
  return normalizeFile(segments.slice(0, packageEnd).join("/"));
}

function canResolveDeclarationDependency(specifier: string, packageBoundary: string | null): boolean {
  if (packageBoundary === null || isRelativeSpecifier(specifier) || specifier.startsWith("#")) return true;
  const packageJson = readFileIfAvailable(resolvePath(packageBoundary, "package.json"));
  if (!packageJson) return false;
  try {
    const packageName = (JSON.parse(packageJson) as { name?: unknown }).name;
    return typeof packageName === "string" && (specifier === packageName || specifier.startsWith(`${packageName}/`));
  } catch {
    return false;
  }
}

function isBuiltinSpecifier(specifier: string): boolean {
  return canonicalBuiltinSpecifier(specifier) !== null;
}

function isDeferredBuiltinResolutionDiagnostic(diagnostic: ts.Diagnostic): boolean {
  if (diagnostic.code !== 2307) return false;
  const message = ts.flattenDiagnosticMessageText(diagnostic.messageText, "\n");
  const match = /Cannot find module ['"]([^'"]+)['"]/.exec(message);
  return Boolean(match && isBuiltinSpecifier(match[1]));
}

function canonicalBuiltinSpecifier(specifier: string): string | null {
  if (specifier.startsWith("node:")) {
    const bareSpecifier = specifier.slice("node:".length);
    return NODE_BUILTIN_MODULES.has(bareSpecifier) ? specifier : null;
  }
  return LEGACY_NODE_BUILTIN_MODULES.has(specifier) ? `node:${specifier}` : null;
}

function isCanonicalBuiltinSpecifier(specifier: string): boolean {
  return specifier.startsWith("node:") && canonicalBuiltinSpecifier(specifier) === specifier;
}

function resolutionKey(importer: string, specifier: string): string {
  return `${normalizeFile(importer)}::${specifier}`;
}

function toResolvedModule(fileName: string): ts.ResolvedModuleFull {
  const extension = fileName.endsWith(".d.ts")
    ? ts.Extension.Dts
    : fileName.endsWith(".tsx")
      ? ts.Extension.Tsx
      : fileName.endsWith(".ts")
        ? ts.Extension.Ts
        : fileName.endsWith(".jsx")
          ? ts.Extension.Jsx
          : ts.Extension.Js;
  return {
    resolvedFileName: fileName,
    extension,
    isExternalLibraryImport: isNodeModulesPath(fileName),
  };
}

function readFileIfAvailable(fileName: string): string | null {
  try {
    return ts.sys.fileExists(fileName) ? ts.sys.readFile(fileName) ?? null : null;
  } catch {
    return null;
  }
}

function leftoverRuntimeModuleSpecifiers(source: string): string[] {
  const leftover: string[] = [];
  const sourceFile = ts.createSourceFile("runtime.mjs", source, ts.ScriptTarget.Latest, true, ts.ScriptKind.JS);
  const visit = (node: ts.Node) => {
    if ((ts.isImportDeclaration(node) || ts.isExportDeclaration(node)) && node.moduleSpecifier && ts.isStringLiteral(node.moduleSpecifier)) {
      leftover.push(node.moduleSpecifier.text);
    } else if (ts.isImportEqualsDeclaration(node) && ts.isExternalModuleReference(node.moduleReference) && node.moduleReference.expression && ts.isStringLiteral(node.moduleReference.expression)) {
      leftover.push(node.moduleReference.expression.text);
    } else if (ts.isCallExpression(node) && node.expression.kind === ts.SyntaxKind.ImportKeyword) {
      const argument = node.arguments[0];
      leftover.push(argument && ts.isStringLiteralLike(argument) ? argument.text : "[unsupported dynamic import]");
    }
    ts.forEachChild(node, visit);
  };
  visit(sourceFile);
  return leftover;
}

function ensureSelfTypesDirective(source: string): string {
  const directive = '// @ts-self-types="./index.d.ts"';
  const withoutExisting = source.replace(/^\s*\/\/\s*@ts-self-types=.*(?:\r?\n|$)/, "");
  return `${directive}\n${withoutExisting.trimStart()}`;
}

function stripBundlerLocationComments(source: string): string {
  return source
    .replace(/^\s*\/\/\#region[^\r\n]*(?:\r?\n|$)/gm, "")
    .replace(/^\s*\/\/\#endregion[^\r\n]*(?:\r?\n|$)/gm, "");
}

function normalizeDeclarationSource(source: string): string {
  return source.replace(/\r\n/g, "\n").trim() + "\n";
}

function stripDeclarationSourceMap(source: string): string {
  return source.replace(/\s*\/\/#[#]?\s*sourceMappingURL=.*$/gm, "");
}

function hasMeaningfulDeclarationSurface(source: string): boolean {
  return /\bexport\s+(?:declare\s+)?(?:function|class|const|let|var|interface|type|enum|namespace|abstract\s+class)/.test(source) ||
    /\bexport\s+default\s+(?:function|class|const|let|var|interface|type)/.test(source) ||
    /\bexport\s+import\b/.test(source) ||
    /\bexport\s*\{/.test(source) || /\bexport\s*=/.test(source);
}

function createContentHash(runtimeSource: string, typesSource: string): string {
  return createHash("sha256")
    .update(ARTIFACT_HASH_VERSION)
    .update("\0")
    .update(runtimeSource)
    .update("\0")
    .update(typesSource)
    .digest("hex");
}

function formatDiagnostics(diagnostics: readonly ts.Diagnostic[]): string {
  return diagnostics
    .map((diagnostic) => ts.flattenDiagnosticMessageText(diagnostic.messageText, "\n"))
    .join("; ");
}

function formatError(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function registerOwner(
  owner: string,
  dependencies: Set<string>,
  ownersByDependency: Map<string, Set<string>>,
  dependenciesByOwner: Map<string, Set<string>>,
): void {
  const previous = dependenciesByOwner.get(owner);
  if (previous) {
    for (const dependency of previous) {
      const owners = ownersByDependency.get(dependency);
      owners?.delete(owner);
      if (owners?.size === 0) ownersByDependency.delete(dependency);
    }
  }
  dependenciesByOwner.set(owner, dependencies);
  for (const dependency of dependencies) {
    const normalizedDependency = normalizeFile(dependency);
    const owners = ownersByDependency.get(normalizedDependency) ?? new Set<string>();
    owners.add(owner);
    ownersByDependency.set(normalizedDependency, owners);
  }
}

function invalidateOwners(
  context: HmrContext,
  ownersByDependency: Map<string, Set<string>>,
): Array<import("vite").ModuleNode> | void {
  const changed = normalizeFile(context.file);
  const owners = ownersByDependency.get(changed);
  if (!owners || owners.size === 0) return;

  const modules: Array<import("vite").ModuleNode> = [];
  for (const owner of owners) {
    const module = context.server.moduleGraph.getModuleById(owner)
      ?? context.server.moduleGraph.getModuleById(normalizeFile(owner))
      ?? [...(context.server.moduleGraph.getModulesByFile(owner) ?? [])][0]
      ?? [...(context.server.moduleGraph.getModulesByFile(normalizeFile(owner)) ?? [])][0];
    if (!module) continue;
    context.server.moduleGraph.invalidateModule(module);
    modules.push(module);
  }
  return modules;
}
