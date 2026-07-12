---
name: pedelec-sdk
description: Use this skill when implementing, reviewing, debugging, or modifying a browser-side integration that uses @kaoruisaac/pedelec, including clients, sessions, skills, tools, handlers, lifecycle management, resume behavior, and error handling.
---

# Pedelec SDK Integration Skill

## Scope

Use this skill whenever a task touches:

- `Pedelec` or `PedelecSession`;
- session creation, preparation, messaging, resume, or cleanup;
- `skills.guidance`;
- browser-side tool definitions or handlers;
- streamed chat, status, or error events;
- UI lifecycle safety around tool execution.

This is an implementation guardrail, not a complete API reference.

Project requirements, existing architecture, and tests take priority over generic examples in this file.

## Source of truth

Before changing a Pedelec integration, inspect sources in this order:

1. The project's Pedelec integration specification.
2. Existing Pedelec-related source code.
3. The installed version in `package.json` and the lockfile.
4. `node_modules/@kaoruisaac/pedelec/dist/index.d.ts`.
5. The README shipped with the installed package.
6. The official documentation website.

The installed TypeScript declarations are the final source of truth for available APIs and signatures.

Do not:

- invent an API from memory;
- assume another project uses the same SDK version;
- copy an online example without checking the installed declarations;
- modify the Pedelec SDK or upstream repository unless the task explicitly requires it.

## Hard rules

1. Pedelec SDK code runs in a browser page.
2. Do not initialize it in Node.js, SSR server code, build-time code, or a background worker.
3. Normally share one `Pedelec` client within the same page or tab.
4. Register listeners and required tool handlers before the first real `sendText()`.
5. Treat `onChat()` values as incremental deltas and append them.
6. A session accepts only one active turn at a time.
7. Handle `SESSION_BUSY` even when the UI disables duplicate sends.
8. TypeScript annotations do not validate agent-supplied arguments at runtime.
9. Tool results must be compact, JSON-compatible values.
10. Unregister handlers before destroying the UI state they depend on.
11. Unregistering a handler does not cancel a handler already executing.
12. A tool timeout does not stop the handler's JavaScript execution.
13. `sessionId`, `turnId`, and `toolRequestId` are correlation identifiers, not authorization or lifecycle proof.
14. Mutation tools must validate current application-owned identity, lifecycle, version, permission, or ownership before changing state.
15. Do not automatically retry a mutation when the first attempt may already have applied its side effect.

## Standard workflow

### 1. Determine ownership

Before implementation, identify:

- where the shared client lives;
- what application object owns each session;
- whether sessions are page-scoped, route-scoped, document-scoped, or persistent;
- when handlers are registered and removed;
- when sessions end;
- whether reload or navigation must resume sessions;
- which tab or component may perform mutations.

Do not change session lifetime implicitly.

`autoEndOnDisconnect` defaults to `true`.

Use `autoEndOnDisconnect: false` only when the product requires persistence across navigation, reload, or disconnection. In that case the application must store the session ID, restore handlers after resume, and eventually call `session.end()`.

### 2. Create a browser-side client

```ts
import { Pedelec } from "@kaoruisaac/pedelec";

const pedelec = new Pedelec();
```

Create it only after a browser environment exists.

In an SSR-capable framework, initialize it through the framework's browser-only lifecycle or a guarded client module.

Do not create a new client for every button click or message.

### 3. Check installation and providers when needed

```ts
const approval = await pedelec.getApprovalStatus();
const providers = await pedelec.listProviders();
```

Use these APIs for installation, approval, provider, and model UI.

A prior availability check is not a guarantee. Session creation and provider execution remain the final authority.

### 4. Define tools

Use `defineTool()` and preserve the tool tuple with `as const`.

```ts
import {
  defineTool,
  type ToolArgsSchema,
} from "@kaoruisaac/pedelec";

const noArgs = {
  type: "object",
  properties: {},
  required: [],
} satisfies ToolArgsSchema;

const getCurrentPage = defineTool({
  name: "get_current_page",
  description:
    "Read the current page title and URL. This tool does not modify application state.",
  argsSchema: noArgs,
  handler: () => ({
    title: document.title,
    url: location.href,
  }),
});

export const pedelecTools = [getCurrentPage] as const;
```

The readonly tuple preserves literal tool names for typed named handlers.

### 5. Create the session

```ts
const session = await pedelec.createSession({
  provider: "codex",
  skills: {
    guidance: [
      "Use the declared tools to interact with the current application.",
      "Read current state before attempting a mutation.",
      "Do not claim a mutation succeeded unless its result confirms success.",
    ].join("\n"),
    tools: pedelecTools,
  },
});
```

`skills.guidance` should contain product-facing operating rules, such as:

- which tool to use;
- required read-before-write flow;
- prohibited actions;
- confirmation requirements;
- how to recover from structured errors;
- which result fields prove success.

Do not use guidance as a copy of the SDK manual.

### 6. Register listeners and handlers

Register listeners immediately after creating or resuming the session.

```ts
let assistantText = "";

const offChat = session.onChat((delta) => {
  assistantText += delta;
  renderAssistantMessage(assistantText);
});

const offStatus = session.onStatus((status) => {
  renderSessionStatus(status);
  setComposerDisabled(status !== "idle");
});

const offError = session.onError((error, ctx) => {
  reportPedelecError(error, ctx);
});

const offEnded = session.onEnded(() => {
  setComposerDisabled(true);
});
```

Read the initial status with `session.getStatus()` because `onStatus()` only fires on changes.

Register lifecycle-bound named handlers before sending the first prompt.

### 7. Optionally prepare

```ts
await session.prepare();
```

`prepare()` is an optional warm-up optimization.

Correct behavior must not depend on it. `sendText()` must still work when preparation was not called or failed.

### 8. Send one turn at a time

```ts
try {
  await session.sendText(userText);
} catch (error) {
  handleSendFailure(error);
}
```

`sendText()` resolves when the agent turn finishes, not when transport merely accepts the text.

Use both:

- `try/catch` around initiating actions;
- `session.onError()` for asynchronous session or transport failures.

### 9. Clean up

All event and handler registration methods return unsubscribe functions.

```ts
function disposeBindings() {
  offChat();
  offStatus();
  offError();
  offEnded();
  offTool();
}
```

If the application owns the session lifetime:

```ts
await session.end();
disposeBindings();
```

If the session survives the current route or component, unregister handlers referencing old UI state and register new handlers only after the replacement UI is ready.

## Tool authoring

### Name

Tool names must:

- match `^[a-zA-Z][a-zA-Z0-9_.-]*$`;
- be unique in one `skills.tools` array;
- remain stable unless an intentional capability change requires renaming.

Prefer lowercase snake case:

```text
get_document
replace_selection
rename_document
```

Renaming only the definition or only the handler creates an integration bug.

### Description

A description must explain:

1. what the tool does;
2. what state it reads or changes;
3. important preconditions;
4. important side effects;
5. expected failure conditions;
6. when a similar tool should be used instead.

Good:

```text
Replace the selected editor text. Fails when there is no selection or when
expectedVersion does not match the current document. Use insert_text when
there is no selection.
```

Bad:

```text
Edit text.
```

Never hide destructive or irreversible behavior behind a vague description.

### Argument schema

The root `argsSchema` must always be an object, including no-argument tools.

Pedelec supports a schema subset containing:

- `string`;
- `number`;
- `integer`;
- `boolean`;
- `array`;
- `object`;
- `oneOf`.

Do not assume full JSON Schema support.

Use schema descriptions, required fields, enums, bounds, and lengths to improve agent output.

Still validate important values inside the handler:

```ts
function parseRenameArgs(raw: unknown) {
  if (!raw || typeof raw !== "object") return null;

  const value = raw as Record<string, unknown>;

  if (typeof value.documentId !== "string" || value.documentId.length === 0) {
    return null;
  }

  if (!Number.isInteger(value.expectedVersion)) {
    return null;
  }

  if (typeof value.name !== "string" || value.name.trim().length === 0) {
    return null;
  }

  return {
    documentId: value.documentId,
    expectedVersion: value.expectedVersion as number,
    name: value.name.trim(),
  };
}
```

### Results

Return only JSON-compatible data:

- `null`;
- strings, numbers, booleans;
- JSON-compatible arrays;
- plain JSON-compatible objects.

Convert rich values explicitly:

```ts
return {
  savedAt: date.toISOString(),
  bytes: Array.from(bytes),
  entries: [...map.entries()],
};
```

Do not return DOM nodes, functions, cyclic objects, `BigInt`, unconverted `Map` or `Set`, browser handles, secrets, or unclear class instances.

Keep results compact. Return an application-owned ID and summary instead of a large artifact.

### Expected domain errors

When the handler works correctly but the action is not allowed or cannot complete, return a structured error:

```ts
return {
  error: {
    code: "VERSION_CONFLICT",
    message: "The document changed. Read current state before retrying.",
    details: {
      documentId,
      expectedVersion,
      actualVersion,
    },
    retryable: true,
  },
};
```

Use structured errors for:

- invalid arguments;
- missing resources;
- invalid UI state;
- permission denial;
- user cancellation;
- version conflicts;
- stale lifecycle;
- missing selection.

Prefer a project-wide shape:

```ts
type ToolErrorResult = {
  error: {
    code: string;
    message: string;
    details?: unknown;
    retryable?: boolean;
  };
};
```

`details` must be JSON-compatible and must not contain secrets.

Throw or reject only for unexpected programming, infrastructure, or dependency failures. The SDK converts thrown handler failures into a `TOOL_HANDLER_ERROR` result.

## Handler choice

### Inline handler

Use when the capability is stable and implementation naturally belongs beside its definition.

### Named handler

Use when:

- implementation depends on a mounted component;
- implementation is route-local or resource-local;
- a session needs an override;
- the tool needs explicit lifecycle or permission checks;
- handlers must be restored after resume.

```ts
const offTool = session.onTool("rename_document", handleRename);
```

### Generic handler

Use for central routing, diagnostics, or a deliberate fallback.

```ts
const offFallback = session.onTool(async (tool, args, ctx) => {
  return routeToolCall(tool, args, ctx);
});
```

Do not use a generic handler to avoid per-tool validation.

Handler priority is:

1. named `session.onTool("tool_name", handler)`;
2. inline handler from `defineTool()`;
3. generic handler;
4. automatic `TOOL_HANDLER_NOT_FOUND` result.

A named handler overrides an inline handler with the same name.

## UI lifecycle safety

Tool calls are asynchronous. Before execution, the user may:

- navigate;
- switch documents;
- replace an editor or canvas;
- close a modal;
- unmount a component;
- change selection;
- lose permission;
- transfer ownership to another tab.

The SDK cannot prove that a closure-captured object is still current.

Every mutation tool must validate the strongest relevant application-owned state:

- resource ID;
- entity version;
- editor or canvas generation;
- mounted or disposed state;
- current user permission;
- session-to-resource mapping;
- tab ownership;
- idempotency state.

Example pattern:

```ts
function attachDocumentTools(
  session: PedelecSession,
  documentId: string,
  generation: number,
) {
  return session.onTool("rename_document", (raw, ctx) => {
    const args = parseRenameArgs(raw);

    if (!args) {
      return {
        error: {
          code: "INVALID_TOOL_ARGS",
          message: "documentId, expectedVersion, and name are required.",
          retryable: true,
        },
      };
    }

    if (
      documentStore.activeId !== documentId ||
      documentStore.generation !== generation
    ) {
      return {
        error: {
          code: "STALE_TOOL_CALL",
          message: "The active document changed before the tool executed.",
          details: {
            sessionId: ctx.sessionId,
            turnId: ctx.turnId,
          },
          retryable: false,
        },
      };
    }

    const document = documentStore.get(documentId);

    if (!document) {
      return {
        error: {
          code: "DOCUMENT_NOT_FOUND",
          message: "The target document no longer exists.",
          retryable: false,
        },
      };
    }

    if (document.version !== args.expectedVersion) {
      return {
        error: {
          code: "VERSION_CONFLICT",
          message: "The document changed. Read current state before retrying.",
          retryable: true,
        },
      };
    }

    return documentStore.rename(documentId, args.name);
  });
}
```

Do not use `ctx.turnId` alone as a lifecycle guard. One turn may continue while the route or resource changes.

Before replacing UI state:

1. invalidate the old generation or lifecycle token;
2. unregister old handlers;
3. resolve or cancel application-owned interactive UI;
4. decide whether to end, persist, or transfer the session;
5. register new handlers only after new state is ready.

An already executing handler still needs its own lifecycle checks.

## Destructive and sensitive tools

For destructive, irreversible, privileged, or externally visible actions, verify:

- exact resource identity;
- current version;
- current user permission;
- whether the operation already ran;
- whether explicit confirmation is required;
- allowed bounds and values;
- idempotency and replay behavior.

Do not infer authorization from an agent request or context identifier.

Do not expose tools for actions the product does not intend the agent to perform.

Prefer separate read and mutation tools over one broad ambiguous tool.

Use this flow for high-impact actions:

```text
read current state
→ obtain required confirmation
→ mutate with resource ID and expected version
→ verify the structured success result
```

## Timeouts and retries

`timeoutMs` is optional and must be a positive integer. Core currently defaults to 60 seconds when it is omitted.

A timeout:

- stops the runtime from waiting;
- does not cancel browser JavaScript;
- does not prove the side effect failed;
- may cause late result submission failure.

Handlers must implement their own cancellation and cleanup when needed.

Do not automatically retry after:

- timeout;
- result submission failure;
- unknown transport failure after a possible side effect;
- stale lifecycle;
- user cancellation.

Reconcile current state before retrying a mutation.

For retry-safe operations, use one or more of:

- idempotency key;
- `expectedVersion`;
- stable resource ID;
- operation ID;
- explicit read-after-failure reconciliation.

## Status and error handling

Known statuses are:

- `idle`;
- `running`;
- `waiting_tool_result`;
- `ended`;
- `error`.

`waiting_tool_result` means a browser tool handler is running or being awaited.

Do not send another turn until the session is ready.

Distinguish:

- expected structured domain errors;
- `TOOL_HANDLER_NOT_FOUND`;
- `TOOL_HANDLER_ERROR`;
- result submission failures;
- extension or transport failures;
- Core or provider failures;
- session status errors.

A structured domain error does not automatically trigger `session.onError()`.

Log correlation information when useful:

```ts
console.error("Pedelec tool failed", {
  sessionId: ctx.sessionId,
  turnId: ctx.turnId,
  toolRequestId: ctx.toolRequestId,
  tool: ctx.tool,
  code: error.code,
});
```

Do not log complete tool arguments by default. They may contain private application or user data.

Do not blindly retry after `SUBMIT_TOOL_RESULT_FAILED`; the side effect may already have completed.

## Resume and multi-tab behavior

After `resumeSession(sessionId)`, restore before the next turn:

- chat listener;
- status listener;
- error listener;
- ended listener;
- required named or generic handlers;
- application session-to-resource mapping;
- mutation ownership and lifecycle guards.

Browser handlers do not survive reload.

Persist provider, model, resource, or creation metadata separately when the UI needs it after reload.

Two tabs may attach to the same persistent session. The application must explicitly decide which tab may mutate state.

Possible strategies:

- prevent cross-tab resume;
- keep an owner lease in shared storage;
- elect a controller with `BroadcastChannel`;
- expose read-only tools in secondary tabs;
- reject mutation tools when the tab lacks ownership.

Pedelec context identifiers are not distributed locks.

## Existing-project boundaries

When changing an existing project:

- reuse its shared client;
- reuse existing tool registries and error helpers;
- follow current session ownership;
- follow framework lifecycle conventions;
- keep business rules outside generic SDK wrappers;
- avoid new abstractions unless the task requires them;
- do not silently change persistence behavior;
- do not rename tools without updating guidance, handlers, tests, and compatibility expectations;
- do not add broad or destructive tools merely to simplify implementation.

A possible organization is:

```text
src/pedelec/
├─ client.ts
├─ session.ts
├─ tools.ts
├─ tool-errors.ts
└─ lifecycle.ts
```

This is only a suggestion. Existing project structure takes precedence.

## Pre-implementation checklist

Before changing code, determine:

- the installed SDK version;
- where the shared client is created;
- who owns each session;
- whether sessions are temporary or persistent;
- existing tool names and guidance;
- current inline, named, and generic handlers;
- UI resources that may change during a turn;
- identity or version fields protecting mutations;
- cleanup behavior on unmount, route change, reload, and close;
- confirmation and permission requirements.

Do not silently invent a missing product decision.


## References

- Official documentation: https://kaoruisaac.github.io/pedelec/
- Repository: https://github.com/kaoruisaac/pedelec
- npm package: https://www.npmjs.com/package/@kaoruisaac/pedelec