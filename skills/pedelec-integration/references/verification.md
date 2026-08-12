# Integration verification

Read this checklist before declaring the integration complete.

## SDK lifecycle

- Client initialization is browser-only; there is no SSR / Node dead client.
- Shared client and session ownership are explicit.
- One active turn is enforced and `SESSION_BUSY` races are handled.
- Listeners and required handlers exist before the first real turn.
- Chat deltas append to the correct session / turn.
- End, unmount, route change, reload, and error paths clean up listeners and handlers.
- Resume restores UI listeners, handlers, resource mapping, and ownership guards.
- `autoEndOnDisconnect` matches the persistence requirement.
- Sandbox ownership and explicit workspace conflict rules are understood.

## Tool contracts

- Every tool expresses a product intent, not a generic action or DOM operation.
- Every handler input is represented in the schema; required, optional, nested, array, enum, range, and length constraints agree.
- Optional omissions have defined behavior and schema defaults are not assumed to be applied.
- Runtime validation exists; TypeScript annotations are not treated as validation.
- Mutation handlers validate resource identity, lifecycle, version, permission, ownership, and idempotency as relevant.
- Results are compact JSON-compatible data and do not contain secrets or rich browser objects.
- Duplicate, timeout, ambiguous failure, and retry semantics are explicit.
- High-impact actions have the required confirmation and visible feedback.

## Connection and UI

- Checking, Extension unavailable, Desktop unavailable, approval required, Provider setup incomplete, ready, running, and failure are distinguishable.
- Setup actions match the failing layer; Recheck runs a real probe.
- User input and unsaved work survive setup, connection, and task failure.
- The chosen integration level's required UI capabilities exist in an appropriate scope.
- Web controls use Provider + `default | low | high` effort semantics; there is no arbitrary Web-side Model editor.
- Running tasks do not make the UI look frozen, show fake percentages, or expose private reasoning.
- Accessibility does not rely on color alone.
- Agent Chat, if present, separates user / Agent / system messages, appends deltas, handles typing and tool-wait states, and prevents concurrent turns.

## Technical source of truth

- API calls and return shapes were checked against the installed `node_modules/@kaoruisaac/pedelec/dist/index.d.ts`.
- Package README and official docs were checked for material contract divergence.
- No API was copied from an older skill or another SDK version without verification.

Report the integration level, tools changed, schema / handler parity, UI capabilities, Provider / Effort boundary, unavailable flows, tests or manual checks, and anything still pending.

