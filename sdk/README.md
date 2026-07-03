# Pedelec SDK

[![npm version](https://img.shields.io/npm/v/@kaoruisaac/pedelec.svg)](https://www.npmjs.com/package/@kaoruisaac/pedelec)

Browser SDK for connecting a web app to the Pedelec Chrome Extension and Desktop Runtime.

The SDK lets a browser page create local agent sessions, send user text, receive streamed assistant output, handle tool calls in the web app, and resume or end sessions.

## Requirements

Pedelec SDK runs in a browser page context. It is not intended for Node.js, SSR server code, or background workers.

Before using it, the user needs:

- Pedelec Chrome Extension installed.
- Pedelec Desktop App running.
- Chrome Native Messaging host registered by the Desktop App.
- The target provider available locally. CLI-backed providers use commands such as `codex`, `gemini`, `opencode`, `cursor`, or `claude`; the Ollama provider uses Pedelec's bundled `pedelec-agent`.

## Installation

Install from npm:

```bash
npm install @kaoruisaac/pedelec
```

Import the SDK in browser-side TypeScript:

```ts
import { Pedelec } from "@kaoruisaac/pedelec";
```

For local development before publishing:

```bash
npm install ../path/to/pedelec/sdk
```

To build the SDK from this repository:

```bash
cd sdk
npm install
npm run build
```

## Quick Start

```ts
import { Pedelec } from "@kaoruisaac/pedelec";

const pedelec = new Pedelec();

const session = await pedelec.createSession({
  provider: "codex",
  model: "gpt-5",
  skillsUrls: ["https://example.com/tools.md"],
});

session.onChat((text) => {
  console.log(text);
});

session.onStatus((status) => {
  console.log("status", status);
});

session.onError((error) => {
  console.error(error.code, error.message, error.details);
});

await session.sendText("Analyze the current page state.");
```

`sendText()` resolves after the current agent turn is done. If the same session is already running a prompt, another `sendText()` rejects with `SESSION_BUSY`.

## Create a Client

```ts
const pedelec = new Pedelec({
  bridgeTimeoutMs: 30_000,
});
```

`bridgeTimeoutMs` controls how long SDK requests wait for an extension response. The default is 30 seconds. Values below 1 ms are clamped to 1 ms.

## Check Approval Status

The extension may ask the user to approve the current origin before a page can create or resume sessions. You can check status before showing a connect button.

```ts
const status = await pedelec.getApprovalStatus();

if (!status.installed) {
  console.log("Pedelec extension is not available.");
} else if (!status.approved) {
  console.log(`${status.origin} still needs approval.`);
}
```

Return shape:

```ts
type ApprovalStatus = {
  installed: boolean;
  approved: boolean;
  origin: string | null;
};
```

If the extension cannot be reached, `getApprovalStatus()` returns `{ installed: false, approved: false, origin }` instead of throwing for extension unavailable cases.

## Providers

List available providers on the user's machine:

```ts
const providers = await pedelec.listProviders();

for (const provider of providers) {
  console.log(provider.code, provider.available, provider.path, provider.error);
}
```

```ts
type ProviderCode = "codex" | "gemini" | "opencode" | "cursor" | "claude" | "ollama";

type ProviderInfo = {
  name: string;
  code: ProviderCode;
  path: string | null;
  available: boolean;
  error: string | null;
};
```

Supported provider codes:

| Provider | Code | Example model |
| --- | --- | --- |
| Codex | `codex` | `gpt-5` |
| Gemini | `gemini` | provider-supported model id |
| OpenCode | `opencode` | `ollama/qwen2.5-coder:14b` |
| Cursor | `cursor` | `gpt-5` |
| Claude Code | `claude` | `sonnet` |
| Ollama | `ollama` | `qwen3-14b-32k:latest` |

`available: false` usually means the provider CLI is not installed or is not available in `PATH`.
For Ollama, `available: true` only means Pedelec can find `pedelec-agent`; it does not mean the Ollama server is running or the selected model is installed.

## Settings

Read the Desktop App default provider and per-provider models:

```ts
const settings = await pedelec.getSettings();

console.log(settings.defaultProvider);
console.log(settings.defaultModels.codex);
console.log(settings.defaultModels.gemini);
console.log(settings.defaultModels.ollama);
```

```ts
type PedelecSettings = {
  defaultProvider: ProviderCode | null;
  defaultModels: Partial<Record<ProviderCode, string>>;
};
```

## Create Sessions

Use the Desktop App default provider:

```ts
const session = await pedelec.createSession();
```

Use default provider with skills:

```ts
const session = await pedelec.createSession({
  skillsUrls: ["https://example.com/tools.md", "https://example.com/tools.json"],
});
```

If no default provider is configured, this rejects with `DEFAULT_PROVIDER_NOT_SET`. If the default provider is configured but unavailable, it rejects with `DEFAULT_PROVIDER_UNAVAILABLE`. If the default provider has its own default model, the SDK sends that model with the session request.

Use an explicit provider:

```ts
const session = await pedelec.createSession({
  provider: "codex",
});
```

When only `provider` is passed, the SDK uses that provider's own Desktop App default model if one is configured. Otherwise, it sends the provider without a model and lets the provider CLI use its own default behavior.
Ollama is the exception: it requires a model, so provider-only Ollama sessions need `defaultModels.ollama` or they fail with `MODEL_REQUIRED`.

Use an explicit provider and model:

```ts
const session = await pedelec.createSession({
  provider: "ollama",
  model: "qwen3-14b-32k:latest",
});
```

Ollama sessions are executed by the `pedelec-agent` binary bundled with the Desktop App, not by the `ollama` CLI. You still need to start the local Ollama server yourself. Ollama requires a model from the session input or `defaultModels.ollama`; otherwise the CoreRuntime returns `MODEL_REQUIRED`.

By default, SDK-created sessions are page-scoped. `autoEndOnDisconnect` defaults to `true`, so the Desktop thread is ended when the last SDK connection for that session disconnects, such as on page refresh or tab close. Keep the default for demo and page-scoped apps. Set `autoEndOnDisconnect: false` when you need to resume the same session after navigation or share it across pages:

```ts
const session = await pedelec.createSession({
  provider: "codex",
  autoEndOnDisconnect: false,
});
```

`model` cannot be provided without `provider`.

```ts
type CreateSessionInput =
  | {
      provider: ProviderCode;
      model?: string;
      skillsUrls?: string[];
      autoEndOnDisconnect?: boolean;
    }
  | {
      provider?: undefined;
      skillsUrls?: string[];
      autoEndOnDisconnect?: boolean;
    };
```

## Stream Responses

`onChat()` receives text deltas. A delta is not guaranteed to be a sentence or paragraph, so UI code should append chunks.

```ts
let assistantText = "";

session.onChat((text) => {
  assistantText += text;
  renderAssistantMessage(assistantText);
});
```

The returned function unsubscribes the handler:

```ts
const unsubscribe = session.onChat((text) => {
  console.log(text);
});

unsubscribe();
```

## Send Text

```ts
try {
  await session.sendText("Summarize this workspace.");
} catch (error) {
  console.error("send failed", error);
}
```

`sendText(text)` sets the session status to `running`, sends the prompt to the runtime, and resolves after a `done` or idle status event. It rejects if the session ends, errors, disconnects, or is already busy.

## Session Status

```ts
session.onStatus((status) => {
  switch (status) {
    case "idle":
      break;
    case "running":
      break;
    case "waiting_tool_result":
      break;
    case "ended":
      break;
    case "error":
      break;
  }
});

console.log(session.getStatus());
```

```ts
type PedelecSessionStatus =
  | "idle"
  | "running"
  | "waiting_tool_result"
  | "ended"
  | "error";
```

| Status | Meaning |
| --- | --- |
| `idle` | The session can accept another prompt. |
| `running` | The agent is processing user text. |
| `waiting_tool_result` | The agent requested a tool result from the web app. |
| `ended` | The session has ended and cannot receive more text. |
| `error` | The session entered an error state. |

## Tool Calling

Use `skillsUrls` to tell the agent what tools your web app can handle. When the agent calls a tool, the SDK invokes the registered `onTool()` handler and automatically submits the returned result to the runtime.

```ts
session.onTool(async (tool, args) => {
  if (tool === "get_current_page") {
    return {
      url: location.href,
      title: document.title,
      selectedText: window.getSelection()?.toString() ?? "",
    };
  }

  if (tool === "update_counter") {
    const { delta } = args as { delta: number };
    counter.value += delta;

    return {
      counter: counter.value,
      delta,
    };
  }

  return {
    error: {
      code: "TOOL_NOT_FOUND",
      message: `Unknown tool: ${tool}`,
    },
  };
});
```

Tool results must be JSON-serializable. If no handler is registered, the SDK returns a `TOOL_HANDLER_NOT_FOUND` error result. If the handler throws, the SDK returns a `TOOL_HANDLER_ERROR` error result.

Only one tool handler is active per session. Calling `onTool()` again replaces the previous handler.

## Resume Sessions

If you have saved a `sessionId`, resume it later:

```ts
const session = await pedelec.resumeSession("thread_abc123");

session.onChat((text) => {
  console.log(text);
});

await session.sendText("Continue the previous task.");
```

`resumeSession(sessionId)` rejects with `INVALID_INPUT` when `sessionId` is empty.

## End Sessions

```ts
await session.end();
```

After a session ends, future `sendText()` calls reject with `SESSION_ENDED`.

Listen for runtime-ended sessions:

```ts
session.onEnded(() => {
  console.log("session ended");
});
```

## Low-Level Requests

Most apps should use the typed methods above. `request<T>(type, payload)` is exposed for advanced integrations that need to call bridge operations directly.

```ts
const result = await pedelec.request<{ sessionId: string }>("create_session", {
  input: {
    provider: "codex",
    skillsUrls: [],
    autoEndOnDisconnect: true,
  },
});
```

Low-level requests still use the SDK bridge timeout and reject with `PedelecError` objects.

## Error Handling

Register `onError()` for session-level errors and use `try/catch` around async SDK calls.

```ts
session.onError((error) => {
  console.error("session error", error.code, error.message, error.details);
});

try {
  await session.sendText("Update the selected content.");
} catch (error) {
  console.error("request failed", error);
}
```

```ts
type PedelecError = {
  code: string;
  message: string;
  details?: unknown;
};
```

Common error codes:

| Code | Meaning |
| --- | --- |
| `EXTENSION_UNAVAILABLE` | The SDK is not running in a supported browser page, or the extension cannot be reached. |
| `EXTENSION_DISCONNECTED` | The extension connection was interrupted. |
| `SDK_BRIDGE_TIMEOUT` | The extension did not respond before `bridgeTimeoutMs`. |
| `SDK_PROTOCOL_ERROR` | The extension response did not match the SDK protocol. |
| `SDK_TRANSPORT_ERROR` | A bridge request failed at the transport layer. |
| `APPROVAL_REJECTED` | The user rejected approval for the current origin. |
| `APPROVAL_TIMEOUT` | The user did not complete origin approval in time. |
| `OPEN_POPUP_FAILED` | The extension could not open the approval popup. |
| `NATIVE_HOST_UNAVAILABLE` | Chrome Native Messaging host cannot be reached. |
| `DEFAULT_PROVIDER_NOT_SET` | The Desktop App has no default provider configured. |
| `DEFAULT_PROVIDER_UNAVAILABLE` | The configured default provider is not currently available. |
| `INVALID_INPUT` | The request input is invalid, such as model without provider or empty session id. |
| `SESSION_BUSY` | The session already has a prompt in progress. |
| `SESSION_ENDED` | The session has ended. |
| `SESSION_ERROR` | The runtime reported a session error. |
| `TOOL_HANDLER_NOT_FOUND` | The agent called a tool but no web app handler is registered. |
| `TOOL_HANDLER_ERROR` | The web app tool handler threw an error. |
| `SUBMIT_TOOL_RESULT_FAILED` | The SDK could not submit a tool result back to the runtime. |

## Public API Summary

```ts
class Pedelec {
  constructor(options?: { bridgeTimeoutMs?: number });
  createSession(input?: CreateSessionInput): Promise<PedelecSession>;
  listProviders(): Promise<ProviderInfo[]>;
  getSettings(): Promise<PedelecSettings>;
  getApprovalStatus(): Promise<ApprovalStatus>;
  resumeSession(sessionId: string): Promise<PedelecSession>;
  request<T>(type: string, payload?: Record<string, unknown>): Promise<T>;
}

class PedelecSession {
  readonly sessionId: string;
  readonly provider: string;
  readonly model?: string;

  sendText(text: string): Promise<void>;
  onChat(handler: (text: string) => void): () => void;
  onTool(handler: (tool: string, args: unknown) => unknown | Promise<unknown>): () => void;
  onError(handler: (error: PedelecError) => void): () => void;
  onStatus(handler: (status: PedelecSessionStatus) => void): () => void;
  onEnded(handler: () => void): () => void;
  getStatus(): PedelecSessionStatus;
  end(): Promise<void>;
}
```
