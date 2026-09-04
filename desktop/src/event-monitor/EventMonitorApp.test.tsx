/** @vitest-environment jsdom */

import { render } from "solid-js/web";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  eventHandlers: new Map<string, (event: { payload: unknown }) => void>(),
  unlisteners: new Map<string, ReturnType<typeof vi.fn>>(),
  monitorEndThread: vi.fn(),
  openThreadWorkspace: vi.fn(),
  sendThreadText: vi.fn(),
  listen: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: mocks.listen,
}));

vi.mock("solid-icons/fa", () => ({
  FaRegularFolderOpen: () => null,
  FaSolidStop: () => null,
  FaSolidTrash: () => null,
}));

vi.mock("./eventMonitorActions", () => ({
  monitorEndThread: mocks.monitorEndThread,
  openThreadWorkspace: mocks.openThreadWorkspace,
  sendThreadText: mocks.sendThreadText,
}));

import { EventMonitorApp } from "./EventMonitorApp";

let activeDispose: (() => void) | undefined;

describe("EventMonitorApp Debug Prompt", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
    mocks.eventHandlers.clear();
    mocks.unlisteners.clear();
    mocks.monitorEndThread.mockReset();
    mocks.monitorEndThread.mockResolvedValue(undefined);
    mocks.openThreadWorkspace.mockReset();
    mocks.sendThreadText.mockReset();
    mocks.sendThreadText.mockResolvedValue({ threadId: "t000123" });
    mocks.listen.mockReset();
    mocks.listen.mockImplementation(async (eventName: string, handler: (event: { payload: unknown }) => void) => {
      const unlisten = vi.fn();
      mocks.eventHandlers.set(eventName, handler);
      mocks.unlisteners.set(eventName, unlisten);
      return unlisten;
    });
  });

  afterEach(() => {
    activeDispose?.();
    activeDispose = undefined;
    document.body.innerHTML = "";
  });

  it("enables Send for idle and ended threads with non-whitespace prompt", async () => {
    const container = mountMonitor();
    emitThread("t000123", "idle");
    await tick();

    const textarea = promptTextarea(container);
    const send = sendButton(container);
    expect(send.disabled).toBe(true);

    setPrompt(textarea, "   ");
    await tick();
    expect(send.disabled).toBe(true);

    setPrompt(textarea, "  What did you just change?  ");
    await tick();
    expect(send.disabled).toBe(false);

    for (const status of ["running", "waitingToolResult", "stopping", "error"]) {
      emitStatus("t000123", status);
      await tick();
      expect(send.disabled).toBe(true);
      expect(textarea.disabled).toBe(true);
    }

    emitStatus("t000123", "ended");
    await tick();
    expect(send.disabled).toBe(false);
    expect(textarea.disabled).toBe(false);
  });

  it("submits an ended thread through the same guarded path", async () => {
    const container = mountMonitor();
    emitThread("t000123", "ended");
    await tick();

    const textarea = promptTextarea(container);
    setPrompt(textarea, "  What happened before the thread ended?  ");
    sendButton(container).click();
    await tick();

    expect(mocks.sendThreadText).toHaveBeenCalledWith(
      "t000123",
      "What happened before the thread ended?",
    );
    expect(textarea.value).toBe("");
  });

  it("submits the selected thread and trimmed message, then clears on success", async () => {
    const container = mountMonitor();
    emitThread("t000123", "idle");
    await tick();

    const textarea = promptTextarea(container);
    setPrompt(textarea, "  Why did you choose this change?  ");
    sendButton(container).click();
    await tick();

    expect(mocks.sendThreadText).toHaveBeenCalledTimes(1);
    expect(mocks.sendThreadText).toHaveBeenCalledWith("t000123", "Why did you choose this change?");
    expect(textarea.value).toBe("");
  });

  it("keeps the local submit lock until send_text settles", async () => {
    let resolveSend!: (value: { threadId: string }) => void;
    mocks.sendThreadText.mockReturnValueOnce(
      new Promise<{ threadId: string }>((resolve) => {
        resolveSend = resolve;
      }),
    );

    const container = mountMonitor();
    emitThread("t000123", "idle");
    await tick();
    const textarea = promptTextarea(container);
    setPrompt(textarea, "Explain the last tool call.");

    sendButton(container).click();
    sendButton(container).click();
    expect(mocks.sendThreadText).toHaveBeenCalledTimes(1);
    expect(sendButton(container).disabled).toBe(true);

    resolveSend({ threadId: "t000123" });
    await tick();
    expect(sendButton(container).disabled).toBe(true);
    expect(textarea.value).toBe("");
  });

  it("uses the same guarded submit path for Ctrl+Enter and Cmd+Enter", async () => {
    const container = mountMonitor();
    emitThread("t000123", "idle");
    await tick();
    const textarea = promptTextarea(container);

    setPrompt(textarea, "First shortcut");
    textarea.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", ctrlKey: true, bubbles: true }));
    await tick();
    expect(mocks.sendThreadText).toHaveBeenNthCalledWith(1, "t000123", "First shortcut");

    await settlePendingSend();
    setPrompt(textarea, "Second shortcut");
    textarea.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", metaKey: true, bubbles: true }));
    await tick();
    expect(mocks.sendThreadText).toHaveBeenNthCalledWith(2, "t000123", "Second shortcut");
  });

  it("preserves the prompt and shows a readable error when send_text rejects", async () => {
    mocks.sendThreadText.mockRejectedValueOnce(new Error("thread is already running"));

    const container = mountMonitor();
    emitThread("t000123", "idle");
    await tick();
    const textarea = promptTextarea(container);
    setPrompt(textarea, "  Retry this later.  ");
    sendButton(container).click();
    await tick();

    expect(textarea.value).toBe("  Retry this later.  ");
    expect(container.querySelector(".event-monitor-global-error")?.textContent).toContain(
      "thread is already running",
    );
  });

  it("clears a draft when the selected thread changes", async () => {
    const container = mountMonitor();
    emitThread("t000123", "idle");
    emitThread("t000456", "idle");
    await tick();

    const firstThread = threadButtons(container).find((button) => button.textContent?.includes("t000123"))!;
    const secondThread = threadButtons(container).find((button) => button.textContent?.includes("t000456"))!;
    const textarea = promptTextarea(container);
    setPrompt(textarea, "Draft for the first thread");

    secondThread.click();
    await tick();
    expect(promptTextarea(container).value).toBe("");

    firstThread.click();
    await tick();
    expect(promptTextarea(container).value).toBe("");
  });

  it("renders a Stop session control for the selected thread", async () => {
    const container = mountMonitor();
    emitThread("t000123", "running");
    await tick();

    const stop = stopButton(container);
    expect(stop).not.toBeNull();
    expect(stop.title).toBe("Stop session");
    expect(stop.disabled).toBe(false);

    stop.click();
    await tick();
    expect(mocks.monitorEndThread).toHaveBeenCalledWith("t000123");
  });

  it("allows stopping every non-terminal Monitor state", async () => {
    const container = mountMonitor();
    emitThread("t000123", "idle");
    await tick();

    for (const status of ["idle", "running", "waitingToolResult", "error"]) {
      emitStatus("t000123", status);
      await tick();
      mocks.monitorEndThread.mockClear();

      const stop = stopButton(container);
      expect(stop.disabled).toBe(false);
      stop.click();
      await tick();
      expect(mocks.monitorEndThread).toHaveBeenCalledWith("t000123");
    }
  });

  it("disables Stop for stopping and ended threads", async () => {
    const container = mountMonitor();
    emitThread("t000123", "idle");
    await tick();

    for (const status of ["stopping", "ended"]) {
      emitStatus("t000123", status);
      await tick();
      expect(stopButton(container).disabled).toBe(true);
    }
  });

  it("keeps Stop locked until the selected thread request settles", async () => {
    let resolveStop!: () => void;
    mocks.monitorEndThread.mockReturnValueOnce(
      new Promise<void>((resolve) => {
        resolveStop = resolve;
      }),
    );

    const container = mountMonitor();
    emitThread("t000123", "idle");
    await tick();

    stopButton(container).click();
    stopButton(container).click();
    expect(mocks.monitorEndThread).toHaveBeenCalledTimes(1);
    expect(stopButton(container).disabled).toBe(true);

    resolveStop();
    await tick();
    expect(stopButton(container).disabled).toBe(false);
  });

  it("surfaces Stop failures and unlocks the action for retry", async () => {
    mocks.monitorEndThread.mockRejectedValueOnce(new Error("thread cannot be stopped"));

    const container = mountMonitor();
    emitThread("t000123", "idle");
    await tick();
    stopButton(container).click();
    await tick();

    expect(container.querySelector(".event-monitor-global-error")?.textContent).toContain(
      "thread cannot be stopped",
    );
    expect(stopButton(container).disabled).toBe(false);
  });

  it("does not apply one thread's pending Stop state to another selection", async () => {
    let resolveFirstStop!: () => void;
    mocks.monitorEndThread.mockImplementation((threadId: string) => {
      if (threadId === "t000123") {
        return new Promise<void>((resolve) => {
          resolveFirstStop = resolve;
        });
      }
      return Promise.resolve();
    });

    const container = mountMonitor();
    emitThread("t000123", "idle");
    emitThread("t000456", "idle");
    await tick();

    stopButton(container).click();
    expect(mocks.monitorEndThread).toHaveBeenCalledWith("t000123");

    threadButtons(container).find((button) => button.textContent?.includes("t000456"))!.click();
    await tick();
    expect(stopButton(container).disabled).toBe(false);

    stopButton(container).click();
    await tick();
    expect(mocks.monitorEndThread).toHaveBeenCalledWith("t000456");

    threadButtons(container).find((button) => button.textContent?.includes("t000123"))!.click();
    await tick();
    expect(stopButton(container).disabled).toBe(true);

    resolveFirstStop();
    await tick();
  });

  it("keeps the clear-ended control disabled when no ended threads exist", async () => {
    const container = mountMonitor();
    emitThread("t000123", "running");
    await tick();

    expect(clearEndedButton(container).disabled).toBe(true);
  });

  it("clears all ended threads and keeps metrics for retained threads", async () => {
    const container = mountMonitor();
    emitThread("t000123", "running");
    emitThread("t000456", "ended");
    emitThread("t000789", "ended");
    await tick();

    expect(clearEndedButton(container).disabled).toBe(false);
    clearEndedButton(container).click();
    await tick();

    expect(threadButtons(container)).toHaveLength(1);
    expect(threadButtons(container)[0].textContent).toContain("t000123");
    expect(metricValue(container, "Total sessions")).toBe("1");
    expect(metricValue(container, "Total events")).toBe("2");
    expect(clearEndedButton(container).disabled).toBe(true);
  });

  it("falls back to the first remaining thread when the selected ended thread is cleared", async () => {
    const container = mountMonitor();
    emitThread("t000123", "ended");
    emitThread("t000456", "running");
    await tick();

    clearEndedButton(container).click();
    await tick();

    expect(threadButtons(container)).toHaveLength(1);
    expect(container.querySelector(".event-monitor-summary")?.textContent).toContain("t000456");
  });

  it("shows the empty Monitor state when all ended threads are cleared", async () => {
    const container = mountMonitor();
    emitThread("t000123", "ended");
    emitThread("t000456", "ended");
    await tick();

    clearEndedButton(container).click();
    await tick();

    expect(threadButtons(container)).toHaveLength(0);
    expect(container.querySelector(".event-monitor-empty-state")?.textContent).toContain(
      "No App Thread events yet.",
    );
    expect(metricValue(container, "Total sessions")).toBe("0");
    expect(metricValue(container, "Total events")).toBe("0");
  });

  it("renders raw protocol traffic, runtime stderr, and runtime errors without a Runtime Diagnostics panel", async () => {
    const container = mountMonitor();
    emitThread("t000123", "running");
    emitThreadEvent({
      type: "provider_runtime_attached",
      provider: "codex",
      threadId: "t000123",
      providerThreadId: "provider-thread",
      processId: 4321,
      runtimeGeneration: 9,
      resumed: false,
    });
    emitProtocolTraffic({
      type: "provider_protocol_traffic",
      provider: "codex",
      threadId: "t000123",
      processId: 4321,
      runtimeGeneration: 9,
      ts: "2026-09-02T08:00:00.000Z",
      direction: "provider_to_client",
      kind: "response",
      message: { id: 12, result: { marker: "full-rpc-frame" } },
      unmatched: true,
    });
    emitThreadEvent({
      type: "provider_runtime_stderr",
      provider: "codex",
      processId: 4321,
      runtimeGeneration: 9,
      text: "persistent stderr marker",
    });
    emitThreadEvent({
      type: "provider_runtime_error",
      provider: "codex",
      threadId: "t000123",
      processId: 4321,
      runtimeGeneration: 9,
      code: "PROVIDER_PROTOCOL_ERROR",
      message: "runtime error marker",
    });
    await tick();

    expect(container.textContent).toContain("Protocol Traffic");
    expect(container.textContent).not.toContain("Commands");
    expect(container.textContent).not.toContain("Stdout");
    expect(container.textContent).not.toContain("Runtime Diagnostics");
    expect(container.textContent).toContain("full-rpc-frame");
    expect(container.textContent).toContain("unmatched");
    expect(container.textContent).toContain("persistent stderr marker");
    expect(container.textContent).toContain("runtime error marker");
  });

  it("subscribes to both monitor event streams and cleans up both listeners", async () => {
    mountMonitor();
    await tick();

    expect(mocks.listen).toHaveBeenCalledWith("thread_event", expect.any(Function));
    expect(mocks.listen).toHaveBeenCalledWith("provider_protocol_traffic", expect.any(Function));

    activeDispose?.();
    activeDispose = undefined;
    expect(mocks.unlisteners.get("thread_event")).toHaveBeenCalledTimes(1);
    expect(mocks.unlisteners.get("provider_protocol_traffic")).toHaveBeenCalledTimes(1);
  });
});

function mountMonitor(): HTMLElement {
  const container = document.createElement("div");
  document.body.append(container);
  activeDispose = render(() => <EventMonitorApp />, container);
  return container;
}

function emitThread(threadId: string, status: string): void {
  emitThreadEvent({ type: "created", threadId });
  emitStatus(threadId, status);
}

function emitStatus(threadId: string, status: string): void {
  emitThreadEvent({ type: "status_changed", threadId, status });
}

function emitThreadEvent(payload: unknown): void {
  mocks.eventHandlers.get("thread_event")!({ payload });
}

function emitProtocolTraffic(payload: unknown): void {
  mocks.eventHandlers.get("provider_protocol_traffic")!({ payload });
}

function promptTextarea(container: HTMLElement): HTMLTextAreaElement {
  return container.querySelector<HTMLTextAreaElement>('textarea[aria-label="Debug prompt"]')!;
}

function sendButton(container: HTMLElement): HTMLButtonElement {
  return container.querySelector<HTMLButtonElement>('button[type="submit"]')!;
}

function stopButton(container: HTMLElement): HTMLButtonElement {
  return container.querySelector<HTMLButtonElement>('button[aria-label="Stop session"]')!;
}

function clearEndedButton(container: HTMLElement): HTMLButtonElement {
  return container.querySelector<HTMLButtonElement>('button[aria-label="Clear ended sessions"]')!;
}

function metricValue(container: HTMLElement, label: string): string {
  const metric = [...container.querySelectorAll<HTMLElement>(".event-monitor-metric")].find(
    (item) => item.querySelector("span")?.textContent === label,
  );
  return metric?.querySelector("strong")?.textContent || "";
}

function threadButtons(container: HTMLElement): HTMLButtonElement[] {
  return [...container.querySelectorAll<HTMLButtonElement>(".event-monitor-thread-item")];
}

function setPrompt(textarea: HTMLTextAreaElement, value: string): void {
  textarea.value = value;
  textarea.dispatchEvent(new Event("input", { bubbles: true }));
}

async function settlePendingSend(): Promise<void> {
  await tick();
}

async function tick(): Promise<void> {
  await Promise.resolve();
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  await Promise.resolve();
}
