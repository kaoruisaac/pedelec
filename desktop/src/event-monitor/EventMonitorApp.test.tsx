/** @vitest-environment jsdom */

import { render } from "solid-js/web";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  eventHandler: undefined as ((event: { payload: unknown }) => void) | undefined,
  unlisten: vi.fn(),
  openThreadSandbox: vi.fn(),
  sendThreadText: vi.fn(),
  listen: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: mocks.listen,
}));

vi.mock("solid-icons/fa", () => ({
  FaRegularFolderOpen: () => null,
}));

vi.mock("./eventMonitorActions", () => ({
  openThreadSandbox: mocks.openThreadSandbox,
  sendThreadText: mocks.sendThreadText,
}));

import { EventMonitorApp } from "./EventMonitorApp";

let activeDispose: (() => void) | undefined;

describe("EventMonitorApp Debug Prompt", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
    mocks.eventHandler = undefined;
    mocks.unlisten.mockReset();
    mocks.openThreadSandbox.mockReset();
    mocks.sendThreadText.mockReset();
    mocks.sendThreadText.mockResolvedValue({ threadId: "t000123" });
    mocks.listen.mockReset();
    mocks.listen.mockImplementation(async (_eventName: string, handler: (event: { payload: unknown }) => void) => {
      mocks.eventHandler = handler;
      return mocks.unlisten;
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
});

function mountMonitor(): HTMLElement {
  const container = document.createElement("div");
  document.body.append(container);
  activeDispose = render(() => <EventMonitorApp />, container);
  return container;
}

function emitThread(threadId: string, status: string): void {
  mocks.eventHandler!({ payload: { type: "created", threadId } });
  emitStatus(threadId, status);
}

function emitStatus(threadId: string, status: string): void {
  mocks.eventHandler!({ payload: { type: "status_changed", threadId, status } });
}

function promptTextarea(container: HTMLElement): HTMLTextAreaElement {
  return container.querySelector<HTMLTextAreaElement>('textarea[aria-label="Debug prompt"]')!;
}

function sendButton(container: HTMLElement): HTMLButtonElement {
  return container.querySelector<HTMLButtonElement>('button[type="submit"]')!;
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
