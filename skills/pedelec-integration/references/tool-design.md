# Tool design

Tools are product contracts, not UI automation primitives. Start from the user intent and expose stable product capabilities such as `rename_document`, `create_task`, `apply_selected_style`, or `publish_draft`.

Avoid `click_selector`, `execute_javascript`, `execute_action`, `set_dom_value`, and other generic contracts. Do not use CSS selectors, DOM positions, temporary screen indexes, arbitrary JavaScript, or an undefined generic object as the primary contract.

## Contract checklist

For every proposed tool, write down:

- stable product-intent `name` and purpose;
- required and optional arguments;
- behavior when each optional argument is omitted;
- enum, range, length, array-item, and nested-object constraints;
- application state read;
- application state changed;
- success evidence returned to the Agent and visible to the user;
- structured failure behavior;
- idempotency and duplicate risk;
- confirmation, permission, ownership, and version requirements.

The schema is the Agent-facing interface. Every handler input must be represented in `argsSchema`; required fields must match what the handler truly needs; nested and array shapes must be explicit. Do not rely on TypeScript types or schema defaults for runtime behavior. Use only the schema subset supported by the installed SDK and verify it in the installed declarations.

## Runtime validation and safety

Validate important arguments inside every handler. Validate the strongest relevant application-owned guards before mutations: stable resource identity, current version, lifecycle / generation, mounted state, current permission, session-to-resource mapping, tab ownership, and idempotency state. A session or tool context ID does not authorize a change.

Prefer separate read and mutation tools. For destructive, irreversible, privileged, externally visible, payment, publish, delete, or large-batch operations, use:

```text
read current state
→ obtain required user confirmation
→ mutate with stable identity and expected version
→ verify and return structured success
```

## Results and errors

Return compact JSON-compatible values: IDs, summaries, bounded arrays, and serializable timestamps. Return structured domain errors for invalid arguments, missing resources, stale UI, permission denial, user cancellation, version conflicts, missing selection, and other expected business outcomes. A consistent shape is useful:

```ts
{
  error: {
    code: string,
    message: string,
    details?: unknown,
    retryable?: boolean
  }
}
```

Do not put secrets in error details. Throw only for unexpected programming, infrastructure, or dependency failures; the SDK can convert handler failures to its technical error result.

## Handlers, retries, and UI safety

Use inline handlers for stable capabilities. Use named handlers for route-local or mounted resources, session overrides, permission checks, and resume restoration. A generic handler may route deliberately, but must not replace per-tool validation.

Tools are asynchronous. A user can navigate, replace an editor, change selection, unmount a modal, lose permission, or transfer a tab while a call is pending. Invalidate old generations, unregister old handlers, and check current state inside the handler. Timeouts do not cancel JavaScript. Do not automatically retry an operation that may already have applied a side effect; reconcile state or use an idempotency key / operation ID first.

