# Pedelec SDK

[![npm version](https://img.shields.io/npm/v/@kaoruisaac/pedelec.svg)](https://www.npmjs.com/package/@kaoruisaac/pedelec)

## Documentation

- [Pedelec documentation](https://kaoruisaac.github.io/pedelec/)
- [GitHub repository](https://github.com/kaoruisaac/pedelec)


Pedelec is a browser SDK and local bridge for applications that want to work with AI coding agents such as Codex, Gemini, OpenCode, Cursor, Claude Code, or an Ollama-backed agent.

A web application can use Pedelec to:

- create an agent session on the user's machine;
- send user instructions and receive streamed assistant text;
- expose narrowly scoped browser-side tools to the agent;
- resume or end sessions; and
- upload and list completed sandbox assets; and
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
Codex / Gemini / OpenCode / Cursor / Claude Code / Ollama
```
## SDK Prerequisites

The Pedelec SDK must run in a browser page environment and requires:

1. The user has installed the Pedelec Chrome Extension.
2. The user has started the Pedelec Desktop App.
3. The Desktop App has registered the Chrome Native Messaging host.
4. The target provider is available on the user's machine. CLI-backed providers use commands such as `codex`, `gemini`, `opencode`, `cursor`, or `claude`; the Ollama provider uses Pedelec's bundled `pedelec-agent`.

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

## Minimal Example

```ts
import { Pedelec, defineTool } from "@kaoruisaac/pedelec";

const pedelec = new Pedelec();

const session = await pedelec.createSession({
  provider: "codex",
  model: "gpt-5",
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
  // Incremental text stream from the agent.
  console.log(text);
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

`sendText()` resolves after the current agent response completes. If the session is already handling a previous prompt, the new `sendText()` call is rejected to prevent multiple concurrent requests from running in the same session.

## Listing uploaded assets

```ts
const path = await session.uploadAsset(file);
const assets = await session.listAssets();
```

`listAssets()` lists only the first level of `input/`, ordered by filesystem modification time (newest first). Each item's `name` is the actual sandbox filename and may differ from the original `File.name`; `modifiedAt` is a filesystem modification timestamp, not an exact upload time. `path` is an agent-sandbox-relative `input/...` path and never an absolute local path. It may be called while the agent is running. Ended sessions reject with `SESSION_ENDED`. Recursive listing, pagination, download, deletion, rename, and move are not supported.

---

## Creating a Session

The first time an origin calls `createSession()` or `resumeSession()`, the extension asks the user to approve that origin in the popup. After approval, the same origin can create sessions directly.

You can query the current origin's approval status first to decide whether to show UI such as "Connect Pedelec":

```ts
const status = await pedelec.getApprovalStatus();

console.log(status.installed, status.approved, status.origin);
```

For a complete Extension, approval, and Desktop readiness probe, use `checkAvailability()`. It never creates/resumes a session or opens approval. When approved it validates Desktop by calling `getSettings()`, which may invoke the existing Native Host auto-launch fallback.

```ts
const availability = await pedelec.checkAvailability();
if (availability.available) startUi();
```

An unavailable extension may be disconnected rather than absent. `desktop.launchAttempted` means the settings probe was sent, not that Desktop was confirmed to have launched. Invalid settings responses also count as Desktop unavailable in this probe.

### Specifying Provider and Model

```ts
const session = await pedelec.createSession({
  provider: "opencode",
  model: "ollama/qwen2.5-coder:14b",
});
```

Currently supported provider codes in the SDK:

| Provider | Code | Example model |
| --- | --- | --- |
| Codex | `codex` | `gpt-5` |
| Gemini | `gemini` | Any model ID supported by the provider |
| OpenCode | `opencode` | `ollama/qwen2.5-coder:14b` |
| Cursor | `cursor` | `gpt-5` |
| Claude Code | `claude` | `sonnet` |
| Ollama | `ollama` | `qwen3-14b-32k:latest` |

Ollama sessions are executed by the `pedelec-agent` binary bundled with the Desktop App, not by the `ollama` CLI. You still need to start the local Ollama server yourself and specify a model explicitly or configure `defaultModels.ollama` in Settings:

```ts
const session = await pedelec.createSession({
  provider: "ollama",
  model: "qwen3-14b-32k:latest",
});
```
