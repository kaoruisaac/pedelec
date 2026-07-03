import { createMemo, createSignal, onCleanup, onMount, Show } from "solid-js";
import { Pedelec, type PedelecError, type PedelecSession, type PedelecSessionStatus } from "pedelec";
import { normalizeSpawnCommand, type SpawnBasicShapesResult } from "./commands";
import { ShapeWorld } from "./shapeWorld";

type UiState = "ready" | "connecting" | "submitting" | "generating" | "error" | "disconnected";

type RuntimeStatus = {
  label: string;
  detail: string;
};

const EXAMPLE_PROMPTS = "Try: pink triangle, five blue circles, yellow stars";

export default function App() {
  let stageElement: HTMLDivElement | undefined;
  let sessionDisposer: (() => void) | undefined;
  const world = new ShapeWorld();

  const [prompt, setPrompt] = createSignal("");
  const [uiState, setUiState] = createSignal<UiState>("connecting");
  const [session, setSession] = createSignal<PedelecSession | null>(null);
  const [sessionStatus, setSessionStatus] = createSignal<PedelecSessionStatus | "none">("none");
  const [message, setMessage] = createSignal("Connecting to Pedelec...");
  const [lastToolResult, setLastToolResult] = createSignal<SpawnBasicShapesResult | null>(null);
  const [chatPreview, setChatPreview] = createSignal("");

  const busy = createMemo(() => {
    const status = sessionStatus();
    return uiState() === "connecting" || uiState() === "submitting" || status === "running" || status === "waiting_tool_result";
  });

  const canSubmit = createMemo(() => Boolean(prompt().trim()) && !busy() && sessionStatus() !== "ended" && sessionStatus() !== "error");

  const runtimeStatus = createMemo<RuntimeStatus>(() => {
    const current = uiState();
    if (current === "ready") return { label: "Ready", detail: "Pedelec connected" };
    if (current === "connecting") return { label: "Connecting", detail: "Checking Extension and Desktop App" };
    if (current === "submitting") return { label: "Submitting", detail: "Agent is reading your request" };
    if (current === "generating") return { label: "Generating", detail: "Tool command received" };
    if (current === "disconnected") return { label: "Disconnected", detail: "Pedelec is unavailable" };
    return { label: "Error", detail: "Action needed" };
  });

  onMount(() => {
    if (stageElement) {
      void world.mount(stageElement);
    }
    void connectPedelec();
  });

  onCleanup(() => {
    sessionDisposer?.();
    world.destroy();
  });

  async function connectPedelec(): Promise<void> {
    sessionDisposer?.();
    sessionDisposer = undefined;
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

      registerSession(nextSession);
      setUiState("ready");
      setMessage("Ready. Describe the basic shapes you want to drop.");
    } catch (err) {
      const friendly = friendlyPedelecError(err);
      setUiState(friendly.disconnected ? "disconnected" : "error");
      setMessage(friendly.message);
    }
  }

  function registerSession(nextSession: PedelecSession): void {
    sessionDisposer?.();
    const disposeStatus = nextSession.onStatus((status) => {
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
      const friendly = friendlyPedelecError(error);
      setUiState(friendly.disconnected ? "disconnected" : "error");
      setMessage(friendly.message);
    });
    const disposeEnded = nextSession.onEnded(() => {
      setSessionStatus("ended");
      setUiState("disconnected");
      setMessage("This Pedelec session ended. Connect again to start a new one.");
    });
    const disposeChat = nextSession.onChat((text) => {
      setChatPreview((current) => (current + text).slice(-180));
    });
    const disposeTool = nextSession.onTool((tool, args) => handleTool(tool, args));
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

  function handleTool(tool: string, args: unknown): SpawnBasicShapesResult | { error: { code: string; message: string; details?: unknown } } {
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

    const result = normalizeSpawnCommand(args);
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
    const result = normalizeSpawnCommand({
      items: [
        { shape: "circle", count: 3, color: "yellow", size: "medium" },
        { shape: "triangle", count: 2, color: "green", size: 58 },
        { shape: "square", count: 2, color: "blue", size: 54 },
        { shape: "star", count: 1, color: "pink", size: 62 },
      ],
    });
    world.spawn(result.normalizedItems);
    setLastToolResult(result);
  }

  return (
    <main class="app-shell">
      <div ref={stageElement} class="stage-layer" aria-hidden="true" />

      <header class="topbar">
        <div class="brand">
          <div class="brand-mark" aria-hidden="true">
            <span />
            <i />
          </div>
          <h1>Shape Rain</h1>
        </div>
        <nav class="toolbar" aria-label="Shape Rain tools">
          <button type="button" title="Reconnect Pedelec" onClick={() => void connectPedelec()}>
            ↻
          </button>
          <button type="button" title="Drop demo shapes" onClick={spawnDemoBatch}>
            ◇
          </button>
          <button type="button" title="Clear shapes" onClick={() => world.clearObjects()}>
            ⌫
          </button>
        </nav>
      </header>

      <section class="interaction-layer" aria-label="Shape Rain prompt">
        <div class="fall-guide" aria-hidden="true">
          <span />
          <span />
          <span />
        </div>

        <form class="prompt-card" onSubmit={submitPrompt}>
          <input
            value={prompt()}
            disabled={uiState() === "connecting" || sessionStatus() === "ended"}
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
