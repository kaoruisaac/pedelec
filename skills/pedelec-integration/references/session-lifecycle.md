# Session lifecycle

## Ownership decision

Before coding, decide where a session lives and who may mutate the related application resource. Document whether it is page-scoped, route-scoped, document-scoped, or persistent across navigation / reload. Do not change session lifetime implicitly.

`autoEndOnDisconnect` defaults to `true`. Use `autoEndOnDisconnect: false` only when persistence across navigation, reload, or disconnection is a product requirement. Then persist the session ID and application metadata, restore handlers after resume, and eventually call `session.end()`.

## Bindings and turns

After creating or resuming a session, establish the chat, status, error, ended, and required tool-handler bindings before the next turn. Keep one active turn per session. A UI lock is not enough: handle busy errors and stale callbacks.

On route, document, editor, or selection changes:

1. Invalidate the old application generation or lifecycle token.
2. Unregister handlers that reference the old UI or resource.
3. Resolve or cancel application-owned interactive UI.
4. End, persist, or transfer the session according to the ownership decision.
5. Register new handlers only after the replacement UI and resource state are ready.

Handlers already running need their own lifecycle checks; unregistering cannot stop them.

## End and cleanup

All listener and handler registration methods return unsubscribe functions. Dispose them deterministically. If the application owns the session, unregister dependent bindings and then call `session.end()` at the chosen lifecycle boundary. Do not keep UI callbacks attached to an ended handle.

Ending a session makes its handle unusable but does not mean every workspace path is immediately deleted. A session without `workspace.path` uses temporary Desktop-managed storage, cleaned on normal Desktop exit and stale cleanup. An absolute `workspace.path` is application-managed: Pedelec does not delete it on end or stale cleanup. Multiple active sessions may share an explicit workspace, but the application owns write-conflict prevention and must not overlap the managed workspace root.

Do not treat a managed workspace as durable storage, and do not expect an ended session to use retained managed storage through asset APIs. Forced termination or locked files can defer managed cleanup.

## Resume and multi-tab behavior

After `resumeSession(sessionId)`, restore all browser-side listeners, handlers, resource mappings, and ownership checks before sending another turn. Browser handlers do not survive reload. Persist Provider, effort level, resource identity, and creation metadata separately if the UI needs them after reload; a resumed handle may not recover all metadata.

SDK-created sessions are owned by their creating origin. A session ID is not a cross-origin bearer credential; changing scheme, host, subdomain, or port is not a valid way to resume.

Two tabs may attach to one persistent session. The application must choose an owner lease, BroadcastChannel controller, read-only secondary tab, or another explicit policy. Pedelec context IDs are not distributed locks.

