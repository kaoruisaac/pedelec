# Pedelec SDK 官方文件規劃

此文件規劃 `dev-docs/` 內 Pedelec SDK 官方文件的資訊架構、章節內容與撰寫優先順序。

目前文件網站使用 Astro Starlight，英文內容位於 `src/content/docs/`，繁體中文內容位於 `src/content/docs/zh-tw/`。正式撰寫時，兩個語系應維持相同頁面結構與對應路徑。

## 文件目標

Pedelec SDK 文件主要服務以下讀者：

1. 想在 Web App 中接入本機 AI agent 的前端開發者。
2. 需要讓 agent 讀取或操作前端狀態的 SDK 使用者。
3. 需要處理 session、串流回應、tool calling 與錯誤狀態的應用程式開發者。

文件應讓第一次接觸 Pedelec 的開發者能依序完成：

1. 理解 Pedelec 解決的問題與整體資料流。
2. 安裝必要元件並確認環境可用。
3. 建立第一個 session 並取得 agent 回應。
4. 定義 tool，讓 agent 可以讀取或操作 Web App。
5. 正確處理 session lifecycle、錯誤與使用者授權。
6. 透過 API Reference 查詢完整型別與方法。

## 文件撰寫原則

- 以公開 SDK API 為主，不把 Desktop Runtime、Native Messaging 或 provider adapter 的內部實作細節混入一般使用指南。
- 所有範例皆使用目前實際存在的 API，例如 `new Pedelec()`、`createSession()`、`sendText()`、`defineTool()`，不再保留示意用的 `connect()` 或 `send()`。
- 先解釋使用情境，再提供程式碼，最後補充型別、限制與錯誤情況。
- TypeScript 為主要範例語言。
- 需要明確標示 SDK 僅能在 browser page context 執行，不適用於 Node.js、SSR server code 或 Web Worker。
- framework-specific 文件只處理 integration pattern，不重複完整 SDK API 說明。
- 英文與繁體中文頁面應同步維護，避免其中一個語系出現獨有功能說明。

---

# 建議側邊欄與章節

## 1. Introduction

### 1.1 Overview

建議路徑：

- `src/content/docs/index.mdx`
- `src/content/docs/zh-tw/index.mdx`

大致內容：

- Pedelec SDK 是什麼。
- Pedelec 適合解決哪些問題。
- Web App 可以透過 SDK 建立 agent session、傳送訊息、接收串流回應與處理 tool call。
- Pedelec 與一般雲端聊天 API 的差異：agent 實際執行於使用者本機環境。
- 簡化版架構圖：Web App → Chrome Extension → Desktop App → Provider。
- 導向 Quick Start、Tool Calling 與 API Reference 的入口。

### 1.2 How Pedelec Works

建議路徑：

- `concepts/how-pedelec-works.mdx`

大致內容：

- SDK、Chrome Extension、Native Messaging Host、Desktop Runtime 與 provider 的角色。
- 一次 `sendText()` 的完整資料流。
- agent 呼叫前端 tool 時，請求如何返回 Web App。
- Web App 不會直接啟動本機 process，也不需要自行開 localhost server。
- 哪些部分由 Pedelec 管理，哪些部分由整合 Pedelec 的 Web App 管理。

此頁只說明外部可觀察的架構，不深入 IPC protocol 或 provider adapter 實作。

### 1.3 Requirements and Browser Support

建議路徑：

- `getting-started/requirements.mdx`

大致內容：

- Pedelec Chrome Extension。
- Pedelec Desktop App。
- Native Messaging Host 必須完成註冊。
- 至少一個可用 provider。
- SDK 必須在 Chrome browser page context 使用。
- 不支援的環境：Node.js、SSR server、background worker、Web Worker。
- 開發環境使用 localhost 與正式網站 origin 的差異。

---

## 2. Getting Started

### 2.1 Installation

建議路徑：

- `getting-started/installation.mdx`

大致內容：

- 從 npm 安裝 `@kaoruisaac/pedelec`。
- ESM 與 TypeScript import 範例。
- Pedelec Desktop App 與 Chrome Extension 的安裝入口。
- 安裝後如何確認 Extension、Desktop App 與 provider 都可使用。
- 本 repo 開發時使用本機 SDK package 的方式，移至「本地開發」提示區塊，避免與一般使用者安裝流程混淆。

### 2.2 Quick Start

建議路徑：

- `getting-started/quick-start.mdx`

大致內容：

- 建立 `Pedelec` client。
- 檢查 `getApprovalStatus()`。
- 使用 `createSession()` 建立 session。
- 使用 `onChat()` 累積串流文字。
- 使用 `onStatus()` 與 `onError()` 更新 UI。
- 呼叫 `sendText()`。
- 呼叫 `end()` 清理 session。
- 完整、可直接複製的最小 TypeScript 範例。
- 說明 `sendText()` 會在本次 turn 完成後 resolve，而不是在訊息剛送出時 resolve。

### 2.3 Origin Approval and Connection State

建議路徑：

- `getting-started/approval-and-connection.mdx`

大致內容：

- 為什麼 Extension 需要核准目前網站 origin。
- `getApprovalStatus()` 的使用方式與回傳型別。
- `installed`、`approved`、`origin` 的意義。
- 首次呼叫 `createSession()` 或 `resumeSession()` 時可能出現的核准流程。
- Connect Pedelec 按鈕建議狀態：未安裝、未核准、可連線、連線中、失敗。
- 常見錯誤：`EXTENSION_UNAVAILABLE`、`APPROVAL_REJECTED`、`APPROVAL_TIMEOUT`、`OPEN_POPUP_FAILED`、`NATIVE_HOST_UNAVAILABLE`。

---

## 3. Core SDK Concepts

### 3.1 Creating the Client

建議路徑：

- `sdk/client.mdx`

大致內容：

- `new Pedelec()`。
- `PedelecOptions`。
- `bridgeTimeoutMs` 的用途、預設值與適合調整的情境。
- client 與 Extension connection 的關係。
- 建議在單一 browser page 共用一個 client instance。
- client 不應在 SSR module initialization 階段建立。

### 3.2 Providers, Models, and Settings

建議路徑：

- `sdk/providers-and-models.mdx`

大致內容：

- `ProviderCode` 支援值。
- `listProviders()` 與 `ProviderInfo`。
- `available`、`path`、`error` 的意義。
- `getSettings()` 與 Desktop App 的 default provider/default model。
- `createSession()` 不指定 provider、只指定 provider、同時指定 provider 與 model 的差異。
- `model` 不能在沒有 `provider` 的情況下單獨指定。
- Ollama 的特殊需求：需要 model，且 provider available 不代表 Ollama server 或 model 已可使用。
- provider model ID 由 provider 本身決定，不應在文件中把範例 model 說成固定支援清單。

### 3.3 Creating Sessions

建議路徑：

- `sdk/creating-sessions.mdx`

大致內容：

- `createSession()` overload 與 `CreateSessionInput`。
- 使用 Desktop App 預設 provider。
- 指定 provider 與 model。
- 傳入 `skills`。
- `autoEndOnDisconnect` 預設為 `true`。
- session 的 `sessionId`、`provider`、`model`。
- `DEFAULT_PROVIDER_NOT_SET`、`DEFAULT_PROVIDER_UNAVAILABLE`、`MODEL_REQUIRED`、`INVALID_INPUT`。

### 3.4 Preparing and Sending a Turn

建議路徑：

- `sdk/sending-messages.mdx`

大致內容：

- `prepare()` 的用途：提前準備 session，以降低第一個 prompt 的等待感。
- `prepare()` 是最佳化，不是呼叫 `sendText()` 的必要前置步驟。
- `sendText(text)` 的完整生命週期。
- 同一 session 不允許同時執行多個 turn。
- `SESSION_BUSY` 與 `SESSION_ENDED`。
- UI 如何避免重複送出。
- 何時將輸入框 disabled，何時恢復可輸入。

### 3.5 Streaming Responses

建議路徑：

- `sdk/streaming-responses.mdx`

大致內容：

- `onChat()` 接收到的是文字 delta，而不是完整訊息。
- 正確累積同一 turn 文字的方法。
- unsubscribe function。
- 多 session UI 如何依 `ctx.sessionId` 分流。
- 使用 `ctx.turnId` 區分 turn，但不要解析或持久化 SDK-local turn ID 的格式。
- 避免把每個 delta 當成獨立 chat message。

### 3.6 Session Status and Events

建議路徑：

- `sdk/status-and-events.mdx`

大致內容：

- `PedelecSessionStatus` 的所有狀態。
- `idle`、`running`、`waiting_tool_result`、`ended`、`error` 的意義。
- 常見狀態流程：`idle → running → waiting_tool_result → running → idle`。
- `onStatus()`、`onError()`、`onEnded()`。
- `getStatus()` 的適用情境。
- 各 callback context 的共通欄位與事件專用欄位。
- `ctx.source` 為 `core` 或 `sdk` 時代表什麼。

### 3.7 Session Lifecycle and Resume

建議路徑：

- `sdk/session-lifecycle.mdx`

大致內容：

- page-scoped session 與 persistent session 的差異。
- `autoEndOnDisconnect: true` 的預設行為。
- 重新整理、關閉 tab、Extension disconnect 對 session 的影響。
- 需要跨頁或重新整理後繼續工作時，使用 `autoEndOnDisconnect: false`。
- 保存 `sessionId` 與 `resumeSession(sessionId)`。
- resume 後重新註冊 event/tool handlers。
- `end()` 的行為與 idempotent 特性。
- session 結束後不可再次 `sendText()`。

---

## 4. Tool Calling

### 4.1 Tool Calling Overview

建議路徑：

- `tools/overview.mdx`

大致內容：

- tool calling 適合用於哪些情境：讀取頁面狀態、修改 UI、取得使用者輸入、操作 canvas/editor。
- `skills.guidance` 與 `skills.tools` 的角色。
- Web App 宣告 tool、agent 發出 tool call、SDK 執行 handler、回傳結果的完整流程。
- tool handler 執行於 browser page，因此可以存取 DOM 與前端 state。
- Core 會產生 agent 所需的 tool artifacts，SDK 使用者不需要自行管理 `tools.md`。

### 4.2 Defining Tools

建議路徑：

- `tools/defining-tools.mdx`

大致內容：

- `defineTool()`。
- `name`、`description`、`argsSchema`、`timeoutMs`、`handler`。
- tool name 命名規則。
- description 如何寫得讓 agent 正確選擇 tool。
- inline handler。
- 使用 `as const`、`satisfies ToolArgsSchema` 與 TypeScript generic 保留 tool name 型別。
- 無參數 tool、單一參數 tool、巢狀物件 tool 範例。

### 4.3 Tool Args Schema

建議路徑：

- `tools/args-schema.mdx`

大致內容：

- Pedelec Tool Args Schema 是 JSON Schema 的子集合，不是完整 JSON Schema。
- root schema 必須為 object。
- 支援 `string`、`number`、`integer`、`boolean`、`array`、`object`、`oneOf`。
- 支援的 metadata 與 validation 欄位。
- `default` 只是給 agent 的 guidance，不會替缺少的參數自動補值。
- 不支援的 `$defs`、`$ref`、`additionalProperties`、`format` 等欄位。
- 如何用 TypeScript constants 重用 schema fragment。
- schema validation error 的常見原因。

### 4.4 Registering Tool Handlers

建議路徑：

- `tools/handlers.mdx`

大致內容：

- inline `handler`。
- `session.onTool(name, handler)` named handler。
- `session.onTool((tool, args, ctx) => ...)` generic fallback handler。
- handler 優先順序：named handler、inline handler、generic handler。
- unsubscribe function 與 component cleanup。
- tool result 必須是 JSON-serializable。
- sync 與 async handler。
- handler throw 與回傳 domain error object 的差異。

### 4.5 Interactive and Long-Running Tools

建議路徑：

- `tools/interactive-tools.mdx`

大致內容：

- tool handler 開啟 modal，等待使用者輸入後再 resolve。
- 適合使用 Promise 的 UI pattern。
- `waiting_tool_result` 狀態如何映射到 UI。
- `timeoutMs` 的用途。
- 使用者取消操作時應回傳的結果格式。
- 避免 handler 永遠不 resolve。
- 頁面切換或 component unmount 時如何清理 pending interaction。

### 4.6 Tool Context and UI Lifecycle Safety

建議路徑：

- `tools/ui-lifecycle-safety.mdx`

大致內容：

- `ToolCallContext` 的 `sessionId`、`toolRequestId`、`turnId`、時間欄位。
- SDK context 可以辨識事件來源，但無法知道目前 UI world/canvas/editor instance 是否仍有效。
- 如何使用 application-owned lifecycle ID 或 generation token 避免 stale tool call 修改新的 UI。
- 多 session、多 tab 或 SPA route 切換時的 handler 安全策略。

### 4.7 Tool Errors and Timeouts

建議路徑：

- `tools/errors-and-timeouts.mdx`

大致內容：

- `TOOL_HANDLER_NOT_FOUND`。
- `TOOL_HANDLER_ERROR`。
- `SUBMIT_TOOL_RESULT_FAILED`。
- tool timeout 的行為與除錯方向。
- JSON serialization 失敗的常見資料型別，例如 DOM node、function、cyclic object。
- 建議的 application-level error result 結構。

---

## 5. Framework and Application Guides

### 5.1 SolidJS Integration

建議路徑：

- `guides/solidjs.mdx`

大致內容：

- 在 browser-only lifecycle 建立 `Pedelec`。
- 使用 signals 保存 client、session、status 與串流訊息。
- 在 `onCleanup()` 解除 callback。
- 避免 reactive effect 重複建立 session。
- tool handler 如何讀寫 Solid signal。
- 從 `demo/solid-sdk-demo` 擷取簡化且可閱讀的整合範例。

### 5.2 React Integration

建議路徑：

- `guides/react.mdx`

大致內容：

- 使用 `useRef` 保存 client/session。
- 使用 `useEffect` 註冊與清理 event handler。
- 避免 Strict Mode 開發環境重複建立 session。
- 串流文字 state 更新方式。

此頁可在首版文件完成後再補。

### 5.3 Vanilla TypeScript Integration

建議路徑：

- `guides/vanilla-typescript.mdx`

大致內容：

- 最低框架依賴的完整範例。
- 綁定 connect、send、end 按鈕。
- 累積 transcript、顯示 status 與錯誤。
- 適合當作其他 framework 文件的共同基準。

### 5.4 SSR and SPA Considerations

建議路徑：

- `guides/ssr-and-spa.mdx`

大致內容：

- SDK 不可在 server render 階段建立。
- Astro、SolidStart、Next.js 等環境需在 client-only lifecycle 初始化。
- navigation 與 page refresh 對 session 的影響。
- 何時使用 `autoEndOnDisconnect: false`。
- hydrate 前後避免重複 client/session instance。

### 5.5 Production Checklist

建議路徑：

- `guides/production-checklist.mdx`

大致內容：

- 顯示 Extension/Desktop/provider unavailable 的明確 UI。
- origin approval UX。
- disable duplicate send。
- unsubscribe handlers。
- 結束不再使用的 session。
- 對 tool input 做 application-level validation。
- 不信任 agent 產生的 tool args。
- tool handler 只暴露必要能力。
- 處理 disconnect、timeout、stale UI lifecycle。
- 將 provider/model 選擇與使用者設定清楚呈現。

---

## 6. API Reference

API Reference 應保持簡潔、可搜尋，避免重複 guides 中的大量情境說明。

### 6.1 `Pedelec`

建議路徑：

- `reference/pedelec.mdx`

應記錄：

- constructor
- `createSession()`
- `listProviders()`
- `getSettings()`
- `getApprovalStatus()`
- `resumeSession()`
- `request<T>()`
- 每個方法的參數、回傳值、常見錯誤與短範例。
- `request<T>()` 標記為 advanced/low-level API。

### 6.2 `PedelecSession`

建議路徑：

- `reference/pedelec-session.mdx`

應記錄：

- `sessionId`
- `provider`
- `model`
- `prepare()`
- `sendText()`
- `onChat()`
- `onTool()` 的兩種 overload
- `onError()`
- `onStatus()`
- `onEnded()`
- `getStatus()`
- `end()`

### 6.3 Tools API

建議路徑：

- `reference/tools.mdx`

應記錄：

- `defineTool()`
- `ToolDefinition`
- `SkillsInput`
- `ToolSpecificHandler`
- `ToolNameOf`
- `SerializableToolManifest`
- `SerializableSkillsManifest`

### 6.4 Types

建議路徑：

- `reference/types.mdx`

應記錄：

- `PedelecOptions`
- `ProviderCode`
- `ProviderInfo`
- `PedelecSettings`
- `ApprovalStatus`
- `CreateSessionInput`
- `PedelecSessionStatus`
- `PedelecError`
- event context types
- Tool Args Schema types

### 6.5 Error Codes

建議路徑：

- `reference/error-codes.mdx`

大致內容：

- 依類別整理錯誤：environment、approval、transport、configuration、session、tool。
- 每個 code 的觸發條件。
- 使用者可採取的處理方式。
- 是否適合 retry。
- 是否應提示使用者開啟 Desktop App、安裝 Extension、設定 provider 或重新建立 session。

---

## 7. Troubleshooting and FAQ

### 7.1 Troubleshooting

建議路徑：

- `help/troubleshooting.mdx`

優先涵蓋：

- Extension 無法連線。
- Desktop App 未啟動。
- Native host unavailable。
- origin approval popup 沒有出現。
- provider 顯示 unavailable。
- Ollama available 但 session 仍失敗。
- `sendText()` 一直 pending。
- session 回傳 `SESSION_BUSY`。
- tool call 沒有觸發 handler。
- tool result 無法序列化。
- 重新整理後 session 消失。
- SSR 環境出現 `window` 或 extension unavailable 錯誤。

### 7.2 FAQ

建議路徑：

- `help/faq.mdx`

建議問題：

- Pedelec SDK 是否可以直接在 Node.js 使用？
- 是否一定需要 Chrome Extension？
- Web App 是否可以直接啟動 provider CLI？
- session 是否會在重新整理後保留？
- 是否可以同時建立多個 session？
- 同一 session 是否可以同時送出多個 prompt？
- tool handler 是否可以等待使用者操作？
- tool result 可以回傳哪些資料？
- provider 與 model 是由 Web App 還是 Desktop App 決定？
- Pedelec 是否會把前端資料傳到 Pedelec server？

---

# 首版建議範圍

首版不需要一次完成所有頁面。建議先完成以下頁面，讓使用者可以完整走通 SDK 的主要流程：

1. Overview
2. Requirements and Browser Support
3. Installation
4. Quick Start
5. Origin Approval and Connection State
6. Providers, Models, and Settings
7. Creating Sessions
8. Preparing and Sending a Turn
9. Streaming Responses
10. Session Status and Events
11. Session Lifecycle and Resume
12. Tool Calling Overview
13. Defining Tools
14. Tool Args Schema
15. Registering Tool Handlers
16. Tool Errors and Timeouts
17. SolidJS Integration
18. `Pedelec` API Reference
19. `PedelecSession` API Reference
20. Error Codes
21. Troubleshooting

下列頁面可排在第二階段：

- React Integration
- Vanilla TypeScript Integration
- SSR and SPA Considerations
- Interactive and Long-Running Tools
- Tool Context and UI Lifecycle Safety
- Production Checklist
- FAQ
- 完整 Types Reference

---

# 現有文件需要調整的地方

## `getting-started.mdx`

目前仍是 placeholder，且包含不存在的 `pedelec connect` 指令。正式撰寫時應拆分為 Requirements、Installation、Quick Start 與 Approval 四頁。

## `sdk-overview.mdx`

目前範例使用不存在的 `pedelec.connect()` 與 `session.send()`。應改為目前實際 API：

```ts
const pedelec = new Pedelec();
const session = await pedelec.createSession();
await session.sendText("Explain this project.");
```

現有 SolidJS counter 可以保留作為 hydration demo，但不應放在 SDK Overview 主線內容中；更適合移至 SolidJS integration 或獨立 interactive examples 頁面。

## `sdk/README.md`

現有 README 已包含大量可用內容，可作為官方文件的主要資料來源，但不建議直接整頁搬入 Starlight。應依上述架構拆頁，減少重複並補上：

- `prepare()`。
- 更完整的 browser/SSR integration 注意事項。
- 互動式 tool handler pattern。
- production checklist。
- 依類別整理的 error reference。

---

# 建議 Starlight Sidebar 分組

```txt
Introduction
  Overview
  How Pedelec Works

Getting Started
  Requirements
  Installation
  Quick Start
  Approval and Connection

SDK
  Client
  Providers and Models
  Creating Sessions
  Sending Messages
  Streaming Responses
  Status and Events
  Session Lifecycle

Tool Calling
  Overview
  Defining Tools
  Args Schema
  Handlers
  Interactive Tools
  UI Lifecycle Safety
  Errors and Timeouts

Guides
  SolidJS
  React
  Vanilla TypeScript
  SSR and SPA
  Production Checklist

API Reference
  Pedelec
  PedelecSession
  Tools
  Types
  Error Codes

Help
  Troubleshooting
  FAQ
```

# 完成標準

每一頁正式完成時應符合：

- 英文與繁體中文都有對應頁面。
- 程式碼使用目前 SDK 實際公開 API。
- 範例可通過 TypeScript type check，或明確標示為 pseudo-code。
- 頁面包含至少一個正常流程範例。
- 若該功能可能失敗，列出主要錯誤與使用者可採取的動作。
- 不把 internal protocol 當成 public API 承諾。
- 內部連結可在 GitHub Pages 的 `/pedelec` base path 下正常運作。
