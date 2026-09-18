# Pedelec SDK Solid Demo

This demo is a small SolidJS playground for validating the browser `pedelec` SDK through the Pedelec Chrome extension.

The transcript follows the current SDK chat contract: `onChatDelta()` is used for best-effort incremental rendering, while `onChat()` supplies completed logical assistant messages and reconciles the final text.

It demonstrates:

- SDK initialization and extension diagnostics
- `createSession`, `resumeSession`, same-handle session resume, and explicit `prepare()` before the first user turn
- multiple independent sessions
- `sendText`, completed chat messages, streaming chat deltas, session errors, and ended sessions
- frontend tool handlers and tool result/error display
- per-session transcript, tool call log, error log, and debug event log
- the managed `Pedelec.createSession()` flow and explicit `PedelecWorkspace.createSession()` flow
- `session.workspace.path` visibility without exposing a Desktop-managed absolute path
- `workspace.listFiles()`, `workspace.listFolders()`, and `workspace.run()` with raw output/truncation flags

## Install

Build the local SDK first, then install demo dependencies:

```bash
cd ../../sdk
npm install
npm run build

cd ../demo/solid-sdk-demo
npm install
```

## Run

```bash
npm run dev
```

Open the Vite URL in Chrome. The Pedelec extension must be loaded and the desktop/native host must be running.

Production builds use this extension id:

```txt
ogccgaminlphbkeghldidiiimajfdpag
```

During `npm run dev`, the demo first reads `PEDELEC_DEV_CHROME_EXTENSION_ID` from the repo-root `.env.local` and falls back to the production id when the value is missing or invalid:

```txt
PEDELEC_DEV_CHROME_EXTENSION_ID=mifjcaefhmigmhmejhficbnhgnecfibk
```

## Demo Tools

The page registers these frontend tools:

- `get_current_page`: returns the current page title and URL
- `get_selected_text`: returns the user's current text selection
- `ask_user`: opens an in-page prompt and returns the submitted text
- `throw_error`: intentionally throws to demonstrate tool handler errors

Unknown tools return a structured `TOOL_HANDLER_NOT_FOUND` result.

## Workspace flows

The Create Session panel makes the two public flows explicit:

- Leave the Workspace in managed mode to call `pedelec.createSession()`.
- Click Open explicit Workspace to call `pedelec.openWorkspace()`. Cancellation leaves the current selection unchanged; a selected `PedelecWorkspace` handle is retained and used for `selectedWorkspace.createSession()`.
- Click Use managed to switch future sessions back to the managed flow. Existing sessions keep their own `session.workspace` handle, so multiple sessions can share one explicit Workspace.

The Workspace API panel exercises the active/selected Workspace directly. Its list path is omitted when empty, and the script result displays `exitCode`, raw `stdout`/`stderr`, and both truncation flags without parsing stdout.

## Common Errors

- `Extension unavailable`: load the Pedelec extension and confirm the extension id matches the SDK.
- `Approval rejected`: approve this origin from the Pedelec extension popup before creating or resuming a session.
- `Session busy`: wait for the active session to finish before sending another message.
- `Session ended`: resume the ended active session or create/resume another session before sending text.
