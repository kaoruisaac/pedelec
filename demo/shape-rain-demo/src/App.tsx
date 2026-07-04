import { createMemo, createSignal, onCleanup, onMount, Show } from "solid-js";
import { Pedelec, type PedelecError, type PedelecSession, type PedelecSessionStatus } from "pedelec";
import { normalizeSpawnCommand, type SpawnBasicShapesResult } from "./commands";
import { createShapeWorld } from "./shapeWorldFactory";
import type { RenderMode, ShapeWorldLike } from "./shapeWorldTypes";

type UiState = "ready" | "connecting" | "switching" | "submitting" | "generating" | "error" | "disconnected";

type RuntimeStatus = {
  label: string;
  detail: string;
};

const EXAMPLE_PROMPTS = "Try: pink triangle, five blue circles, yellow stars";

export default function App() {
  let stageElement: HTMLDivElement | undefined;
  let sessionDisposer: (() => void) | undefined;
  let world: ShapeWorldLike | null = null;
  let lifecycleId = 0;

  const [prompt, setPrompt] = createSignal("");
  const [renderMode, setRenderMode] = createSignal<RenderMode>("3d");
  const [uiState, setUiState] = createSignal<UiState>("connecting");
  const [session, setSession] = createSignal<PedelecSession | null>(null);
  const [sessionStatus, setSessionStatus] = createSignal<PedelecSessionStatus | "none">("none");
  const [message, setMessage] = createSignal("Connecting to Pedelec...");
  const [lastToolResult, setLastToolResult] = createSignal<SpawnBasicShapesResult | null>(null);
  const [chatPreview, setChatPreview] = createSignal("");

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
        setUiState("disconnected");
        setMessage("Pedelec Extension is unavailable. Open this page in Chrome with the extension installed.");
        return;
      }
      if (!approval.approved) {
        setMessage("Approve this site in the Pedelec Extension popup, then connect again.");
      }

      const nextSession = await client.createSession({
        skillsUrls: [`${location.origin}/tools.md`, `${location.origin}/tools.json`],
      });
      if (generation !== lifecycleId) {
        await nextSession.end().catch(() => undefined);
        return;
      }

      registerSession(nextSession, generation);
      setUiState("ready");
      setMessage("Ready. Describe the basic shapes you want to drop.");
    } catch (err) {
      if (generation !== lifecycleId) return;
      const friendly = friendlyPedelecError(err);
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
        setMessage("Ready. Describe the basic shapes you want to drop.");
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
        setUiState("error");
        setMessage("The Pedelec session reported an error. Connect again if it does not recover.");
      }
    });
    const disposeError = nextSession.onError((error) => {
      if (generation !== lifecycleId) return;
      const friendly = friendlyPedelecError(error);
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

    try {
      await activeSession.sendText(text);
    } catch (err) {
      const friendly = friendlyPedelecError(err);
      setUiState(friendly.disconnected ? "disconnected" : "error");
      setMessage(friendly.message);
    }
  }

  function handleTool(
    tool: string,
    args: unknown,
    generation = lifecycleId,
  ): SpawnBasicShapesResult | { error: { code: string; message: string; details?: unknown } } {
    if (generation !== lifecycleId || !world) {
      return {
        error: {
          code: "STALE_TOOL_CALL",
          message: "This tool call belongs to an older render session.",
        },
      };
    }
    setUiState("generating");
    if (tool !== "spawn_basic_shapes") {
      return {
        error: {
          code: "TOOL_HANDLER_NOT_FOUND",
          message: `Shape Rain does not support the frontend tool "${tool}".`,
          details: { tool },
        },
      };
    }

    const result = normalizeSpawnCommand(args, renderMode());
    if (result.normalizedItems.length > 0) {
      const spawned = world.spawn(result.normalizedItems);
      const finalResult = { ...result, spawned, success: spawned > 0 };
      setLastToolResult(finalResult);
      setMessage(spawned > 0 ? `Dropped ${spawned} shape${spawned === 1 ? "" : "s"}.` : "The command was valid, but no shapes could be spawned.");
      return finalResult;
    }

    setLastToolResult(result);
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

  return (
    <main class="app-shell">
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

        <div class="status-line" data-state={uiState()}>
          <strong>{runtimeStatus().label}</strong>
          <span>{runtimeStatus().detail}</span>
        </div>

        <p class="message-line">{message()}</p>

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

      <footer class="hint-line">Enter sends the prompt to Pedelec. The frontend only executes validated shape tool commands.</footer>
    </main>
  );
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
