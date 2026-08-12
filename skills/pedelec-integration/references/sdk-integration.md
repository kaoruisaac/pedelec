# SDK integration guardrails

Use the installed `@kaoruisaac/pedelec` declarations as the API source of truth. The following are architectural hard rules.

## Browser and client ownership

- Pedelec SDK code runs in a browser page. Do not create a usable client in SSR, Node, build-time code, or a background worker.
- In SSR or hydration frameworks, initialize from the browser-only lifecycle or a guarded client module.
- Normally share one `Pedelec` client within a page or tab. Do not create a client for every button click or message.
- Decide whether the client and each session are page-, route-, document-, or persistent-scoped before implementation.

## Session setup

- Register listeners and required tool handlers before the first real `sendText()`.
- `onChat()` delivers deltas; append them to the current Agent message rather than replacing text or creating a bubble per delta.
- A session has one active turn at a time. Disable duplicate sends in the UI and still handle a `SESSION_BUSY` race from another event or tab.
- `prepare()` is an optional warm-up; correctness must not depend on it.
- Wrap initiating calls in `try/catch` and also subscribe to session errors for asynchronous failures.
- Read the initial status explicitly because a status listener may only receive changes.

## Availability probes

- `getApprovalStatus()` is suitable for non-sensitive Extension / approval / Desktop connection status.
- `getApprovalStatus().appConnected` is a Desktop connection signal, not Provider readiness.
- `checkAvailability()` is the complete readiness probe for Extension, approval, and Desktop. A successful preflight does not guarantee session creation or Provider execution.
- `getSettings()` and `listProviders()` require approval and are not side-effect-free initial probes; use them when their public data is needed.
- `getSettings()` exposes only `defaultProvider`. Do not expect actual model mappings, provider-native effort arguments, credentials, API keys, executable paths, or full Desktop settings.

## Runtime safety

- TypeScript annotations and `satisfies ToolArgsSchema` do not validate Agent-supplied arguments at runtime.
- Tool results must be compact JSON-compatible values. Convert dates, bytes, maps, and sets explicitly; never return DOM nodes, functions, cyclic objects, `BigInt`, handles, secrets, or unclear class instances.
- Unregister handlers before destroying UI state they reference. Unregistering does not cancel a handler already executing.
- A tool timeout stops the SDK from waiting but does not stop browser JavaScript execution. Late side effects and result-submission failures remain possible.
- `sessionId`, `turnId`, and `toolRequestId` are correlation identifiers, not authorization, lifecycle, or ownership proof.
- Every mutation must validate application-owned identity, lifecycle / generation, current resource version, permission, ownership, and idempotency as relevant before changing state.
- Do not blindly auto-retry a mutation after timeout, ambiguous transport failure, result-submission failure, or any event that may already have produced a side effect. Reconcile current state first.

## Guidance and boundaries

Put product operating rules in `skills.guidance`: read-before-write behavior, confirmation requirements, prohibited actions, recovery from structured errors, and which result fields prove success. Do not use guidance as a copy of the SDK manual.

Keep product rules in product code and tool handlers. Reuse an existing shared client, tool registry, error helper, and framework lifecycle when working in an existing application. Do not change persistence or add broad tools merely to simplify integration.

## Handler and status interpretation

When the installed SDK supports the standard handler model, a named session handler takes precedence over an inline handler with the same name, followed by a generic handler; if none applies, return the SDK's documented missing-handler result. Do not use a generic fallback to skip per-tool validation.

Treat `idle`, `running`, `waiting_tool_result`, `ended`, and `error` as distinct session states when they exist in the installed declarations. Distinguish expected structured domain errors from missing handlers, handler failures, result-submission failures, Extension / transport failures, Core failures, and Provider failures. A structured domain error does not necessarily trigger the session error callback. Log correlation IDs only when useful and do not log complete tool arguments by default.
