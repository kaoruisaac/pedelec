# Pedelec SDK

[![npm version](https://img.shields.io/npm/v/@kaoruisaac/pedelec.svg)](https://www.npmjs.com/package/@kaoruisaac/pedelec)

## Documentation

- [Pedelec Official](https://pedelec.cc)
- [Pedelec documentation](https://kaoruisaac.github.io/pedelec/)
- [GitHub repository](https://github.com/kaoruisaac/pedelec)

## Agent Skill

If you use an AI coding agent that supports skills, run the following command from the project you want to integrate with Pedelec:

```bash
npx skills add kaoruisaac/pedelec
```

This installs the `pedelec-integration` skill, which guides the agent through SDK setup, session lifecycle, frontend tools, Provider and effort selection, connection/recovery UX, and integration verification. For exact SDK APIs and behavior, continue to use the installed SDK declarations and the official documentation as the source of truth.

---
Pedelec is a browser SDK and local bridge for applications that want to work with AI coding agents such as Codex, Antigravity, OpenCode, Cursor, Claude Code, or an Ollama-backed agent.

A web application can use Pedelec to:

- create an agent session on the user's machine;
- open an application-managed workspace with the native directory picker or an explicit path;
- send user instructions and receive streamed assistant text;
- expose narrowly scoped browser-side tools to the agent;
- resume or end sessions; and
- upload and list completed workspace assets; and
- show connection, approval, provider, and lifecycle state in the UI.

## Why Pedelec exists

A normal chat API accepts text and returns text. An agent integration often needs more:

- The agent may need to inspect the page, editor, canvas, selection, or application state.
- The application may need to ask the user for confirmation while an agent turn is paused.
- The user may want to use a provider CLI that is already installed and authenticated locally.
- The browser should not receive permission to launch arbitrary local processes directly.

Pedelec separates those responsibilities. Your web application owns the UI and the tool handlers. The Pedelec extension and desktop runtime own the local transport, session lifecycle, and provider process.

```text
Web application
  ↓ @kaoruisaac/pedelec
Pedelec Chrome Extension
  ↓ Chrome Native Messaging
Pedelec native host
  ↓ local Core IPC
Pedelec Desktop Runtime
  ↓ provider process
Codex / Antigravity / OpenCode / Cursor / Claude Code / Ollama
```
## SDK Prerequisites

The Pedelec SDK must run in a browser page environment and requires:

1. The user has installed the Pedelec Chrome Extension.
2. The user has started the Pedelec Desktop App.
3. The Desktop App has registered the Chrome Native Messaging host.
4. The target provider is available on the user's machine. CLI-backed providers use commands such as `codex`, `agy`, `opencode`, `cursor-agent`, or `claude`; the Ollama provider uses Pedelec's bundled `pedelec-agent`.

The SDK is not suitable for direct use in Node.js, an SSR server, or a background worker; it needs extension runtime messaging from a Chrome page environment.

---

## Installing and Importing the SDK

The SDK package is located in `sdk/`:

```bash
cd sdk
npm install
npm run build
```

Import it from the Web App:

```ts
import { Pedelec, defineTool } from "@kaoruisaac/pedelec";
```

If it has not been published to npm yet, install it from a local path first:

```bash
npm install ../path/to/pedelec/sdk
```

---

## Deno Modules

Vite applications can package local or installed JavaScript/TypeScript libraries for Agent-authored `pedelec-deno` scripts.

### Setup

```ts
// vite.config.ts
import { defineConfig } from "vite";
import { pedelecVitePlugin } from "@kaoruisaac/pedelec/vite";

export default defineConfig({
  plugins: [pedelecVitePlugin()],
});
```

### Authoring

```ts
import { Pedelec, defineDenoModule } from "@kaoruisaac/pedelec";

const pedelec = new Pedelec();
const session = await pedelec.createSession({
  skills: {
    guidance: "Use sprite-tools for sprite authoring tasks.",
    tools: [],
    denoModules: [
      defineDenoModule({
        name: "sprite-tools",
        description: "Sprite authoring and preview utilities.",
        entry: "./agent/sprite-tools.ts",
        usage: `import { previewActorSource } from "sprite-tools";`,
        preferStdinExecution: true,
      }),
    ],
  },
});
```

For an installed package, use its package specifier as the build-time entry:

```ts
defineDenoModule({
  name: "gsap",
  description: "Animation utilities.",
  entry: "gsap",
  usage: `import { gsap } from "gsap";`,
});
```

`entry` is resolved by Vite during build/dev and is never a Desktop filesystem instruction. The plugin bundles normal third-party runtime dependencies locally and generates the public `index.d.ts` declarations automatically. Node built-ins may be imported with either canonical `node:` specifiers or legacy bare specifiers; the prepared artifact preserves them as canonical `node:` imports for Deno's Node compatibility layer. This does not change the sandbox: runtime npm, JSR, network fetching, and filesystem access outside the permitted workspace remain unavailable.

The generated declaration describes the module's Agent-facing API, so implementation-only dependency types are omitted. Module-owned types are supported; packages that expose dependency-owned or Node-specific external types directly in their public API are not guaranteed to produce a standalone declaration in V1. `name` is the Agent import specifier, and `usage` is required common-case guidance. `createSession()` transfers the prepared artifact automatically; a session keeps an immutable module snapshot, and `session.end()`/`session.resume()` reuse it while Core still knows the thread.

Deno Modules are imported inside JavaScript/TypeScript run with `pedelec-deno`. They are different from `skills.tools`, whose browser/App RPC capabilities use `pedelec-cli` tool-spec and tool-call commands.

Set the optional `preferStdinExecution: true` when short scripts primarily use a module and should be steered toward stdin execution. Host Context then adds that module's concrete `runCommandTemplate` (`@'\n<typescript-source>\n'@ | pedelec-deno --thread-id <thread-id> run -`), so the Agent only needs to replace the TypeScript source placeholder instead of deciding how to serialize stdin. This is a preference, not a restriction: file-backed execution remains valid, and omitting the field or setting it to `false` preserves the default behavior. It does not change bundling, artifact contents, or runtime permissions.

---

## Minimal Example

```ts
import { Pedelec, defineTool } from "@kaoruisaac/pedelec";

const pedelec = new Pedelec();

const session = await pedelec.createSession({
  provider: "codex",
  effortLevel: "high",
  skills: {
    guidance: "Use get_current_page when you need browser page context.",
    tools: [
      defineTool({
        name: "get_current_page",
        description: "Read the current browser page title and URL.",
        argsSchema: {
          type: "object",
          properties: {},
          required: [],
        },
        handler: () => ({
          url: location.href,
          title: document.title,
        }),
      }),
    ],
  },
});

session.onChat((text) => {
  // One completed logical assistant message.
  console.log(text);
});

session.onChatDelta((delta) => {
  // Optional best-effort incremental text for live UI.
  console.log(delta);
});

session.onStatus((status) => {
  // idle | running | waiting_tool_result | ended | error
  console.log("status", status);
});

session.onError((error) => {
  console.error(error.code, error.message, error.details);
});

await session.sendText("Please help me analyze the current page state");
```

`sendText()` resolves after Core reports the matching semantic operation as completed. An `idle` status or bridge request response alone does not complete it. If the session is already handling a previous prompt, the new `sendText()` call is rejected to prevent multiple concurrent requests from running in the same session.

## End and resume a session

```ts
await session.end();
await session.resume();

console.log(session.getStatus()); // "idle"
await session.sendText("Continue");
```

`session.resume()` reactivates the same non-detached handle and Core thread when Core still knows the thread and its recorded workspace still exists. It preserves handlers and session state, does not contact the provider runtime, and leaves provider startup/resume lazy until the next operation. A missing workspace returns `WORKSPACE_OPEN_FAILED`; a thread lost after a Desktop/Core restart returns `THREAD_NOT_FOUND`. `pedelec.resumeSession(sessionId)` is a separate reattachment API: it returns a new handle and does not implicitly revive an ended thread. A transport-detached old handle must use that API.

## Session assets

```ts
const path = await session.uploadAsset(file);
const assets = await session.listAssets();
```

Assets are stored in the Workspace shared by the Session. The physical shared App/Agent directory is `.pedelec-runtime/assets/`. In the SDK contract, `assets/` is its implicit root: `listAssets()` recursively lists regular files at every level as a flat array, ordered by filesystem modification time (newest first) and then by name; nested paths such as `/results/report.json` are returned in full. Directory entries and symlinks are excluded, as are `.pedelec-*` entries at every level; other dotfiles are included. It may run while the agent runs. One session can only upload one file at a time, but uploads can run alongside prepare or agent execution. When several sessions share a Workspace, they share this `.pedelec-runtime/assets/` area.

```ts
const text = await session.readAsset("/report.txt", "text");
const json = await session.readAsset<{ ok: boolean }>("/result.json", "json");
const result = await session.readAsset("/model.glb", "file");
```

Public asset paths use `/...` with `assets/` as their implicit root; nested paths such as `/results/model.glb` are supported up to 100 MiB.

## Workspace

Workspace is the filesystem root in which the Agent works. Pedelec stores its private runtime data under `<workspace>/.pedelec-runtime/`. There are two supported flows:

### Explicit Workspace

```ts
const workspace = await pedelec.openWorkspace();
if (!workspace) return;

const session = await workspace.createSession({
  model: "gpt-5.6-sol",
  effort: "high",
});
```

`openWorkspace()` without a path opens the native folder picker. User cancellation resolves to `null`; no Workspace or Session is created. The returned `PedelecWorkspace` handle is the object to retain and reuse. An explicit path bypasses the picker:

```ts
const workspace = await pedelec.openWorkspace("C:\\workspace\\project-a");
```

Pedelec initializes `.pedelec-runtime` for Workspace-owned runtime data without clearing project files or existing private data. Assets, logs, and temporary data are Workspace-level, while generated Session skills and Deno Module state are isolated per Thread. Applications should not depend on the private `.pedelec-runtime` subdirectory layout. Explicit Workspaces are application-managed, are never deleted by Pedelec, and can own multiple Sessions. An explicit path must not overlap Pedelec's managed workspace root.

### Managed convenience

```ts
const session = await pedelec.createSession({
  effortLevel: "high",
});
console.log(session.workspace.path); // null for managed Workspace
```

`Pedelec.createSession()` creates a temporary Desktop-managed Workspace automatically. It no longer accepts Workspace configuration; use `openWorkspace()` followed by `workspace.createSession()` for an explicit Workspace. Every created or resumed Session exposes its Workspace as `session.workspace`.

### Workspace methods

`workspace.listFiles(path?)` and `workspace.listFolders(path?)` recursively return Workspace-relative paths. An omitted path lists from the Workspace root; `/` is the separator and results are lexicographically sorted. The `.pedelec-runtime` directory is included. Symlinks and junctions are neither followed nor returned. Results are not silently truncated: an oversized response fails with `WORKSPACE_LIST_TOO_LARGE`, so retry with a narrower path.

`workspace.run(script, options?)` executes application-supplied Deno code in the Workspace:

```ts
const result = await workspace.run(
  "console.log(JSON.stringify({ ok: true }))",
  { timeoutMs: 60_000 },
);

if (result.stdoutTruncated) {
  throw new Error("Workspace script output was truncated");
}
const value = JSON.parse(result.stdout);
```

The default timeout is 60 seconds; `timeoutMs` must be a positive integer. The return value is the raw `DenoRunOutput` shape: `exitCode`, `stdout`, `stderr`, `stdoutTruncated`, and `stderrTruncated`. A non-zero `exitCode` still resolves normally. The application parses `stdout` when it wants a function-like return value. Check truncation flags before relying on output; an oversized browser response fails with `WORKSPACE_RUN_OUTPUT_TOO_LARGE`.

`workspace.run()` has read and write access to the Workspace, but no network, host environment, subprocess execution, FFI, or `sys` permission. The existing no-remote, cached-only, and no-npm runtime restrictions still apply. This is a direct deterministic capability for an approved Web application to read and modify the opened Workspace; it is not unrestricted host code execution and is not mediated by an Agent.

Several `workspace.run()` calls in one Workspace may execute concurrently. A Workspace run cannot start while any Session in that Workspace has an active Agent/provider operation. Conversely, a Session in that Workspace cannot begin `sendText()` or provider preparation while one or more Workspace runs are active. This rule is Workspace-wide, including operations started through another `PedelecSession` handle; conflicts reject with `WORKSPACE_BUSY`.

Session `skills.denoModules` belong to the Session/Thread that declares them and are available only to Agent-side `pedelec-deno` execution for that Thread. `workspace.run()` does not inherit any Session Deno Modules and the Workspace has no Deno Module registration API. `workspace.run()` is intentionally Workspace-only: it does not choose a Session and does not receive a Session import map.

The Workspace handle outlives an individual Session conceptually. Ending a Session does not close or delete a custom Workspace, and Pedelec does not delete it during app cleanup. Managed Workspaces remain under Desktop cleanup ownership and are not promised to disappear immediately when a Thread ends. The browser capability is runtime-scoped; reopen the custom Workspace after a Desktop/Core restart when necessary. `pedelec.resumeSession(sessionId)` returns a Session that already includes `session.workspace`; the application does not need to call `openWorkspace()` separately. Same-handle `session.resume()` preserves the existing Workspace handle, subject to the existing Core/thread and transport caveats.

---

## Creating a Session

The first time an origin calls `createSession()` or `resumeSession()`, the extension asks the user to approve that origin in the popup. After approval, the same origin can create sessions directly.

Sessions created through the SDK are bound to the creating origin. The same origin may resume them after reload, in another tab, or from another SDK instance; a different scheme, host, subdomain, or port cannot use the ID. A `sessionId` is an identifier, not a cross-origin bearer credential, and applications cannot supply or override its owner origin.

You can query the current origin's approval status first to decide whether to show UI such as "Connect Pedelec":

```ts
const status = await pedelec.getApprovalStatus();

console.log(status.installed, status.approved, status.origin);
```

`getApprovalStatus()` includes `appConnected`, a non-sensitive Core `ping` result. It does not require approval or open the approval popup, but it may use the Native Host's existing Desktop auto-launch fallback. `appConnected` does not mean the origin is approved or a provider is ready.

`getSettings()` and `listProviders()` require origin approval and may open the approval popup. Settings expose only defaults (never provider credentials); providers expose only `name`, `code`, `available`, `isDefault`, and `error`. `isDefault` identifies the provider selected by the current Desktop settings and is independent of `available`; it remains true when that provider is unavailable. Use `getApprovalStatus().appConnected` rather than `listProviders()` as a connection probe.

```ts
type ProviderInfo = {
  name: string;
  code: ProviderCode;
  available: boolean;
  isDefault: boolean;
  error: string | null;
};
```

For a complete Extension, approval, and Desktop readiness probe, use `checkAvailability()`. It never creates/resumes a session or opens approval. Its status ping may attempt Desktop launch even for an unapproved origin.

```ts
const availability = await pedelec.checkAvailability();
if (availability.available) startUi();
```

An unavailable extension may be disconnected rather than absent. `desktop.launchAttempted` means the settings probe was sent, not that Desktop was confirmed to have launched. Invalid settings responses also count as Desktop unavailable in this probe.

### Selecting Provider, Model, and Effort

```ts
const session = await pedelec.createSession({
  provider: "codex",
  effortLevel: "high",
});

// Or let Desktop Settings choose the provider.
const defaultSession = await pedelec.createSession({ effortLevel: "low" });
```

The SDK supports two mutually exclusive session configuration modes. In Desktop profile mode, `effortLevel` selects the complete Desktop profile, including its configured model and native effort arguments. An omitted level means `default`:

```ts
const profileSession = await pedelec.createSession({
  provider: "codex",
  effortLevel: "high",
});
```

In explicit model mode, `model` selects the provider-native model directly and does not read or inherit any Desktop profile. The optional `effort` is provider-native and uses `low`, `medium`, `high`, `xhigh`, or `max`:

```ts
const modelSession = await pedelec.createSession({
  provider: "codex",
  model: "gpt-x",
  effort: "max",
});
```

Model-only requests are valid and use the provider's default effort behavior. `effort` requires `model`; `model` cannot be combined with `effortLevel`. The SDK does not expose or maintain a model catalog, and it does not translate the identifier into provider CLI flags. Model validity, effort support, and account entitlement remain provider-dependent, so an invalid or unavailable configuration may fail when the provider starts.

Explicit model requests require an `explicitModelConfigApplied` acknowledgment from Core before the SDK exposes the session or starts Deno Module setup. If the connected Extension/Desktop is too old to provide that acknowledgment, the SDK aborts the newly created setup and reports a compatibility error instead of silently using a different configuration.

Currently supported provider codes in the SDK:

| Provider | Code |
| --- | --- |
| Codex | `codex` |
| Antigravity | `antigravity` |
| OpenCode | `opencode` |
| Cursor | `cursor` |
| Claude Code | `claude` |
| Ollama | `ollama` |

Ollama sessions are executed by the bundled `pedelec-agent`. In profile mode, the selected Ollama effort profile must contain a model; otherwise Core returns `MODEL_REQUIRED`. In explicit model mode, `{ model }` supplies the model directly. Ollama currently does not accept an explicit `effort`. Low and high profiles are optional, but there is no fallback between profiles.

`getSettings()` returns only `{ defaultProvider }`. It never exposes provider settings, effort arguments, credentials, or model names.
