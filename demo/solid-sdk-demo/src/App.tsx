import { createEffect, createMemo, createSignal, For, Show, type JSX } from "solid-js";
import { ProviderCode, Pedelec, defineTool, type ProviderInfo, type PedelecError, type PedelecSession, type PedelecSessionStatus, type ToolArgsSchema } from "pedelec";

const MAX_EVENTS = 300;
const MAX_ASSET_SIZE_BYTES = 100 * 1024 * 1024;

type ChatRole = "user" | "assistant" | "system";
type SessionStatus = PedelecSessionStatus | "unknown";
type ToolStatus = "pending" | "resolved" | "rejected";
type ConnectionValue = "initialized" | "failed" | "available" | "unavailable" | "connected" | "disconnected" | "unknown";

type DemoChatMessage = {
  id: string;
  sessionId: string;
  role: ChatRole;
  text: string;
  createdAt: number;
  updatedAt: number;
};

type DemoToolCall = {
  id: string;
  sessionId: string;
  tool: string;
  args: unknown;
  result?: unknown;
  error?: unknown;
  status: ToolStatus;
  createdAt: number;
  updatedAt: number;
};

type DemoError = {
  id: string;
  sessionId?: string;
  code?: string;
  message: string;
  details?: unknown;
  createdAt: number;
};

type DemoEvent = {
  id: string;
  sessionId?: string;
  type: string;
  payload?: unknown;
  createdAt: number;
};

type DemoUploadedAsset = {
  id: string;
  originalName: string;
  path: string;
  sizeBytes: number;
  mimeType: string;
  uploadedAt: number;
};

type DemoSessionState = {
  sessionId: string;
  provider: string;
  model?: string;
  status: SessionStatus;
  transcript: DemoChatMessage[];
  errors: DemoError[];
  toolCalls: DemoToolCall[];
  events: DemoEvent[];
  createdAt: number;
  updatedAt: number;
  sending: boolean;
  uploadedAssets: DemoUploadedAsset[];
  uploadingAsset: boolean;
  session: PedelecSession;
  dispose: () => void;
};

type AskUserState = {
  toolCallId: string;
  question: string;
  value: string;
  resolve: (value: string) => void;
};

type ConnectionState = {
  sdk: ConnectionValue;
  runtime: ConnectionValue;
  extension: ConnectionValue;
  message: string;
};

const emptyArgsSchema = {
  type: "object",
  properties: {},
  required: [],
} satisfies ToolArgsSchema;

function createDemoSkills() {
  return {
    guidance:
      "Use these tools when you need browser page context, selected text, or explicit input from the user. Do not guess page state.",
    tools: [
      defineTool({
        name: "get_current_page",
        description: "Read the current browser page title and URL.",
        argsSchema: emptyArgsSchema,
        timeoutMs: 60000,
        handler: () => ({
          title: document.title,
          url: location.href,
        }),
      }),
      defineTool({
        name: "get_selected_text",
        description: "Read the text currently selected on the page.",
        argsSchema: emptyArgsSchema,
        timeoutMs: 60000,
      }),
      defineTool({
        name: "ask_user",
        description: "Ask the user a question and wait for their text response.",
        argsSchema: {
          type: "object",
          properties: {
            question: { type: "string" },
          },
          required: ["question"],
        },
        timeoutMs: 60000,
      }),
      defineTool({
        name: "throw_error",
        description: "Trigger the demo error path by throwing an error from the tool handler.",
        argsSchema: emptyArgsSchema,
        timeoutMs: 60000,
      }),
    ],
  };
}

export default function App() {
  const [client, setClient] = createSignal<Pedelec | null>(null);
  const [connection, setConnection] = createSignal<ConnectionState>(initializeClient(setClient));
  const [provider, setProvider] = createSignal("");
  const [providers, setProviders] = createSignal<ProviderInfo[]>([]);
  const [providersLoading, setProvidersLoading] = createSignal(false);
  const [model, setModel] = createSignal("");
  const [resumeId, setResumeId] = createSignal("");
  const [prompt, setPrompt] = createSignal("");
  const [selectedAsset, setSelectedAsset] = createSignal<File | null>(null);
  const [sessions, setSessions] = createSignal<DemoSessionState[]>([]);
  const [activeSessionId, setActiveSessionId] = createSignal("");
  const [globalEvents, setGlobalEvents] = createSignal<DemoEvent[]>([]);
  const [globalErrors, setGlobalErrors] = createSignal<DemoError[]>([]);
  const [askUser, setAskUser] = createSignal<AskUserState | null>(null);
  let assetFileInput: HTMLInputElement | undefined;

  const activeSession = createMemo(() => sessions().find((session) => session.sessionId === activeSessionId()));
  const allErrors = createMemo(() => {
    const sessionErrors = activeSession()?.errors ?? [];
    return [...sessionErrors, ...globalErrors()].sort((a, b) => b.createdAt - a.createdAt);
  });
  const activeEvents = createMemo(() => activeSession()?.events ?? globalEvents());
  const canSend = createMemo(() => {
    const session = activeSession();
    return Boolean(session && prompt().trim() && session.status === "idle" && !session.sending && !session.uploadingAsset);
  });
  const canUploadAsset = createMemo(() => {
    const session = activeSession();
    const file = selectedAsset();
    return Boolean(
      session &&
        file &&
        file.size <= MAX_ASSET_SIZE_BYTES &&
        session.status === "idle" &&
        !session.uploadingAsset &&
        !session.sending,
    );
  });
  const canCreate = createMemo(() =>
    Boolean(client() && provider() && !providersLoading() && providers().length > 0),
  );

  function resetClient() {
    sessions().forEach((session) => session.dispose());
    setSessions([]);
    setActiveSessionId("");
    setGlobalErrors([]);
    setGlobalEvents([]);
    setProviders([]);
    setProvidersLoading(false);
    setProvider("");
    setSelectedAsset(null);
    clearAssetFileInput();
    setConnection(initializeClient(setClient));
  }

  createEffect(() => {
    activeSessionId();
    setSelectedAsset(null);
    clearAssetFileInput();
  });

  async function loadProviders() {
    const sdk = client();
    if (!sdk) return;
    setProvidersLoading(true);
    try {
      const list = await sdk.listProviders();
      const available = list.filter((p) => p.available);
      setProviders(available);
      setProvider((current) => {
        if (available.find((p) => p.code === current)) return current;
        return available[0]?.code ?? "";
      });
    } catch (err) {
      const error = toDemoError(err);
      recordError(error);
      setProviders([]);
      setProvider("");
    } finally {
      setProvidersLoading(false);
    }
  }

  createEffect(() => {
    if (client()) loadProviders();
  });

  async function createSession(event: SubmitEvent) {
    event.preventDefault();
    const sdk = client();
    if (!sdk) return;

    try {
      appendGlobalEvent("create_session_requested", { provider: provider(), model: model() });
      const session = await sdk.createSession({
        provider: provider() as ProviderCode,
        model: model().trim() || undefined,
        skills: createDemoSkills(),
      });
      registerSession(session, provider(), model().trim() || undefined);
      setConnection((current) => ({ ...current, extension: "connected", message: "Extension connected." }));
    } catch (err) {
      recordError(toDemoError(err));
      markExtensionError(err);
    }
  }

  async function resumeSession(event: SubmitEvent) {
    event.preventDefault();
    const sdk = client();
    const sessionId = resumeId().trim();
    if (!sdk || !sessionId) return;

    try {
      appendGlobalEvent("resume_session_requested", { sessionId });
      const session = await sdk.resumeSession(sessionId);
      registerSession(session, "resumed", undefined);
      setResumeId("");
      setConnection((current) => ({ ...current, extension: "connected", message: "Extension connected." }));
    } catch (err) {
      recordError(toDemoError(err, sessionId));
      markExtensionError(err);
    }
  }

  async function sendText(event: SubmitEvent) {
    event.preventDefault();
    const session = activeSession();
    const text = prompt().trim();
    if (!session || !text) return;

    if (session.status === "ended") {
      recordError(toDemoError({ code: "SESSION_ENDED", message: "session has ended" }, session.sessionId));
      return;
    }

    const message = makeMessage(session.sessionId, "user", text);
    updateSession(session.sessionId, (current) => ({
      ...current,
      sending: true,
      transcript: [...current.transcript, message],
      updatedAt: Date.now(),
    }));
    setPrompt("");
    appendSessionEvent(session.sessionId, "user_message_sent", { text });

    try {
      await session.session.sendText(text);
      appendSessionEvent(session.sessionId, "send_text_resolved", {});
    } catch (err) {
      recordError(toDemoError(err, session.sessionId), session.sessionId);
    } finally {
      updateSession(session.sessionId, (current) => ({ ...current, sending: false, updatedAt: Date.now() }));
    }
  }

  async function endSession(sessionId: string) {
    const state = sessions().find((session) => session.sessionId === sessionId);
    if (!state) return;

    try {
      appendSessionEvent(sessionId, "end_session_requested", {});
      await state.session.end();
      updateSession(sessionId, (current) => ({ ...current, status: "ended", sending: false, updatedAt: Date.now() }));
    } catch (err) {
      recordError(toDemoError(err, sessionId), sessionId);
    }
  }

  async function uploadAsset(event: SubmitEvent) {
    event.preventDefault();
    const state = activeSession();
    const file = selectedAsset();
    if (!state || !file) return;

    if (file.size > MAX_ASSET_SIZE_BYTES) {
      recordError(
        toDemoError(
          {
            code: "ASSET_TOO_LARGE",
            message: "File exceeds the 100 MiB limit.",
            details: { filename: file.name, sizeBytes: file.size, maxSizeBytes: MAX_ASSET_SIZE_BYTES },
          },
          state.sessionId,
        ),
        state.sessionId,
      );
      return;
    }

    if (state.status !== "idle" || state.sending || state.uploadingAsset) return;

    updateSession(state.sessionId, (current) => ({ ...current, uploadingAsset: true, updatedAt: Date.now() }));
    appendSessionEvent(state.sessionId, "asset_upload_requested", {
      filename: file.name,
      sizeBytes: file.size,
      mimeType: file.type,
    });

    try {
      const path = await state.session.uploadAsset(file);
      const uploadedAsset: DemoUploadedAsset = {
        id: newId("asset"),
        originalName: file.name,
        path,
        sizeBytes: file.size,
        mimeType: file.type || "application/octet-stream",
        uploadedAt: Date.now(),
      };
      updateSession(state.sessionId, (current) => ({
        ...current,
        uploadedAssets: [uploadedAsset, ...current.uploadedAssets],
        updatedAt: Date.now(),
      }));
      appendSessionEvent(state.sessionId, "asset_upload_resolved", {
        filename: file.name,
        path,
        sizeBytes: file.size,
        mimeType: file.type || "application/octet-stream",
      });
      setSelectedAsset(null);
      clearAssetFileInput();
    } catch (err) {
      recordError(toDemoError(err, state.sessionId), state.sessionId);
      appendSessionEvent(state.sessionId, "asset_upload_rejected", {
        filename: file.name,
        sizeBytes: file.size,
        mimeType: file.type || "application/octet-stream",
      });
    } finally {
      updateSession(state.sessionId, (current) => ({ ...current, uploadingAsset: false, updatedAt: Date.now() }));
    }
  }

  function registerSession(session: PedelecSession, fallbackProvider: string, fallbackModel?: string) {
    const existing = sessions().find((item) => item.sessionId === session.sessionId);
    if (existing) {
      setActiveSessionId(session.sessionId);
      appendSessionEvent(session.sessionId, "session_selected", {});
      return;
    }

    const disposeChat = session.onChat((text) => {
      appendAssistantDelta(session.sessionId, text);
      appendSessionEvent(session.sessionId, "assistant_delta_received", { text });
    });
    const disposeStatus = session.onStatus((status) => {
      updateSession(session.sessionId, (current) => ({ ...current, status, updatedAt: Date.now() }));
      appendSessionEvent(session.sessionId, "status_changed", { status });
    });
    const disposeError = session.onError((error) => {
      recordError(toDemoError(error, session.sessionId), session.sessionId);
    });
    const disposeEnded = session.onEnded(() => {
      updateSession(session.sessionId, (current) => ({ ...current, status: "ended", sending: false, updatedAt: Date.now() }));
      appendSessionEvent(session.sessionId, "session_ended", {});
    });
    const disposeSelectedTextTool = session.onTool("get_selected_text", (args) => handleTool(session.sessionId, "get_selected_text", args));
    const disposeTool = session.onTool((tool, args) => handleTool(session.sessionId, tool, args));
    const dispose = () => {
      disposeChat();
      disposeStatus();
      disposeError();
      disposeEnded();
      disposeSelectedTextTool();
      disposeTool();
    };
    const now = Date.now();
    const state: DemoSessionState = {
      sessionId: session.sessionId,
      provider: session.provider || fallbackProvider || "unknown",
      model: session.model || fallbackModel,
      status: session.getStatus(),
      transcript: [],
      errors: [],
      toolCalls: [],
      events: [],
      createdAt: now,
      updatedAt: now,
      sending: false,
      uploadedAssets: [],
      uploadingAsset: false,
      session,
      dispose,
    };

    setSessions((current) => [state, ...current]);
    setActiveSessionId(session.sessionId);
    appendGlobalEvent(fallbackProvider === "resumed" ? "session_resumed" : "session_created", {
      sessionId: session.sessionId,
      provider: state.provider,
      model: state.model,
    });
    appendSessionEvent(session.sessionId, "session_selected", {});
  }

  async function handleTool(sessionId: string, tool: string, args: unknown): Promise<unknown> {
    const id = newId("tool");
    const createdAt = Date.now();
    const call: DemoToolCall = {
      id,
      sessionId,
      tool,
      args,
      status: "pending",
      createdAt,
      updatedAt: createdAt,
    };
    updateSession(sessionId, (session) => ({
      ...session,
      toolCalls: [call, ...session.toolCalls],
      updatedAt: Date.now(),
    }));
    appendSessionEvent(sessionId, "tool_call_received", { tool, args });

    try {
      const result = await runDemoTool(id, tool, args);
      const isErrorResult = isToolErrorResult(result);
      updateToolCall(sessionId, id, {
        result,
        error: isErrorResult ? result.error : undefined,
        status: isErrorResult ? "rejected" : "resolved",
      });
      appendSessionEvent(sessionId, isErrorResult ? "tool_error_returned" : "tool_result_returned", { tool, result });
      return result;
    } catch (err) {
      const error = toPedelecError(err, "TOOL_HANDLER_ERROR", "Tool handler failed");
      updateToolCall(sessionId, id, { error, status: "rejected" });
      appendSessionEvent(sessionId, "tool_error_returned", { tool, error });
      throw err;
    }
  }

  async function runDemoTool(toolCallId: string, tool: string, args: unknown): Promise<unknown> {
    if (tool === "get_current_page") {
      return {
        title: document.title,
        url: location.href,
      };
    }

    if (tool === "get_selected_text") {
      return {
        text: globalThis.getSelection?.()?.toString() ?? "",
      };
    }

    if (tool === "ask_user") {
      return {
        answer: await openAskUser(toolCallId, getQuestion(args)),
      };
    }

    if (tool === "throw_error") {
      throw new Error("throw_error demo tool was called.");
    }

    return {
      error: {
        code: "TOOL_HANDLER_NOT_FOUND",
        message: `No demo handler registered for ${tool}`,
        details: { tool },
      },
    };
  }

  function openAskUser(toolCallId: string, question: string): Promise<string> {
    return new Promise((resolve) => {
      setAskUser({ toolCallId, question, value: "", resolve });
    });
  }

  function submitAskUser(event: SubmitEvent) {
    event.preventDefault();
    const state = askUser();
    if (!state) return;
    state.resolve(state.value);
    setAskUser(null);
  }

  function cancelAskUser() {
    const state = askUser();
    if (!state) return;
    state.resolve("");
    setAskUser(null);
  }

  function updateSession(sessionId: string, updater: (session: DemoSessionState) => DemoSessionState) {
    setSessions((current) => current.map((session) => (session.sessionId === sessionId ? updater(session) : session)));
  }

  function appendAssistantDelta(sessionId: string, text: string) {
    updateSession(sessionId, (session) => {
      const now = Date.now();
      const last = session.transcript.at(-1);
      const transcript =
        last?.role === "assistant"
          ? [
              ...session.transcript.slice(0, -1),
              {
                ...last,
                text: last.text + text,
                updatedAt: now,
              },
            ]
          : [...session.transcript, makeMessage(sessionId, "assistant", text)];
      return { ...session, transcript, updatedAt: now };
    });
  }

  function appendSessionEvent(sessionId: string, type: string, payload?: unknown) {
    updateSession(sessionId, (session) => ({
      ...session,
      events: [makeEvent(type, payload, sessionId), ...session.events].slice(0, MAX_EVENTS),
      updatedAt: Date.now(),
    }));
  }

  function appendGlobalEvent(type: string, payload?: unknown) {
    setGlobalEvents((current) => [makeEvent(type, payload), ...current].slice(0, MAX_EVENTS));
  }

  function updateToolCall(sessionId: string, id: string, patch: Partial<DemoToolCall>) {
    updateSession(sessionId, (session) => ({
      ...session,
      toolCalls: session.toolCalls.map((call) =>
        call.id === id ? { ...call, ...patch, updatedAt: Date.now() } : call,
      ),
      updatedAt: Date.now(),
    }));
  }

  function recordError(error: DemoError, sessionId?: string) {
    if (sessionId) {
      updateSession(sessionId, (session) => ({
        ...session,
        errors: [error, ...session.errors],
        updatedAt: Date.now(),
      }));
      appendSessionEvent(sessionId, "error_received", error);
      return;
    }

    setGlobalErrors((current) => [error, ...current]);
    appendGlobalEvent("error_received", error);
  }

  function selectSession(sessionId: string) {
    setActiveSessionId(sessionId);
    appendSessionEvent(sessionId, "session_selected", {});
  }

  function clearAssetFileInput() {
    if (assetFileInput) assetFileInput.value = "";
  }

  async function copyAssetPath(asset: DemoUploadedAsset) {
    const sessionId = activeSessionId() || undefined;
    try {
      await navigator.clipboard.writeText(asset.path);
    } catch (err) {
      recordError(
        toDemoError(
          {
            code: "CLIPBOARD_WRITE_FAILED",
            message: "Failed to copy asset path.",
            details: toPedelecError(err, "CLIPBOARD_WRITE_FAILED", "Failed to copy asset path.").details,
          },
          sessionId,
        ),
        sessionId,
      );
    }
  }

  function markExtensionError(err: unknown) {
    const error = toPedelecError(err, "SDK_ERROR", "SDK request failed");
    if (error.code.includes("EXTENSION")) {
      setConnection((current) => ({ ...current, extension: "unavailable", message: error.message }));
    }
  }

  return (
    <main class="app-shell">
      <header class="topbar">
        <div>
          <h1>Pedelec SDK Solid Demo</h1>
          <p>{activeSession()?.sessionId ?? "No active session"}</p>
        </div>
        <button type="button" onClick={resetClient}>
          Reinitialize SDK
        </button>
      </header>

      <section class="layout">
        <aside class="left-pane">
          <Panel title="Connection">
            <StatusGrid connection={connection()} />
          </Panel>

          <Panel title="Create Session">
            <form class="stack" onSubmit={createSession}>
              <label>
                Provider
                <Show
                  when={!providersLoading()}
                  fallback={<select disabled><option>Loading...</option></select>}
                >
                  <Show
                    when={providers().length > 0}
                    fallback={<select disabled><option>No providers available</option></select>}
                  >
                    <select value={provider()} onInput={(event) => setProvider(event.currentTarget.value)}>
                      <For each={providers()}>
                        {(p) => <option value={p.code}>{p.name}</option>}
                      </For>
                    </select>
                  </Show>
                </Show>
              </label>
              <label>
                Model
                <input value={model()} onInput={(event) => setModel(event.currentTarget.value)} placeholder="optional" />
              </label>
              <button type="submit" disabled={!canCreate()}>Create</button>
            </form>
          </Panel>

          <Panel title="Resume Session">
            <form class="stack" onSubmit={resumeSession}>
              <label>
                Session ID
                <input value={resumeId()} onInput={(event) => setResumeId(event.currentTarget.value)} />
              </label>
              <button type="submit" disabled={!resumeId().trim()}>
                Resume
              </button>
            </form>
          </Panel>

          <Panel title="Sessions">
            <div class="session-list">
              <Show when={sessions().length} fallback={<EmptyText text="No sessions yet." />}>
                <For each={sessions()}>
                  {(session) => (
                    <button
                      type="button"
                      class="session-row"
                      classList={{ active: activeSessionId() === session.sessionId }}
                      onClick={() => selectSession(session.sessionId)}
                    >
                      <span>{session.sessionId}</span>
                      <strong data-status={session.status}>{session.status}</strong>
                    </button>
                  )}
                </For>
              </Show>
            </div>
          </Panel>
        </aside>

        <section class="center-pane">
          <Panel title="Active Session">
            <Show when={activeSession()} fallback={<EmptyText text="Create or resume a session to begin." />}>
              {(session) => (
                <div class="session-detail">
                  <div class="detail-grid">
                    <Info label="Session ID" value={session().sessionId} />
                    <Info label="Provider" value={session().provider} />
                    <Info label="Model" value={session().model || "none"} />
                    <Info label="Status" value={session().status} />
                    <Info label="Created" value={formatTime(session().createdAt)} />
                    <Info label="Updated" value={formatTime(session().updatedAt)} />
                  </div>
                  <button
                    type="button"
                    class="danger"
                    disabled={session().status === "ended"}
                    onClick={() => endSession(session().sessionId)}
                  >
                    End Session
                  </button>
                </div>
              )}
            </Show>
          </Panel>

          <Panel title="Uploaded Assets">
            <AssetUploadBlock
              session={activeSession()}
              selectedFile={selectedAsset()}
              canUpload={canUploadAsset()}
              fileInputRef={(element) => (assetFileInput = element)}
              onFileChange={setSelectedAsset}
              onSubmit={uploadAsset}
              onCopyPath={copyAssetPath}
            />
          </Panel>

          <Panel title="Transcript">
            <div class="transcript">
              <Show when={activeSession()?.transcript.length} fallback={<EmptyText text="No messages yet." />}>
                <For each={activeSession()?.transcript ?? []}>
                  {(message) => (
                    <article class="message" data-role={message.role}>
                      <header>
                        <strong>{message.role}</strong>
                        <time>{formatTime(message.createdAt)}</time>
                      </header>
                      <pre>{message.text}</pre>
                    </article>
                  )}
                </For>
              </Show>
            </div>
          </Panel>

          <form class="send-bar" onSubmit={sendText}>
            <textarea
              rows="4"
              value={prompt()}
              disabled={!activeSession() || activeSession()?.status !== "idle" || activeSession()?.sending || activeSession()?.uploadingAsset}
              onInput={(event) => setPrompt(event.currentTarget.value)}
              placeholder="Send text to the active session"
            />
            <button type="submit" disabled={!canSend()}>
              Send
            </button>
          </form>

          <Panel title="Tool Calls">
            <Show when={activeSession()?.toolCalls.length} fallback={<EmptyText text="No tool calls yet." />}>
              <For each={activeSession()?.toolCalls ?? []}>
                {(toolCall) => (
                  <article class="tool-call" data-status={toolCall.status}>
                    <header>
                      <strong>{toolCall.tool}</strong>
                      <span>{toolCall.status}</span>
                    </header>
                    <JsonBlock label="Args" value={toolCall.args} />
                    <Show when={toolCall.result !== undefined}>
                      <JsonBlock label="Result" value={toolCall.result} />
                    </Show>
                    <Show when={toolCall.error !== undefined}>
                      <JsonBlock label="Error" value={toolCall.error} />
                    </Show>
                  </article>
                )}
              </For>
            </Show>
          </Panel>
        </section>

        <aside class="right-pane">
          <Panel title="Errors">
            <Show when={allErrors().length} fallback={<EmptyText text="No errors." />}>
              <For each={allErrors()}>
                {(error) => (
                  <article class="error-row">
                    <header>
                      <strong>{error.code || "ERROR"}</strong>
                      <time>{formatTime(error.createdAt)}</time>
                    </header>
                    <p>{error.message}</p>
                    <Show when={error.sessionId}>
                      <small>{error.sessionId}</small>
                    </Show>
                    <Show when={error.details !== undefined}>
                      <pre>{formatJson(error.details)}</pre>
                    </Show>
                  </article>
                )}
              </For>
            </Show>
          </Panel>

          <Panel title="Event Log">
            <Show when={activeEvents().length} fallback={<EmptyText text="No events yet." />}>
              <For each={activeEvents()}>
                {(event) => (
                  <article class="event-row">
                    <header>
                      <strong>{event.type}</strong>
                      <time>{formatTime(event.createdAt)}</time>
                    </header>
                    <Show when={event.sessionId}>
                      <small>{event.sessionId}</small>
                    </Show>
                    <Show when={event.payload !== undefined}>
                      <pre>{formatJson(event.payload)}</pre>
                    </Show>
                  </article>
                )}
              </For>
            </Show>
          </Panel>
        </aside>
      </section>

      <Show when={askUser()}>
        {(state) => (
          <div class="modal-backdrop">
            <form class="modal" onSubmit={submitAskUser}>
              <h2>ask_user</h2>
              <p>{state().question}</p>
              <textarea
                rows="5"
                value={state().value}
                onInput={(event) => setAskUser((current) => (current ? { ...current, value: event.currentTarget.value } : current))}
                autofocus
              />
              <div class="modal-actions">
                <button type="button" class="secondary" onClick={cancelAskUser}>
                  Cancel
                </button>
                <button type="submit">Return Result</button>
              </div>
            </form>
          </div>
        )}
      </Show>
    </main>
  );
}

function initializeClient(setClient: (client: Pedelec | null) => void): ConnectionState {
  const runtimeAvailable =
    typeof window !== "undefined" &&
    typeof (globalThis as { chrome?: { runtime?: { connect?: unknown } } }).chrome?.runtime?.connect === "function";
  try {
    const sdk = new Pedelec();
    setClient(sdk);
    return {
      sdk: "initialized",
      runtime: runtimeAvailable ? "available" : "unavailable",
      extension: runtimeAvailable ? "unknown" : "unavailable",
      message: runtimeAvailable
        ? "SDK initialized. The extension connection is verified on first request."
        : "Pedelec SDK must run in a Chrome page where extension runtime messaging is available.",
    };
  } catch (err) {
    setClient(null);
    return {
      sdk: "failed",
      runtime: runtimeAvailable ? "available" : "unavailable",
      extension: "unavailable",
      message: toPedelecError(err, "SDK_INIT_FAILED", "SDK initialization failed").message,
    };
  }
}

function StatusGrid(props: { connection: ConnectionState }) {
  return (
    <div class="status-grid">
      <Info label="SDK" value={props.connection.sdk} />
      <Info label="Runtime" value={props.connection.runtime} />
      <Info label="Extension" value={props.connection.extension} />
      <p class="diagnostic">{props.connection.message}</p>
    </div>
  );
}

function Panel(props: { title: string; children: JSX.Element }) {
  return (
    <section class="panel">
      <h2>{props.title}</h2>
      {props.children}
    </section>
  );
}

function Info(props: { label: string; value: string }) {
  return (
    <div class="info">
      <span>{props.label}</span>
      <strong>{props.value}</strong>
    </div>
  );
}

function JsonBlock(props: { label: string; value: unknown }) {
  return (
    <div class="json-block">
      <span>{props.label}</span>
      <pre>{formatJson(props.value)}</pre>
    </div>
  );
}

function EmptyText(props: { text: string }) {
  return <p class="empty">{props.text}</p>;
}

function AssetUploadBlock(props: {
  session: DemoSessionState | undefined;
  selectedFile: File | null;
  canUpload: boolean;
  fileInputRef: (element: HTMLInputElement) => void;
  onFileChange: (file: File | null) => void;
  onSubmit: (event: SubmitEvent) => void;
  onCopyPath: (asset: DemoUploadedAsset) => void;
}) {
  return (
    <Show when={props.session} fallback={<EmptyText text="Create or resume a session before uploading assets." />}>
      {(session) => {
        const fileInputDisabled = () => session().status !== "idle" || session().uploadingAsset || session().sending;
        const hint = () => {
          if (session().uploadingAsset) return "Uploading asset...";
          if (session().status !== "idle") return "Assets can only be uploaded while the session is idle.";
          if (props.selectedFile && props.selectedFile.size > MAX_ASSET_SIZE_BYTES) return "File exceeds the 100 MiB limit.";
          return `Single files up to 100 MiB can be uploaded to this session's sandbox.`;
        };

        return (
          <div class="asset-upload-block">
            <form class="stack" onSubmit={props.onSubmit}>
              <label>
                Choose file
                <input
                  ref={props.fileInputRef}
                  type="file"
                  disabled={fileInputDisabled()}
                  onChange={(event) => props.onFileChange(event.currentTarget.files?.[0] ?? null)}
                />
              </label>
              <Show when={props.selectedFile}>
                {(file) => <p class="asset-selection">{file().name} · {formatBytes(file().size)}</p>}
              </Show>
              <button type="submit" disabled={!props.canUpload}>
                {session().uploadingAsset ? "Uploading..." : "Upload Asset"}
              </button>
            </form>
            <p class="asset-hint">{hint()}</p>
            <div class="asset-list">
              <Show when={session().uploadedAssets.length} fallback={<EmptyText text="No assets uploaded in this page session." />}>
                <For each={session().uploadedAssets}>
                  {(asset) => <UploadedAssetRow asset={asset} onCopy={() => props.onCopyPath(asset)} />}
                </For>
              </Show>
            </div>
          </div>
        );
      }}
    </Show>
  );
}

function UploadedAssetRow(props: { asset: DemoUploadedAsset; onCopy: () => void }) {
  return (
    <article class="asset-row">
      <header>
        <strong>{props.asset.originalName}</strong>
        <time>{formatTime(props.asset.uploadedAt)}</time>
      </header>
      <div class="asset-path">
        <code>{props.asset.path}</code>
        <button type="button" onClick={props.onCopy}>Copy</button>
      </div>
      <small>{formatBytes(props.asset.sizeBytes)} · {props.asset.mimeType}</small>
    </article>
  );
}

function makeMessage(sessionId: string, role: ChatRole, text: string): DemoChatMessage {
  const now = Date.now();
  return {
    id: newId("msg"),
    sessionId,
    role,
    text,
    createdAt: now,
    updatedAt: now,
  };
}

function makeEvent(type: string, payload?: unknown, sessionId?: string): DemoEvent {
  return {
    id: newId("event"),
    sessionId,
    type,
    payload,
    createdAt: Date.now(),
  };
}

function toDemoError(err: unknown, sessionId?: string): DemoError {
  const error = toPedelecError(err, "SDK_ERROR", "SDK request failed");
  return {
    id: newId("err"),
    sessionId,
    code: error.code,
    message: error.message,
    details: error.details,
    createdAt: Date.now(),
  };
}

function toPedelecError(err: unknown, fallbackCode: string, fallbackMessage: string): PedelecError {
  if (!err) return { code: fallbackCode, message: fallbackMessage };
  if (typeof err === "string") return { code: fallbackCode, message: err };
  if (err instanceof Error) return { code: fallbackCode, message: err.message || fallbackMessage };
  const value = err as Partial<PedelecError>;
  if (typeof value.code === "string" && typeof value.message === "string") {
    return {
      code: value.code,
      message: value.message,
      details: value.details,
    };
  }
  return { code: fallbackCode, message: fallbackMessage, details: err };
}

function isToolErrorResult(value: unknown): value is { error: unknown } {
  return Boolean(value && typeof value === "object" && "error" in value);
}

function getQuestion(args: unknown): string {
  if (args && typeof args === "object" && "question" in args) {
    const question = (args as { question?: unknown }).question;
    if (typeof question === "string" && question.trim()) {
      return question;
    }
  }
  return "Enter a response for the agent.";
}

function formatJson(value: unknown): string {
  return JSON.stringify(value, null, 2);
}

function formatTime(value: number): string {
  return new Date(value).toLocaleTimeString();
}

function formatBytes(bytes: number): string {
  if (bytes === 0) return "0 B";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB"];
  const index = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)) - 1, units.length - 1);
  const value = bytes / 1024 ** (index + 1);
  return `${Number(value.toFixed(1))} ${units[index]}`;
}

function newId(prefix: string): string {
  return `${prefix}_${Date.now()}_${Math.random().toString(36).slice(2, 8)}`;
}
