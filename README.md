![Pedelec](./assets/readme-banner.png)

English | [繁體中文](./README.zh-TW.md)

### ➡️ [Pedelec Document](https://kaoruisaac.github.io/pedelec) 🔗

Pedelec is a bridge architecture that lets web frontends call local AI coding agents.

Its core goal is: **to let a Web App create agent sessions through the SDK, send user messages, receive streamed agent responses, and safely hand tool calls back to the Web App whenever the agent needs to operate on frontend state.**

### Agent-assisted Web App integration

Coding Agents can use the repository's integration workflow to inspect an existing browser application and connect its SDK, tools, session lifecycle, and required UI:

```bash
npx skills add kaoruisaac/pedelec
```

This installs the `pedelec-integration` Agent Skill. It is a developer tool for integrating a Web project; it is not a way to install the Pedelec Desktop App or Chrome Extension for end users.

If you do not use the Skills CLI, download `pedelec-integration-guideline.zip` from the matching [GitHub Release](https://github.com/kaoruisaac/pedelec/releases), extract it, and ask your Coding Agent to read `START_HERE.md`. This is a Coding Agent integration bundle, not a Pedelec Desktop App or Chrome Extension installer.

The overall data flow can be understood as:

```txt
Web App / SDK
  ↓ chrome.runtime.connect(extensionId)
Chrome Extension Background
  ↓ origin approval gate
  ↓ Chrome Native Messaging
pedelec-native-host
  ↓ Core IPC
pedelec-app Desktop Runtime
  ↓ provider process
Codex / Antigravity / OpenCode / Cursor / Claude Code / Ollama via pedelec-agent
```

The Web App does not directly touch local processes and does not need to start its own localhost server. The SDK only communicates with the extension; the extension forwards requests to the native host; the desktop app is the only CoreRuntime owner and is responsible for creating sessions, managing provider processes, forwarding events, and handling tool results.

---

## Repo Structure

```txt
sdk/        TypeScript SDK used by Web Apps
extension/  Chrome extension responsible for external SDK connections, origin approval, and the native messaging bridge
desktop/    Tauri desktop app, CoreRuntime, native host, and pedelec-cli
```

---

## Top-Level Architecture

```mermaid
sequenceDiagram
  participant App as Web App
  participant SDK as Pedelec SDK
  participant BG as Extension Background
  participant NH as pedelec-native-host
  participant Desktop as pedelec-app CoreRuntime
  participant Agent as Provider Agent CLI

  App->>SDK: createSession({ provider, effortLevel, skills })
  SDK->>BG: chrome.runtime.connect external message
  BG->>BG: verify sender origin approval
  BG->>NH: native messaging create_thread
  NH->>Desktop: Core IPC create_thread
  Desktop-->>NH: { threadId }
  NH-->>BG: response
  BG-->>SDK: response
  SDK-->>App: PedelecSession

  App->>SDK: session.sendText(prompt)
  SDK->>BG: send_text
  BG->>NH: send_text
  NH->>Desktop: Core IPC send_text
  Desktop->>Agent: run / resume provider process
  Agent-->>Desktop: assistant output / tool call / done
  Desktop-->>NH: thread_event
  NH-->>BG: thread_event
  BG-->>SDK: SDK event
  SDK-->>App: onChat / onStatus / onTool
```

### Responsibilities of Each Layer

| Layer | Responsibility |
| --- | --- |
| SDK | Provides the Web App API, maintains session callbacks, handles request timeouts and event deduplication |
| Background | Manages SDK external channels, origin approval, native host connections, and converts core events into SDK events |
| Native Host | Chrome Native Messaging entry point that forwards requests/events to Core IPC |
| Desktop Runtime | The only session/runtime owner, managing threads, skills, provider processes, and tool requests |
| Agent process | Actually runs Codex/Antigravity/OpenCode/Cursor/Claude Code/Ollama and calls frontend tools through `pedelec-cli` |

---

## Development Workflow

### Development Extension ID

When using the unpacked extension, first create `.env.local` in the repo root:

```txt
PEDELEC_DEV_CHROME_EXTENSION_ID=mifjcaefhmigmhmejhficbnhgnecfibk
```

Desktop debug builds and the demo dev server read this value. If it cannot be read or the format is invalid, the production extension ID is used.

### Starting the Desktop App

```bash
cd desktop
npm install
npm run tauri dev
```

### Loading the Chrome Extension

1. Open `chrome://extensions`.
2. Enable Developer mode.
3. Click **Load unpacked**.
4. Select the `extension/` folder.
5. Start or restart the Desktop App so it registers the native messaging host.

### Building the SDK

```bash
cd sdk
npm install
npm run build
```
