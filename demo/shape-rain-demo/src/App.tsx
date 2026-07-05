import { createEffect, createMemo, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { marked } from "marked";
import {
  Pedelec,
  type CreateSessionInput,
  type PedelecError,
  type PedelecSession,
  type PedelecSessionStatus,
  type ProviderCode,
} from "pedelec";
import {
  normalizeSpawnClosedPolygonsCommand,
  normalizeSpawnCommand,
  type SpawnBasicShapesResult,
  type SpawnClosedPolygonsResult,
} from "./commands";
import { createShapeWorld } from "./shapeWorldFactory";
import type { RenderMode, ShapeWorldLike } from "./shapeWorldTypes";
import { IoChevronDown, IoChevronUp, IoClose, IoSettingsOutline, IoTerminalOutline } from "solid-icons/io";
import { usePopUp } from "./services/PopUpProvider";
import SettingPop, { type PedelecProviderSettings, type ShapeRainSessionSettings } from "./SettingPop/SettingPop";

type UiState = "ready" | "connecting" | "switching" | "submitting" | "generating" | "error" | "disconnected";

type RuntimeStatus = {
  label: string;
  detail: string;
};

const EXAMPLE_PROMPTS = "Try: pink triangle, five blue circles, yellow stars";
const SHAPE_RAIN_SESSION_SETTINGS_KEY = "shape-rain:pedelec-session-settings";
const DEFAULT_SESSION_SETTINGS: ShapeRainSessionSettings = { provider: "default", model: "" };
type ShapeToolResult = SpawnBasicShapesResult | SpawnClosedPolygonsResult;
type ToolCallErrorResult = { error: { code: string; message: string; details?: unknown } };
type ToolCallResult = ShapeToolResult | ToolCallErrorResult;

type ChatRole = "assistant" | "user" | "error" | "system";

type TextChatMessage = {
  role: Exclude<ChatRole, "system">;
  text: string;
};

type SystemChatMessage = {
  role: "system";
  id: number;
  tool: string;
  spawned: number;
  detailLines: string[];
  error?: { code: string; message: string };
  expanded: boolean;
};

type ChatMessage = TextChatMessage | SystemChatMessage;

export default function App() {
  const { pop } = usePopUp();
  let stageElement: HTMLDivElement | undefined;
  let panelMessageEl: HTMLDivElement | undefined;
  let sessionDisposer: (() => void) | undefined;
  let world: ShapeWorldLike | null = null;
  let lifecycleId = 0;
  let nextToolMessageId = 1;

  const [prompt, setPrompt] = createSignal("");
  const [renderMode, setRenderMode] = createSignal<RenderMode>("3d");
  const [uiState, setUiState] = createSignal<UiState>("connecting");
  const [session, setSession] = createSignal<PedelecSession | null>(null);
  const [sessionStatus, setSessionStatus] = createSignal<PedelecSessionStatus | "none">("none");
  const [message, setMessage] = createSignal("Connecting to Pedelec...");
  const [lastToolResult, setLastToolResult] = createSignal<ShapeToolResult | null>(null);
  const [chatPreview, setChatPreview] = createSignal("");
  const [sessionSettings, setSessionSettings] = createSignal<ShapeRainSessionSettings>(readStoredSessionSettings());
  const [conversationOpen, setConversationOpen] = createSignal(false);
  const [conversation, setConversation] = createSignal<ChatMessage[]>([]);

  const busy = createMemo(() => {
    const status = sessionStatus();
    return (
      uiState() === "connecting" ||
      uiState() === "switching" ||
      uiState() === "submitting" ||
      status === "running" ||
      status === "waiting_tool_result"
    );
  });

  const canSubmit = createMemo(() => Boolean(prompt().trim()) && !busy() && sessionStatus() !== "ended" && sessionStatus() !== "error");

  const runtimeStatus = createMemo<RuntimeStatus>(() => {
    const current = uiState();
    if (current === "ready") return { label: "Ready", detail: "Pedelec connected" };
    if (current === "connecting") return { label: "Connecting", detail: "Checking Extension and Desktop App" };
    if (current === "switching") return { label: "Switching", detail: `Starting ${renderMode().toUpperCase()} mode` };
    if (current === "submitting") return { label: "Submitting", detail: "Agent is reading your request" };
    if (current === "generating") return { label: "Generating", detail: "Tool command received" };
    if (current === "disconnected") return { label: "Disconnected", detail: "Pedelec is unavailable" };
    return { label: "Error", detail: "Action needed" };
  });

  onMount(() => {
    void initializeRuntime(renderMode(), ++lifecycleId);
  });

  onCleanup(() => {
    lifecycleId += 1;
    sessionDisposer?.();
    const activeSession = session();
    if (activeSession && activeSession.getStatus() !== "ended") void activeSession.end().catch(() => undefined);
    world?.destroy();
    world = null;
  });

  createEffect(() => {
    conversation()
    panelMessageEl?.scrollTo(0, panelMessageEl.scrollHeight)
  })

  async function initializeRuntime(mode: RenderMode, generation: number): Promise<void> {
    if (!stageElement) return;
    clearWorldUiState();
    setUiState(generation === 1 ? "connecting" : "switching");
    setMessage(generation === 1 ? "Connecting to Pedelec..." : `Switching to ${mode.toUpperCase()} mode...`);
    const nextWorld = createShapeWorld(mode);
    world = nextWorld;
    try {
      await nextWorld.mount(stageElement);
      if (generation !== lifecycleId) {
        nextWorld.destroy();
        return;
      }
      await connectPedelec(generation);
    } catch (err) {
      if (generation !== lifecycleId) return;
      nextWorld.destroy();
      world = null;
      appendConversationMessage("error", err instanceof Error ? err.message : `Could not start ${mode.toUpperCase()} mode.`);
      setUiState("error");
      setMessage(err instanceof Error ? err.message : `Could not start ${mode.toUpperCase()} mode.`);
    }
  }

  async function resetCurrentSession(): Promise<void> {
    sessionDisposer?.();
    sessionDisposer = undefined;
    const activeSession = session();
    setSession(null);
    setSessionStatus("none");
    if (activeSession && activeSession.getStatus() !== "ended") {
      await activeSession.end().catch(() => undefined);
    }
  }

  async function connectPedelec(generation = lifecycleId): Promise<void> {
    await resetCurrentSession();
    if (generation !== lifecycleId) return;
    setUiState("connecting");
    setMessage("Connecting to Pedelec...");
    setChatPreview("");

    try {
      const client = new Pedelec();
      const approval = await client.getApprovalStatus();
      if (!approval.installed) {
        appendConversationMessage("error", "Pedelec Extension is unavailable. Open this page in Chrome with the extension installed.");
        setUiState("disconnected");
        setMessage("Pedelec Extension is unavailable. Open this page in Chrome with the extension installed.");
        return;
      }
      if (!approval.approved) {
        setMessage("Approve this site in the Pedelec Extension popup, then connect again.");
      }

      const nextSession = await client.createSession({
        ...createSessionSettingsInput(sessionSettings()),
      });
      if (generation !== lifecycleId) {
        await nextSession.end().catch(() => undefined);
        return;
      }

      registerSession(nextSession, generation);
      setUiState("ready");
      setMessage("Ready. Describe the shapes you want to drop.");
    } catch (err) {
      if (generation !== lifecycleId) return;
      const friendly = friendlyPedelecError(err);
      appendConversationMessage("error", friendly.message);
      setUiState(friendly.disconnected ? "disconnected" : "error");
      setMessage(friendly.message);
    }
  }

  function registerSession(nextSession: PedelecSession, generation: number): void {
    sessionDisposer?.();
    const disposeStatus = nextSession.onStatus((status) => {
      if (generation !== lifecycleId) return;
      setSessionStatus(status);
      if (status === "idle") {
        setUiState("ready");
        setMessage("Ready. Describe the shapes you want to drop.");
      } else if (status === "running") {
        setUiState("submitting");
        setMessage("Pedelec is interpreting your request.");
      } else if (status === "waiting_tool_result") {
        setUiState("generating");
        setMessage("Generating shapes from the agent command.");
      } else if (status === "ended") {
        setUiState("disconnected");
        setMessage("This Pedelec session ended. Connect again to start a new one.");
      } else if (status === "error") {
        const statusMessage = "The Pedelec session reported an error. Connect again if it does not recover.";
        appendConversationMessage("error", statusMessage);
        setUiState("error");
        setMessage(statusMessage);
      }
    });
    const disposeError = nextSession.onError((error) => {
      if (generation !== lifecycleId) return;
      const friendly = friendlyPedelecError(error);
      appendConversationMessage("error", friendly.message);
      setUiState(friendly.disconnected ? "disconnected" : "error");
      setMessage(friendly.message);
    });
    const disposeEnded = nextSession.onEnded(() => {
      if (generation !== lifecycleId) return;
      setSessionStatus("ended");
      setUiState("disconnected");
      setMessage("This Pedelec session ended. Connect again to start a new one.");
    });
    const disposeChat = nextSession.onChat((text) => {
      if (generation !== lifecycleId) return;
      setChatPreview((current) => (current + text).slice(-180));
      appendConversationMessage("assistant", text);
    });
    const disposeTool = nextSession.onTool((tool, args) => handleTool(tool, args, generation));
    sessionDisposer = () => {
      disposeStatus();
      disposeError();
      disposeEnded();
      disposeChat();
      disposeTool();
    };
    setSession(nextSession);
    setSessionStatus(nextSession.getStatus());
  }

  async function reconnectPedelec(): Promise<void> {
    const generation = ++lifecycleId;
    clearWorldUiState();
    await connectPedelec(generation);
  }

  async function switchRenderMode(): Promise<void> {
    if (busy()) return;
    const nextMode: RenderMode = renderMode() === "2d" ? "3d" : "2d";
    const generation = ++lifecycleId;
    setRenderMode(nextMode);
    setUiState("switching");
    setMessage(`Switching to ${nextMode.toUpperCase()} mode...`);
    await resetCurrentSession();
    world?.destroy();
    world = null;
    stageElement?.replaceChildren();
    await initializeRuntime(nextMode, generation);
  }

  function clearWorldUiState(): void {
    setLastToolResult(null);
    setChatPreview("");
    setConversation([]);
  }

  function appendConversationMessage(role: TextChatMessage["role"], text: string): void {
    setConversation((current) => [...current, { role, text }]);
  }

  function appendToolConversationMessage(tool: string, result: ToolCallResult): void {
    const error = getToolResultError(result);
    setConversation((current) => [
      ...current,
      {
        role: "system",
        id: nextToolMessageId++,
        tool,
        spawned: getToolResultSpawned(result),
        detailLines: buildToolDetailLines(tool, result),
        ...(error ? { error } : {}),
        expanded: false,
      },
    ]);
  }

  function toggleToolMessage(id: number): void {
    setConversation((current) =>
      current.map((message) => (message.role === "system" && message.id === id ? { ...message, expanded: !message.expanded } : message)),
    );
  }

  async function submitPrompt(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const activeSession = session();
    const text = prompt().trim();
    if (!text) return;

    if (!activeSession || sessionStatus() === "none") {
      setUiState("disconnected");
      setMessage("Pedelec is not connected yet. Connect Pedelec before sending a prompt.");
      return;
    }

    if (busy()) {
      setMessage("The model is still handling the previous request. Try again in a moment.");
      return;
    }

    setPrompt("");
    setUiState("submitting");
    setMessage("Pedelec is interpreting your request.");
    appendConversationMessage("user", text);
    setConversationOpen(true);

    try {
      await activeSession.sendText(text);
    } catch (err) {
      const friendly = friendlyPedelecError(err);
      appendConversationMessage("error", friendly.message);
      setUiState(friendly.disconnected ? "disconnected" : "error");
      setMessage(friendly.message);
    }
  }

  function handleTool(
    tool: string,
    args: unknown,
    generation = lifecycleId,
  ): ToolCallResult {
    if (generation !== lifecycleId || !world) {
      const staleResult = {
        error: {
          code: "STALE_TOOL_CALL",
          message: "This tool call belongs to an older render session.",
        },
      };
      appendToolConversationMessage(tool, staleResult);
      return staleResult;
    }
    setUiState("generating");
    if (tool !== "spawn_basic_shapes" && tool !== "spawn_closed_polygons") {
      const unsupportedResult = {
        error: {
          code: "TOOL_HANDLER_NOT_FOUND",
          message: `Shape Rain does not support the frontend tool "${tool}".`,
          details: { tool },
        },
      };
      appendToolConversationMessage(tool, unsupportedResult);
      return unsupportedResult;
    }

    const result =
      tool === "spawn_basic_shapes"
        ? normalizeSpawnCommand(args, renderMode())
        : normalizeSpawnClosedPolygonsCommand(args, renderMode());
    if (result.normalizedItems.length > 0) {
      const spawned = world.spawn(result.normalizedItems);
      const finalResult = { ...result, spawned, success: spawned > 0 };
      setLastToolResult(finalResult);
      appendToolConversationMessage(tool, finalResult);
      setMessage(spawned > 0 ? `Dropped ${spawned} shape${spawned === 1 ? "" : "s"}.` : "The command was valid, but no shapes could be spawned.");
      return finalResult;
    }

    setLastToolResult(result);
    appendToolConversationMessage(tool, result);
    setUiState("error");
    setMessage(result.error?.message ?? "The shape command did not include a supported item.");
    return result;
  }

  function spawnDemoBatch(): void {
    if (!world || busy()) return;
    const result = normalizeSpawnCommand(
      {
        items: [
          { shape: "circle", count: 3, size: "small" },
          { shape: "triangle", count: 2, size: 18 },
          { shape: "square", count: 2, color: "blue", size: 18 },
          { shape: "star", count: 1, color: "pink", size: 18 },
        ],
      },
      renderMode(),
    );
    const spawned = world.spawn(result.normalizedItems);
    setLastToolResult({ ...result, spawned, success: spawned > 0 });
    setMessage(spawned > 0 ? `Dropped ${spawned} demo shapes.` : "Demo shapes could not be spawned yet.");
  }

  function clearShapes(): void {
    world?.clearObjects();
    setLastToolResult(null);
    setMessage(`${renderMode().toUpperCase()} canvas cleared.`);
  }

  async function loadProviderSettings(): Promise<PedelecProviderSettings> {
    const client = new Pedelec();
    const approval = await client.getApprovalStatus();
    if (!approval.installed) {
      throw {
        code: "EXTENSION_UNAVAILABLE",
        message: "Pedelec Extension is unavailable. Open this page in Chrome with the extension installed.",
      } satisfies PedelecError;
    }

    const [providers, settings] = await Promise.all([client.listProviders(), client.getSettings()]);
    return { providers, settings };
  }

  function applySessionSettings(nextSettings: ShapeRainSessionSettings): void {
    if (busy()) {
      setMessage("Pedelec is busy. Try changing provider settings again in a moment.");
      return;
    }
    const normalized = normalizeSessionSettings(nextSettings);
    setSessionSettings(normalized);
    writeStoredSessionSettings(normalized);
    void reconnectPedelec();
  }

  function openSettings(): void {
    if (busy()) {
      setMessage("Pedelec is busy. Try changing provider settings again in a moment.");
      return;
    }
    pop(SettingPop, {
      value: sessionSettings(),
      loadProviderSettings,
      onApply: applySessionSettings,
    });
  }

  return (
    <main class="app-shell" classList={{ "panel-open": conversationOpen() }}>
      <div ref={stageElement} class="stage-layer" aria-hidden="true" />

      <header class="topbar">
        <div class="brand">
          <h1>Shape Rain</h1>
        </div>
        <nav class="toolbar" aria-label="Shape Rain tools">
          <button type="button" title="Switch render mode" disabled={busy()} onClick={() => void switchRenderMode()}>
            {renderMode().toUpperCase()}
          </button>
          <button type="button" title="Reconnect Pedelec" disabled={uiState() === "switching"} onClick={() => void reconnectPedelec()}>
            ↻
          </button>
          <button type="button" title="Drop demo shapes" disabled={busy()} onClick={spawnDemoBatch}>
            ◇
          </button>
          <button type="button" title="Clear shapes" disabled={uiState() === "switching"} onClick={clearShapes}>
            ⌫
          </button>
        </nav>
      </header>

      <section class="interaction-layer" aria-label="Shape Rain prompt">
        <form class="prompt-card" onSubmit={submitPrompt}>
          <input
            value={prompt()}
            disabled={uiState() === "connecting" || uiState() === "switching" || sessionStatus() === "ended"}
            onInput={(event) => setPrompt(event.currentTarget.value)}
            placeholder="Drop a pink triangle"
            aria-label="Describe shapes to drop"
          />
          <button type="submit" disabled={!canSubmit()} title="Send to Pedelec">
            →
          </button>
        </form>

        <p class="examples">{EXAMPLE_PROMPTS}</p>

        <div class="status-row">
          <div class="status-line" data-state={uiState()}>
            <strong>{runtimeStatus().label}</strong>
            <span>{runtimeStatus().detail}</span>
          </div>
          <button type="button" class="settings-btn" title="Provider settings" onClick={openSettings}>
            <span class="settings-btn-label">
              {sessionSettingsLabel(sessionSettings())}
            </span>
            <IoSettingsOutline />
          </button>
        </div>

        <p class="message-line">{message()}</p>

        <button
          type="button"
          class="view-more-pill"
          title="View conversation"
          aria-expanded={conversationOpen()}
          onClick={() => setConversationOpen(true)}
        >
          ...
        </button>

        <Show when={lastToolResult()}>
          {(result) => (
            <p class="tool-summary">
              Last command: {result().spawned} spawned
              <Show when={result().ignored.length > 0}> · {result().ignored.length} ignored</Show>
            </p>
          )}
        </Show>

        <Show when={chatPreview()}>
          <p class="agent-preview">{chatPreview()}</p>
        </Show>
      </section>

      <aside class="chat-panel" classList={{ open: conversationOpen() }} aria-label="Conversation" aria-hidden={!conversationOpen()}>
        <header class="chat-panel-header">
          <h2>Conversation</h2>
          <button type="button" class="chat-panel-close" title="Close conversation" onClick={() => setConversationOpen(false)}>
            <IoClose size={18} />
          </button>
        </header>
        <div class="chat-panel-messages" ref={panelMessageEl}>
          <Show when={conversation().length > 0} fallback={<p class="chat-empty-state">No conversation yet.</p>}>
            <For each={conversation()}>
              {(chatMessage) => (
                <div class="chat-row" data-role={chatMessage.role}>
                  {chatMessage.role === "system" ? (
                    <div class="chat-tool-bubble" data-collapsed={chatMessage.expanded ? "false" : "true"} onClick={() => toggleToolMessage(chatMessage.id)}>
                      {chatMessage.expanded ? (
                        <>
                          <div class="chat-tool-header">
                            <div class="chat-tool-header-left">
                              <span class="chat-tool-icon">
                                <IoTerminalOutline size={14} />
                              </span>
                              <span class="chat-tool-text">{toolSummaryText(chatMessage)}</span>
                            </div>
                            <span class="chat-tool-chevron">
                              <IoChevronUp size={14} />
                            </span>
                          </div>
                          <div class="chat-tool-divider" />
                          <div class="chat-tool-detail">
                            <p class="chat-tool-detail-line chat-tool-detail-title">{chatMessage.tool}</p>
                            <For each={chatMessage.detailLines}>
                              {(line) => <p class="chat-tool-detail-line">{line}</p>}
                            </For>
                          </div>
                        </>
                      ) : (
                        <>
                          <span class="chat-tool-icon">
                            <IoTerminalOutline size={14} />
                          </span>
                          <span class="chat-tool-text">{toolSummaryText(chatMessage)}</span>
                          <span class="chat-tool-chevron">
                            <IoChevronDown size={14} />
                          </span>
                        </>
                      )}
                    </div>
                  ) : (
                    <div class="chat-bubble" innerHTML={renderChatMessageHtml(chatMessage.text)} />
                  )}
                </div>
              )}
            </For>
          </Show>
        </div>
      </aside>

      <footer class="hint-line">Enter sends the prompt to Pedelec. The frontend only executes validated shape tool commands.</footer>
    </main>
  );
}

function toolSummaryText(message: SystemChatMessage): string {
  return `Tool call: ${message.tool} (${message.spawned} spawned)`;
}

function renderChatMessageHtml(text: string): string {
  return marked.parse(text, { async: false });
}

function buildToolDetailLines(tool: string, result: ToolCallResult): string[] {
  const lines: string[] = [];
  const error = getToolResultError(result);
  if (error) {
    lines.push(`Error: ${error.code}`);
    lines.push(error.message);
  }

  if ("normalizedItems" in result) {
    result.normalizedItems.forEach((item, index) => {
      if (tool === "spawn_closed_polygons" && "kind" in item) {
        const label = item.preset ?? item.name ?? "custom polygon";
        lines.push(`${index + 1}. ${label} · ${item.color} · x${item.count}`);
        return;
      }

      if ("shape" in item) {
        lines.push(`${index + 1}. ${item.shape} · ${item.color} · x${item.count}`);
      }
    });
  }

  const ignored = getToolResultIgnored(result);
  if (ignored.length > 0) {
    lines.push(`Ignored: ${ignored.length}`);
    ignored.forEach((item) => {
      lines.push(`- item ${item.index}: ${item.reason}`);
    });
  }

  return lines.length > 0 ? lines : ["No normalized items."];
}

function getToolResultSpawned(result: ToolCallResult): number {
  return "spawned" in result ? result.spawned : 0;
}

function getToolResultIgnored(result: ToolCallResult): Array<{ index: number; reason: string; value?: unknown }> {
  return "ignored" in result ? result.ignored : [];
}

function getToolResultError(result: ToolCallResult): { code: string; message: string } | undefined {
  return "error" in result && result.error ? { code: result.error.code, message: result.error.message } : undefined;
}

function friendlyPedelecError(err: unknown): { message: string; disconnected: boolean } {
  const error = toPedelecError(err);
  const code = error.code;
  if (code.includes("EXTENSION")) {
    return {
      disconnected: true,
      message: "Pedelec Extension is unavailable. Confirm the Chrome Extension is installed and this page is approved.",
    };
  }
  if (code.includes("APPROVAL")) {
    return {
      disconnected: true,
      message: "This page is not approved yet. Open the Pedelec Extension popup and approve this origin.",
    };
  }
  if (code.includes("NATIVE") || code.includes("IPC") || code.includes("DESKTOP")) {
    return {
      disconnected: true,
      message: "Pedelec Desktop App is not reachable. Start the Desktop App and confirm Native Messaging is registered.",
    };
  }
  if (code === "DEFAULT_PROVIDER_NOT_SET" || code === "MODEL_REQUIRED") {
    return {
      disconnected: false,
      message: "Pedelec needs a default provider and model. Open Desktop App Settings and configure them.",
    };
  }
  if (code.includes("PROVIDER")) {
    return {
      disconnected: false,
      message: "The selected Pedelec provider is unavailable. Check Desktop App provider settings.",
    };
  }
  if (code === "SESSION_BUSY") {
    return {
      disconnected: false,
      message: "The model is still handling the previous request. Try again in a moment.",
    };
  }
  if (code === "SESSION_ENDED") {
    return {
      disconnected: true,
      message: "This Pedelec session ended. Connect again to start a new one.",
    };
  }

  return {
    disconnected: false,
    message: error.message || "Pedelec returned an unexpected error.",
  };
}

function createSessionSettingsInput(settings: ShapeRainSessionSettings): CreateSessionInput {
  const skillsUrls = [`${location.origin}/tools.md`, `${location.origin}/tools.json`];
  if (settings.provider === "default") {
    return { skillsUrls };
  }

  const model = settings.model.trim();
  return model ? { provider: settings.provider, model, skillsUrls } : { provider: settings.provider, skillsUrls };
}

function readStoredSessionSettings(): ShapeRainSessionSettings {
  if (typeof localStorage === "undefined") return DEFAULT_SESSION_SETTINGS;

  try {
    const raw = localStorage.getItem(SHAPE_RAIN_SESSION_SETTINGS_KEY);
    if (!raw) return DEFAULT_SESSION_SETTINGS;
    return normalizeSessionSettings(JSON.parse(raw));
  } catch {
    return DEFAULT_SESSION_SETTINGS;
  }
}

function writeStoredSessionSettings(settings: ShapeRainSessionSettings): void {
  if (typeof localStorage === "undefined") return;

  try {
    localStorage.setItem(SHAPE_RAIN_SESSION_SETTINGS_KEY, JSON.stringify(settings));
  } catch {
    // Storage failures should not block reconnecting with the selected settings.
  }
}

function normalizeSessionSettings(value: unknown): ShapeRainSessionSettings {
  if (!value || typeof value !== "object") return DEFAULT_SESSION_SETTINGS;

  const raw = value as Partial<ShapeRainSessionSettings>;
  if (raw.provider === "default") {
    return { provider: "default", model: "" };
  }

  if (!isProviderCode(raw.provider)) {
    return DEFAULT_SESSION_SETTINGS;
  }

  return {
    provider: raw.provider,
    model: typeof raw.model === "string" ? raw.model.trim() : "",
  };
}

function isProviderCode(value: unknown): value is ProviderCode {
  return value === "codex" || value === "gemini" || value === "opencode" || value === "cursor" || value === "claude" || value === "ollama";
}

function sessionSettingsLabel(settings: ShapeRainSessionSettings): string {
  return settings.provider === "default" ? "Default" : settings.provider;
}

function toPedelecError(err: unknown): PedelecError {
  if (!err) return { code: "UNKNOWN_ERROR", message: "Unknown error" };
  if (typeof err === "string") return { code: "UNKNOWN_ERROR", message: err };
  if (err instanceof Error) return { code: "UNKNOWN_ERROR", message: err.message };
  const value = err as Partial<PedelecError>;
  if (typeof value.code === "string" && typeof value.message === "string") {
    return { code: value.code, message: value.message, details: value.details };
  }
  return { code: "UNKNOWN_ERROR", message: "Unknown error", details: err };
}
