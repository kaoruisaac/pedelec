# Pedelec SDK

[![npm version](https://img.shields.io/npm/v/@kaoruisaac/pedelec.svg)](https://www.npmjs.com/package/@kaoruisaac/pedelec)

Browser SDK for connecting a web app to local AI agents through the Pedelec Chrome Extension and Desktop App.

Use it to:

- create and resume agent sessions;
- send prompts and receive streamed responses;
- expose browser-side tools that can read or update your app state.

> Pedelec SDK runs in a browser page. It is not intended for Node.js, SSR server code, or background workers.

## Requirements

The user must have:

1. the Pedelec Chrome Extension installed;
2. the Pedelec Desktop App running;
3. the required provider available locally.

Supported provider codes are `codex`, `gemini`, `opencode`, `cursor`, `claude`, and `ollama`.

## Install

```bash
npm install @kaoruisaac/pedelec
```

```ts
import { Pedelec, defineTool } from "@kaoruisaac/pedelec";
```

## Quick start

```ts
import {
  Pedelec,
  defineTool,
  type ToolArgsSchema,
} from "@kaoruisaac/pedelec";

const noArgs = {
  type: "object",
  properties: {},
  required: [],
} satisfies ToolArgsSchema;

const tools = [
  defineTool({
    name: "get_current_page",
    description: "Read the current browser page title and URL.",
    argsSchema: noArgs,
    handler: () => ({
      title: document.title,
      url: location.href,
    }),
  }),
] as const;

const pedelec = new Pedelec();

const session = await pedelec.createSession({
  provider: "codex",
  skills: {
    guidance: "Use get_current_page when page identity is required.",
    tools,
  },
});

let assistantText = "";

session.onChat((delta) => {
  assistantText += delta;
  console.log(assistantText);
});

session.onStatus((status) => {
  console.log("status", status);
});

session.onError((error) => {
  console.error(error.code, error.message, error.details);
});

await session.sendText("Describe the current page.");
```

`onChat()` receives incremental text chunks. Append them instead of treating each chunk as a complete message.

`sendText()` resolves when the current agent turn finishes. A session processes only one turn at a time; another call made while it is busy rejects with `SESSION_BUSY`.

## Creating a session

Use the provider and model configured in the Desktop App:

```ts
const session = await pedelec.createSession();
```

Choose a provider explicitly:

```ts
const session = await pedelec.createSession({
  provider: "gemini",
});
```

Choose both provider and model:

```ts
const session = await pedelec.createSession({
  provider: "codex",
  model: "provider-supported-model-id",
});
```

Model names are provider-specific strings. When only `provider` is provided, the SDK uses that provider's default model from the Desktop App when one is configured.

Register listeners immediately after creating the session and before the first `sendText()` call so early events are not missed.

### Optional warm-up

```ts
await session.prepare();
```

`prepare()` starts provider setup before the first real prompt. It is optional; `sendText()` works without it. If preparation is already running, `sendText()` waits for it and falls back to the normal first-run path if preparation fails.

## Writing tools

A tool has two parts:

1. a serializable definition sent to the agent;
2. a browser-side handler that performs the action.

```ts
const counter = { value: 0 };

type UpdateCounterArgs = {
  delta: number;
};

type UpdateCounterResult = {
  value: number;
};

const updateCounter = defineTool<
  UpdateCounterArgs,
  UpdateCounterResult
>({
  name: "update_counter",
  description: "Increase or decrease the visible counter by a signed delta.",
  argsSchema: {
    type: "object",
    properties: {
      delta: {
        type: "integer",
        description: "Signed amount to add to the counter.",
        minimum: -100,
        maximum: 100,
      },
    },
    required: ["delta"],
  },
  timeoutMs: 10_000,
  handler: (args) => {
    counter.value += args.delta;
    return { value: counter.value };
  },
});
```

### Tool fields

| Field | Purpose |
| --- | --- |
| `name` | Stable agent-facing identifier. Lowercase snake case is recommended. |
| `description` | Explains what the tool does, what state it reads or changes, and important constraints. |
| `argsSchema` | Describes the arguments the agent must provide. The root must always be an object. |
| `timeoutMs` | Optional positive integer. Core currently defaults to 60 seconds. |
| `handler` | Optional sync or async browser function that returns the tool result. |

Tool names must match `^[a-zA-Z][a-zA-Z0-9_.-]*$` and must be unique within one `skills.tools` array.

`argsSchema` supports the Pedelec Tool Args Schema subset: `string`, `number`, `integer`, `boolean`, `array`, `object`, and `oneOf`. It is not full JSON Schema.

For a no-argument tool, still use an object schema:

```ts
const noArgs = {
  type: "object",
  properties: {},
  required: [],
} satisfies ToolArgsSchema;
```

TypeScript annotations improve authoring but do not validate arguments received at runtime. Validate important or destructive tool inputs inside the handler.

### Handler styles

#### Inline handler

Keep the implementation next to the definition:

```ts
const getSelectedText = defineTool({
  name: "get_selected_text",
  description: "Read the text currently selected on the page.",
  argsSchema: noArgs,
  handler: () => ({
    text: window.getSelection()?.toString() ?? "",
  }),
});
```

Only the serializable metadata is sent to Core. The function remains in the browser.

#### Named handler

Register or override a handler after session creation:

```ts
const dispose = session.onTool(
  "update_counter",
  async (args: UpdateCounterArgs, ctx) => {
    console.log(ctx.toolRequestId, ctx.turnId);
    counter.value += args.delta;
    return { value: counter.value };
  },
);

// Remove the handler when its UI or state is destroyed.
dispose();
```

Named handlers are useful when the implementation belongs to a mounted component or when handlers must be restored after resuming a session.

#### Generic fallback handler

```ts
const dispose = session.onTool(async (tool, args, ctx) => {
  switch (tool) {
    case "get_current_page":
      return { title: document.title, url: location.href };
    default:
      return {
        error: {
          code: "UNSUPPORTED_TOOL",
          message: `No application handler for ${tool}`,
        },
      };
  }
});
```

Handler priority is:

1. named `session.onTool("tool_name", handler)`;
2. inline `handler` from `defineTool()`;
3. generic `session.onTool((tool, args, ctx) => ...)`;
4. automatic `TOOL_HANDLER_NOT_FOUND` result.

### Preserve tool-name types

Keep tools in a readonly tuple so `session.onTool()` only accepts declared names:

```ts
const tools = [getSelectedText, updateCounter] as const;

const session = await pedelec.createSession({
  provider: "codex",
  skills: {
    guidance: "Use the declared tools for page operations.",
    tools,
  },
});

session.onTool("update_counter", handleCounter);
// session.onTool("misspelled_name", handler); // TypeScript error
```

### Tool results and errors

Return JSON-compatible data: primitives, arrays, and plain objects. Convert values such as `Date`, typed arrays, DOM objects, and class instances before returning them.

Return a structured error when failure is an expected domain result:

```ts
return {
  error: {
    code: "NO_SELECTION",
    message: "There is no active editor selection.",
    retryable: false,
  },
};
```

Throw only for unexpected failures. The SDK catches thrown handler errors and sends a `TOOL_HANDLER_ERROR` result to the agent.

## Session events and status

```ts
session.onStatus((status, ctx) => {
  console.log(ctx.previousStatus, "->", status);
});
```

Possible statuses:

- `idle` — ready for another prompt;
- `running` — the agent is processing a turn;
- `waiting_tool_result` — a browser tool handler is running;
- `ended` — the session cannot accept more input;
- `error` — the session entered an error state.

All event registration methods return an unsubscribe function.

## Resume and end sessions

Sessions end automatically when their last SDK connection disconnects by default. Use `autoEndOnDisconnect: false` when a session must survive navigation or reload:

```ts
const session = await pedelec.createSession({
  provider: "codex",
  autoEndOnDisconnect: false,
});

localStorage.setItem("pedelec-session-id", session.sessionId);
```

Resume it later:

```ts
const sessionId = localStorage.getItem("pedelec-session-id");

if (sessionId) {
  const session = await pedelec.resumeSession(sessionId);

  // Restore listeners and browser tool handlers before sending text.
  session.onChat(appendAssistantDelta);
  session.onTool("update_counter", handleCounter);

  await session.sendText("Continue the previous task.");
}
```

When `autoEndOnDisconnect` is disabled, your app owns cleanup:

```ts
await session.end();
```

## Check connection and providers

```ts
const approval = await pedelec.getApprovalStatus();
const providers = await pedelec.listProviders();

const availableProviders = providers.filter((provider) => provider.available);
```

`getApprovalStatus()` returns `installed`, `approved`, and the current `origin`. The extension may ask the user to approve an origin when it first creates or resumes a session.

## Error handling

Use both `try/catch` for the action that initiated a request and `onError()` for asynchronous session-level failures:

```ts
session.onError((error, ctx) => {
  console.error(ctx.sessionId, error.code, error.message, error.details);
});

try {
  await session.sendText("Update the selected content.");
} catch (error) {
  console.error("Turn failed", error);
}
```

SDK errors have this shape:

```ts
type PedelecError = {
  code: string;
  message: string;
  details?: unknown;
};
```

Common codes include `EXTENSION_UNAVAILABLE`, `EXTENSION_DISCONNECTED`, `SESSION_BUSY`, `SESSION_ENDED`, `INVALID_INPUT`, and provider/runtime-specific errors.

## Documentation

- [Pedelec documentation](https://kaoruisaac.github.io/pedelec/)
- [GitHub repository](https://github.com/kaoruisaac/pedelec)
