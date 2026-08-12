# Pedelec SDK Solid Demo

This demo is a small SolidJS playground for validating the browser `pedelec` SDK through the Pedelec Chrome extension.

It demonstrates:

- SDK initialization and extension diagnostics
- `createSession` and `resumeSession`
- multiple independent sessions
- `sendText`, streaming chat deltas, session errors, and ended sessions
- frontend tool handlers and tool result/error display
- per-session transcript, tool call log, error log, and debug event log
- selecting an optional application-owned sandbox with `directoryPicker()`
- creating sessions with an explicit sandbox workspace

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

## Optional Sandbox

The Create Session panel can optionally select the workspace used by a new session:

- Leave Sandbox unselected to use the Desktop-managed temporary sandbox.
- Select directory to choose an application-owned workspace with the native directory picker.
- Click Clear to return to the Desktop-managed temporary sandbox.

The selected workspace remains in the form after a session is created, so consecutive demo sessions can share it.

## Common Errors

- `Extension unavailable`: load the Pedelec extension and confirm the extension id matches the SDK.
- `Approval rejected`: approve this origin from the Pedelec extension popup before creating or resuming a session.
- `Session busy`: wait for the active session to finish before sending another message.
- `Session ended`: create or resume another session before sending text.
