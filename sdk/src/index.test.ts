import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { PEDELEC_EXTENSION_ID } from "./extension-id";
import { Pedelec, defineTool, type PedelecAvailability, type ToolCallContext } from "./index";
import { SDK_VERSION } from "./version.generated";

class MockDocument {
  visibilityState: DocumentVisibilityState = "visible";
  hidden = false;
  focused = true;
  listeners: Record<string, Array<() => void>> = {};

  hasFocus(): boolean {
    return this.focused;
  }

  addEventListener(type: string, listener: () => void): void {
    (this.listeners[type] ??= []).push(listener);
  }

  dispatch(type: string): void {
    for (const listener of this.listeners[type] ?? []) listener();
  }
}

class MockWindow {
  location = { origin: "https://app.example.test" };
  port = new MockRuntimePort();
  queuedPorts: MockRuntimePort[] = [];
  queuedConnectErrors: unknown[] = [];
  connectCalls: Array<{ extensionId: string; connectInfo: { name: string } }> = [];
  listeners: Record<string, Array<() => void>> = {};
  document = new MockDocument();

  postMessage(_message: any, _targetOrigin: string): void {
    throw new Error("window.postMessage should not be used by the SDK transport");
  }

  addEventListener(type: string, listener: () => void): void {
    if (type === "message") {
      throw new Error("window.addEventListener should not be used by the SDK transport");
    }
    (this.listeners[type] ??= []).push(listener);
  }

  dispatch(type: string): void {
    for (const listener of this.listeners[type] ?? []) listener();
  }

  emitFromExtension(message: any): void {
    this.port.emit(message);
  }

  emitFromOtherSource(_message: any): void {
    // External runtime messaging does not expose arbitrary page message sources.
  }

  lastSent(): any {
    return this.port.sent.at(-1);
  }

  queuePort(port: MockRuntimePort): void {
    this.queuedPorts.push(port);
  }

  queueConnectFailure(error: unknown): void {
    this.queuedConnectErrors.push(error);
  }
}

function requestMessages(port: MockRuntimePort): any[] {
  return port.sent.filter((message) => message?.type !== "page_activity" && message?.type !== "sdk_hello");
}

function sdkHelloMessage() {
  return { type: "sdk_hello", sdkVersion: SDK_VERSION };
}

function setPageHidden(pageWindow: MockWindow): void {
  pageWindow.document.hidden = true;
  pageWindow.document.visibilityState = "hidden";
  pageWindow.document.focused = false;
  pageWindow.document.dispatch("visibilitychange");
}

function setPageVisible(pageWindow: MockWindow, focused = true): void {
  pageWindow.document.hidden = false;
  pageWindow.document.visibilityState = "visible";
  pageWindow.document.focused = focused;
  pageWindow.document.dispatch("visibilitychange");
}

function focusPage(pageWindow: MockWindow): void {
  pageWindow.document.hidden = false;
  pageWindow.document.visibilityState = "visible";
  pageWindow.document.focused = true;
  pageWindow.dispatch("focus");
}

function blurPage(pageWindow: MockWindow): void {
  pageWindow.document.focused = false;
  pageWindow.dispatch("blur");
}

class MockRuntimePort {
  sent: any[] = [];
  messageListeners: Array<(message: any) => void> = [];
  disconnectListeners: Array<() => void> = [];
  onMessage = {
    addListener: (listener: (message: any) => void) => this.messageListeners.push(listener),
  };
  onDisconnect = {
    addListener: (listener: () => void) => this.disconnectListeners.push(listener),
  };

  postMessage(message: any): void {
    this.sent.push(message);
  }

  emit(message: any): void {
    for (const listener of this.messageListeners) {
      listener(message);
    }
  }

  disconnect(): void {
    for (const listener of this.disconnectListeners) {
      listener();
    }
  }
}

function installWindowMock() {
  const pageWindow = new MockWindow();
  (globalThis as any).window = pageWindow;
  (globalThis as any).chrome = {
    runtime: {
      lastError: null,
      connect: (extensionId: string, connectInfo: { name: string }) => {
        pageWindow.connectCalls.push({ extensionId, connectInfo });
        const connectError = pageWindow.queuedConnectErrors.shift();
        if (connectError) throw connectError;
        pageWindow.port = pageWindow.queuedPorts.shift() ?? pageWindow.port;
        return pageWindow.port;
      },
    },
  };
  return pageWindow;
}

function respondOk(pageWindow: MockWindow, request: any, result: unknown = {}): void {
  pageWindow.emitFromExtension({
    source: "pedelec-sdk-extension",
    channelId: request.channelId,
    type: "response",
    requestId: request.requestId,
    ok: true,
    result,
  });
}

function respondError(pageWindow: MockWindow, request: any, code = "TEST_ERROR"): void {
  pageWindow.emitFromExtension({
    source: "pedelec-sdk-extension",
    channelId: request.channelId,
    type: "response",
    requestId: request.requestId,
    ok: false,
    error: { code, message: code },
  });
}

function respondSettings(
  pageWindow: MockWindow,
  request: any,
  settings = { defaultProvider: null }
): void {
  respondOk(pageWindow, request, settings);
}

function emitEvent(pageWindow: MockWindow, request: any, event: any): void {
  const normalizedEvent = event.type === "operation_completed"
    ? {
        ...event,
        operationId: event.operationId ?? request.operationId,
        operationKind: request.type === "prepare_session" ? "prepare" : "user",
        success: event.success ?? true,
      }
    : (event.operationId === undefined && request.operationId
      ? { ...event, operationId: request.operationId }
      : event);
  pageWindow.emitFromExtension({
    source: "pedelec-sdk-extension",
    channelId: request.channelId,
    ...normalizedEvent,
  });
}

function emitSnapshot(pageWindow: MockWindow, request: any, snapshot: any): void {
  pageWindow.emitFromExtension({
    source: "pedelec-sdk-extension",
    channelId: request.channelId,
    type: "session_snapshot",
    sessionId: snapshot.threadId,
    seq: snapshot.latestSeq,
    snapshot,
  });
}

function nextTick(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

async function createProviderSession(
  pedelec: Pedelec,
  pageWindow: MockWindow,
  provider = "codex",
  sessionId = "thread_1"
) {
  const create = pedelec.createSession({ provider: provider as any });
  const createRequest = pageWindow.lastSent();
  expect(createRequest).toMatchObject({
    type: "create_session",
    input: { provider, effortLevel: "default", skills: undefined },
  });
  respondOk(pageWindow, createRequest, { sessionId });
  return { session: await create, createRequest };
}

async function startTurn(session: { sendText: (text: string) => Promise<void> }, pageWindow: MockWindow) {
  const send = session.sendText("hello");
  const request = pageWindow.lastSent();
  expect(request).toMatchObject({ type: "send_text" });
  respondOk(pageWindow, request);
  await nextTick();
  return { send, request };
}

describe("Pedelec SDK", () => {
  let pageWindow: MockWindow;

  beforeEach(() => {
    pageWindow = installWindowMock();
  });

  afterEach(() => {
    delete (globalThis as any).window;
    delete (globalThis as any).chrome;
  });

  it("uses the production extension id by default", () => {
    expect(PEDELEC_EXTENSION_ID).toBe("ogccgaminlphbkeghldidiiimajfdpag");
  });

  it("reports the SDK version on connection before page activity", () => {
    new Pedelec();
    expect(pageWindow.port.sent).toEqual([
      sdkHelloMessage(),
      { type: "page_activity", active: true },
    ]);
  });

  it("reports the initial page activity state on connection", () => {
    new Pedelec();
    expect(pageWindow.port.sent).toEqual([
      sdkHelloMessage(),
      { type: "page_activity", active: true },
    ]);
  });

  it("reports inactivity for an initial hidden page", () => {
    pageWindow.document.hidden = true;
    pageWindow.document.visibilityState = "hidden";
    pageWindow.document.focused = false;
    new Pedelec();
    expect(pageWindow.port.sent).toEqual([
      sdkHelloMessage(),
      { type: "page_activity", active: false },
    ]);
  });

  it("reports inactivity when the page becomes hidden", () => {
    new Pedelec();
    setPageHidden(pageWindow);
    expect(pageWindow.port.sent.at(-1)).toEqual({ type: "page_activity", active: false });
  });

  it("reports activity when the page returns to the visible foreground", () => {
    new Pedelec();
    setPageHidden(pageWindow);
    setPageVisible(pageWindow, true);
    expect(pageWindow.port.sent.filter((message) => message.type === "page_activity").map((message) => message.active)).toEqual([
      true,
      false,
      true,
    ]);
  });

  it("can reactivate the Pedelec tab context from window focus while visible", () => {
    new Pedelec();
    setPageHidden(pageWindow);
    setPageVisible(pageWindow, false);
    expect(pageWindow.port.sent.at(-1)).toEqual({ type: "page_activity", active: false });
    focusPage(pageWindow);
    expect(pageWindow.port.sent.at(-1)).toEqual({ type: "page_activity", active: true });
  });

  it("does not report inactivity from window.blur alone", () => {
    new Pedelec();
    const before = pageWindow.port.sent.slice();
    blurPage(pageWindow);
    expect(pageWindow.port.sent).toEqual(before);
    expect(pageWindow.port.sent.at(-1)).toEqual({ type: "page_activity", active: true });
  });

  it("re-asserts page activity on window.focus even if already reported active", () => {
    new Pedelec();
    expect(pageWindow.port.sent).toEqual([
      sdkHelloMessage(),
      { type: "page_activity", active: true },
    ]);
    blurPage(pageWindow);
    expect(pageWindow.port.sent.filter((message) => message.type === "page_activity").map((message) => message.active)).toEqual([
      true,
    ]);
    focusPage(pageWindow);
    expect(pageWindow.port.sent.filter((message) => message.type === "page_activity")).toEqual([
      { type: "page_activity", active: true },
      { type: "page_activity", active: true },
    ]);
  });

  it("does not create pending SDK requests or session events for activity messages", async () => {
    const pedelec = new Pedelec();
    const { session, createRequest } = await createProviderSession(pedelec, pageWindow);
    expect(createRequest.requestId).toMatch(/^sdk_\d+_1$/);
    expect(createRequest).toHaveProperty("channelId");

    const hello = pageWindow.port.sent.find((message) => message.type === "sdk_hello");
    expect(hello).toEqual(sdkHelloMessage());
    expect(hello).not.toHaveProperty("requestId");
    expect(hello).not.toHaveProperty("channelId");
    expect(hello).not.toHaveProperty("sessionId");

    const activity = pageWindow.port.sent.find((message) => message.type === "page_activity");
    expect(activity).toEqual({ type: "page_activity", active: true });
    expect(activity).not.toHaveProperty("requestId");
    expect(activity).not.toHaveProperty("channelId");
    expect(activity).not.toHaveProperty("sessionId");

    const errors: unknown[] = [];
    const chats: string[] = [];
    session.onError((error) => errors.push(error));
    session.onChat((text) => chats.push(text));
    setPageHidden(pageWindow);
    pageWindow.emitFromExtension({
      type: "page_activity",
      active: true,
      sessionId: session.sessionId,
      seq: 99,
      channelId: createRequest.channelId,
    });
    await nextTick();
    expect(errors).toEqual([]);
    expect(chats).toEqual([]);
  });

  it("does not report page activity outside a browser page", async () => {
    delete (globalThis as any).window;
    delete (globalThis as any).chrome;
    const pedelec = new Pedelec();
    await expect(pedelec.getApprovalStatus()).resolves.toEqual({
      installed: false,
      approved: false,
      origin: null,
      appConnected: false,
    });
  });

  it("posts runtime messages and creates a session", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.createSession({
      provider: "codex",
      effortLevel: "high",
      skills: {
        guidance: "Use get_app_state for app state.",
        tools: [
          defineTool({
            name: "get_app_state",
            description: "Get app state.",
            argsSchema: {
              type: "object",
              properties: {},
              required: [],
            },
            handler: () => ({ ok: true }),
          }),
        ],
      },
    });
    const request = pageWindow.lastSent();

    expect(request).toMatchObject({
      type: "create_session",
      input: {
        provider: "codex",
        effortLevel: "high",
        skills: {
          guidance: "Use get_app_state for app state.",
          tools: [
            {
              name: "get_app_state",
              description: "Get app state.",
              argsSchema: {
                type: "object",
                properties: {},
                required: [],
              },
            },
          ],
        },
        autoEndOnDisconnect: true,
      },
    });
    expect(JSON.stringify(request.input.skills)).not.toContain("handler");
    expect(request.channelId).toMatch(/^pedelec_/);
    expect(request.requestId).toMatch(/^sdk_/);
    expect(pageWindow.connectCalls).toEqual([
      {
        extensionId: "ogccgaminlphbkeghldidiiimajfdpag",
        connectInfo: { name: "pedelec-sdk-external" },
      },
    ]);

    respondOk(pageWindow, request, { sessionId: "thread_1" });
    const session = await promise;

    expect(session.sessionId).toBe("thread_1");
    expect(session.provider).toBe("codex");
    expect(session.effortLevel).toBe("high");
  });

  it("forwards an explicit workspace path with an explicit provider and effort level", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.createSession({
      provider: "codex",
      effortLevel: "high",
      workspace: { path: "C:\\workspace\\project-a" },
    });
    const request = pageWindow.lastSent();

    expect(request).toMatchObject({
      type: "create_session",
      callerSdkVersion: SDK_VERSION,
      input: {
        provider: "codex",
        effortLevel: "high",
        workspace: { path: "C:\\workspace\\project-a" },
      },
    });
    respondOk(pageWindow, request, { sessionId: "thread_custom_explicit" });
    await promise;
  });

  it("preserves workspace through provider-only and default-provider resolution", async () => {
    const pedelec = new Pedelec();
    const providerOnly = pedelec.createSession({
      provider: "codex",
      workspace: { path: "C:\\workspace\\provider-only" },
    });
    const providerCreate = pageWindow.lastSent();
    expect(providerCreate.input.workspace).toEqual({ path: "C:\\workspace\\provider-only" });
    respondOk(pageWindow, providerCreate, { sessionId: "thread_custom_provider" });
    await providerOnly;

    const defaultProvider = pedelec.createSession({
      workspace: { path: "C:\\workspace\\default" },
    });
    const defaultSettings = pageWindow.lastSent();
    respondSettings(pageWindow, defaultSettings, {
      defaultProvider: "codex",
    });
    await nextTick();
    const providersRequest = pageWindow.lastSent();
    respondOk(pageWindow, providersRequest, [
      { name: "Codex", code: "codex", available: true, isDefault: true, error: null },
    ]);
    await nextTick();
    const defaultCreate = pageWindow.lastSent();
    expect(defaultCreate.input.workspace).toEqual({ path: "C:\\workspace\\default" });
    respondOk(pageWindow, defaultCreate, { sessionId: "thread_custom_default" });
    await defaultProvider;
  });

  it("rejects malformed workspace input before sending a request", async () => {
    const pedelec = new Pedelec();

    await expect(pedelec.createSession({ provider: "codex", workspace: null } as any)).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "workspace must be an object",
    });
    await expect(pedelec.createSession({ provider: "codex", workspace: {} } as any)).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "workspace.path must be a string",
    });
    await expect(
      pedelec.createSession({ provider: "codex", workspace: { path: "   " } })
    ).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "workspace.path must not be empty",
    });
    await expect(
      pedelec.createSession({ provider: "codex", workspace: { path: 123 } } as any)
    ).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "workspace.path must be a string",
    });
    expect(requestMessages(pageWindow.port)).toHaveLength(0);
  });

  it("does not forward the removed sandbox input or expose its picker alias", async () => {
    const pedelec = new Pedelec();
    const create = pedelec.createSession({
      provider: "codex",
      sandbox: { path: "C:\\workspace\\legacy" },
    } as any);
    const request = pageWindow.lastSent();

    expect(request.input).not.toHaveProperty("sandbox");
    expect(request.input.workspace).toBeUndefined();
    expect((pedelec as any).sandboxFolderPicker).toBeUndefined();

    respondOk(pageWindow, request, { sessionId: "thread_default_workspace" });
    await create;
  });

  it("gets approval status from the extension without creating a session", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.getApprovalStatus();
    const request = pageWindow.lastSent();

    expect(request).toMatchObject({
      type: "get_approval_status",
    });

    respondOk(pageWindow, request, {
      installed: true,
      approved: true,
      origin: "https://app.example.test",
      appConnected: true,
    });
    await expect(promise).resolves.toEqual({
      installed: true,
      approved: true,
      origin: "https://app.example.test",
      appConnected: true,
    });
  });

  it("returns unavailable approval status when the extension cannot connect", async () => {
    delete (globalThis as any).chrome;
    const pedelec = new Pedelec();

    await expect(pedelec.getApprovalStatus()).resolves.toEqual({
      installed: false,
      approved: false,
      origin: "https://app.example.test",
      appConnected: false,
    });
  });

  it("reconnects the parent client after a real runtime Port disconnect", async () => {
    const pedelec = new Pedelec();
    const portA = pageWindow.port;
    const initial = pedelec.getSettings();
    respondSettings(pageWindow, pageWindow.lastSent());
    await initial;

    const portB = new MockRuntimePort();
    pageWindow.queuePort(portB);
    portA.disconnect();

    const recovered = pedelec.getSettings();
    expect(pageWindow.connectCalls).toHaveLength(2);
    expect(pageWindow.port).toBe(portB);
    expect(portB.sent[0]).toEqual(sdkHelloMessage());
    expect(portB.sent[1]).toEqual({ type: "page_activity", active: true });
    expect(portB.sent.at(-1)).toMatchObject({ type: "get_settings" });
    respondSettings(pageWindow, pageWindow.lastSent());
    await expect(recovered).resolves.toEqual({ defaultProvider: null });
  });

  it("rejects old-port requests without replaying them, then succeeds on a replacement Port", async () => {
    const pedelec = new Pedelec();
    const portA = pageWindow.port;
    const pendingOldRequest = pedelec.request("get_settings");
    const oldMessage = portA.sent.at(-1);

    const portB = new MockRuntimePort();
    pageWindow.queuePort(portB);
    portA.disconnect();

    await expect(pendingOldRequest).rejects.toMatchObject({ code: "EXTENSION_DISCONNECTED" });
    expect(portB.sent).toHaveLength(0);

    const newRequest = pedelec.request("get_settings");
    const newMessage = portB.sent.at(-1);
    expect(newMessage.requestId).not.toBe(oldMessage.requestId);
    respondSettings(pageWindow, newMessage);
    await expect(newRequest).resolves.toEqual({ defaultProvider: null });
  });

  it("keeps a failed reconnect retryable", async () => {
    const pedelec = new Pedelec();
    const portA = pageWindow.port;
    const portB = new MockRuntimePort();
    pageWindow.queueConnectFailure(new Error("extension unavailable"));
    portA.disconnect();

    await expect(pedelec.getSettings()).rejects.toMatchObject({ code: "EXTENSION_UNAVAILABLE" });

    pageWindow.queuePort(portB);
    const recovered = pedelec.getSettings();
    expect(pageWindow.connectCalls).toHaveLength(3);
    expect(pageWindow.port).toBe(portB);
    respondSettings(pageWindow, pageWindow.lastSent());
    await expect(recovered).resolves.toEqual({ defaultProvider: null });
  });

  it("shares one replacement Port across concurrent operations", async () => {
    const pedelec = new Pedelec();
    const portA = pageWindow.port;
    const portB = new MockRuntimePort();
    pageWindow.queuePort(portB);
    portA.disconnect();

    const settings = pedelec.getSettings();
    const approval = pedelec.getApprovalStatus();
    expect(pageWindow.connectCalls).toHaveLength(2);
    expect(requestMessages(portB).map((message) => message.type)).toEqual(["get_settings", "get_approval_status"]);

    const settingsRequest = requestMessages(portB)[0];
    const approvalRequest = requestMessages(portB)[1];
    respondSettings(pageWindow, settingsRequest);
    respondOk(pageWindow, approvalRequest, {
      installed: true,
      approved: false,
      origin: "https://app.example.test",
      appConnected: true,
    });
    await expect(settings).resolves.toEqual({ defaultProvider: null });
    await expect(approval).resolves.toMatchObject({ installed: true, appConnected: true });
  });

  it("ignores a stale Port A disconnect after Port B has pending requests", async () => {
    const pedelec = new Pedelec();
    const portA = pageWindow.port;
    const portB = new MockRuntimePort();
    pageWindow.queuePort(portB);
    portA.disconnect();

    const request = pedelec.getSettings();
    portA.disconnect();

    expect(pageWindow.port).toBe(portB);
    expect(requestMessages(portB)).toHaveLength(1);
    respondSettings(pageWindow, requestMessages(portB)[0]);
    await expect(request).resolves.toEqual({ defaultProvider: null });
  });

  it("recovers checkAvailability after a prior runtime Port disconnect", async () => {
    const pedelec = new Pedelec();
    const portA = pageWindow.port;
    const first = pedelec.checkAvailability();
    respondOk(pageWindow, pageWindow.lastSent(), {
      installed: true,
      approved: false,
      origin: "https://app.example.test",
      appConnected: true,
    });
    await first;

    const portB = new MockRuntimePort();
    pageWindow.queuePort(portB);
    portA.disconnect();

    const second = pedelec.checkAvailability();
    const approvalRequest = requestMessages(portB)[0];
    expect(approvalRequest).toMatchObject({ type: "get_approval_status" });
    respondOk(pageWindow, approvalRequest, {
      installed: true,
      approved: true,
      origin: "https://app.example.test",
      appConnected: true,
    });
    await nextTick();
    const settingsRequest = requestMessages(portB)[1];
    expect(settingsRequest).toMatchObject({ type: "get_settings" });
    respondSettings(pageWindow, settingsRequest);

    await expect(second).resolves.toMatchObject({
      available: true,
      extension: { available: true },
      approval: { approved: true },
      desktop: { available: true, launchAttempted: true },
    });
  });

  it("does not let an old session handle silently migrate after reconnect", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const portA = pageWindow.port;
    const portB = new MockRuntimePort();
    pageWindow.queuePort(portB);
    portA.disconnect();

    const parentRequest = pedelec.getSettings();
    respondSettings(pageWindow, requestMessages(portB)[0]);
    await parentRequest;

    await expect(session.sendText("must fail")).rejects.toMatchObject({ code: "SESSION_ENDED" });
    expect(requestMessages(portB).map((message) => message.type)).toEqual(["get_settings"]);
  });

  it("rejects same-handle resume after transport detachment", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const sentBeforeDisconnect = pageWindow.port.sent.length;
    pageWindow.port.disconnect();

    await expect(session.resume()).rejects.toMatchObject({ code: "SESSION_ENDED" });
    expect(pageWindow.port.sent.length).toBe(sentBeforeDisconnect);
  });

  it("requires explicit resume to bind a disconnected session to the replacement Port", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const portA = pageWindow.port;
    const portB = new MockRuntimePort();
    pageWindow.queuePort(portB);
    portA.disconnect();

    const resumedPromise = pedelec.resumeSession(session.sessionId);
    const resumeRequest = requestMessages(portB)[0];
    expect(resumeRequest).toMatchObject({ type: "resume_session", sessionId: session.sessionId });
    respondOk(pageWindow, resumeRequest, { sessionId: session.sessionId });
    const resumed = await resumedPromise;
    expect(resumed).not.toBe(session);

    const send = resumed.sendText("after resume");
    const sendRequest = requestMessages(portB)[1];
    respondOk(pageWindow, sendRequest);
    emitEvent(pageWindow, sendRequest, { type: "operation_completed", sessionId: session.sessionId, seq: 1 });
    await send;
  });

  it("creates an opencode session and lists providers", async () => {
    const pedelec = new Pedelec();
    const createPromise = pedelec.createSession({
      provider: "opencode",
      effortLevel: "high",
    });
    const createRequest = pageWindow.lastSent();

    expect(createRequest).toMatchObject({
      type: "create_session",
      input: {
        provider: "opencode",
        effortLevel: "high",
        skills: undefined,
      },
    });

    respondOk(pageWindow, createRequest, { sessionId: "thread_opencode" });
    const session = await createPromise;
    expect(session.provider).toBe("opencode");
    expect(session.effortLevel).toBe("high");

    const listPromise = pedelec.listProviders();
    const listRequest = pageWindow.lastSent();
    expect(listRequest).toMatchObject({
      type: "list_providers",
    });

    respondOk(pageWindow, listRequest, [
      { name: "OpenCode", code: "opencode", available: false, isDefault: false, error: "program was not found in PATH" },
    ]);
    await expect(listPromise).resolves.toEqual([
      { name: "OpenCode", code: "opencode", available: false, isDefault: false, error: "program was not found in PATH" },
    ]);
  });

  it("creates a cursor session and lists cursor provider", async () => {
    const pedelec = new Pedelec();
    const createPromise = pedelec.createSession({
      provider: "cursor",
      effortLevel: "high",
    });
    const createRequest = pageWindow.lastSent();

    expect(createRequest).toMatchObject({
      type: "create_session",
      input: {
        provider: "cursor",
        effortLevel: "high",
        skills: undefined,
      },
    });

    respondOk(pageWindow, createRequest, { sessionId: "thread_cursor" });
    const session = await createPromise;
    expect(session.provider).toBe("cursor");
    expect(session.effortLevel).toBe("high");

    const listPromise = pedelec.listProviders();
    const listRequest = pageWindow.lastSent();
    respondOk(pageWindow, listRequest, [
      { name: "Cursor", code: "cursor", available: false, isDefault: false, error: "program was not found in PATH" },
    ]);

    await expect(listPromise).resolves.toEqual([
      { name: "Cursor", code: "cursor", available: false, isDefault: false, error: "program was not found in PATH" },
    ]);
  });

  it("creates a claude session and lists claude provider", async () => {
    const pedelec = new Pedelec();
    const createPromise = pedelec.createSession({
      provider: "claude",
      effortLevel: "high",
    });
    const createRequest = pageWindow.lastSent();

    expect(createRequest).toMatchObject({
      type: "create_session",
      input: {
        provider: "claude",
        effortLevel: "high",
        skills: undefined,
      },
    });

    respondOk(pageWindow, createRequest, { sessionId: "thread_claude" });
    const session = await createPromise;
    expect(session.provider).toBe("claude");
    expect(session.effortLevel).toBe("high");

    const listPromise = pedelec.listProviders();
    const listRequest = pageWindow.lastSent();
    respondOk(pageWindow, listRequest, [
      { name: "Claude Code", code: "claude", available: false, isDefault: false, error: "program was not found in PATH" },
    ]);

    await expect(listPromise).resolves.toEqual([
      { name: "Claude Code", code: "claude", available: false, isDefault: false, error: "program was not found in PATH" },
    ]);
  });

  it("uses provider isDefault directly without requesting settings", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.listProviders();
    const request = pageWindow.lastSent();

    respondOk(pageWindow, request, [
      {
        name: "Codex",
        code: "codex",
        available: true,
        isDefault: true,
        error: null,
        futureField: "ignored",
      },
    ]);

    await expect(promise).resolves.toEqual([
      { name: "Codex", code: "codex", available: true, isDefault: true, error: null },
    ]);
    expect(requestMessages(pageWindow.port).map((message) => message.type)).toEqual(["list_providers"]);
  });

  it("fills missing provider isDefault values from settings for an old extension", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.listProviders();
    const providersRequest = pageWindow.lastSent();

    respondOk(pageWindow, providersRequest, [
      { name: "Codex", code: "codex", available: true, error: null },
      { name: "Claude Code", code: "claude", available: true, error: null },
    ]);
    await nextTick();

    const settingsRequest = pageWindow.lastSent();
    expect(settingsRequest).toMatchObject({ type: "get_settings" });
    respondOk(pageWindow, settingsRequest, { defaultProvider: "claude" });

    await expect(promise).resolves.toEqual([
      { name: "Codex", code: "codex", available: true, isDefault: false, error: null },
      { name: "Claude Code", code: "claude", available: true, isDefault: true, error: null },
    ]);
  });

  it("recomputes all provider defaults for a mixed provider response", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.listProviders();
    const providersRequest = pageWindow.lastSent();

    respondOk(pageWindow, providersRequest, [
      { name: "Codex", code: "codex", available: true, isDefault: true, error: null },
      { name: "Claude Code", code: "claude", available: true, error: null },
    ]);
    await nextTick();

    const settingsRequest = pageWindow.lastSent();
    expect(settingsRequest).toMatchObject({ type: "get_settings" });
    respondOk(pageWindow, settingsRequest, { defaultProvider: "claude" });

    await expect(promise).resolves.toEqual([
      { name: "Codex", code: "codex", available: true, isDefault: false, error: null },
      { name: "Claude Code", code: "claude", available: true, isDefault: true, error: null },
    ]);
  });

  it("rejects a provider isDefault field with the wrong type", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.listProviders();
    respondOk(pageWindow, pageWindow.lastSent(), [
      { name: "Codex", code: "codex", available: true, isDefault: "yes", error: null },
    ]);

    await expect(promise).rejects.toMatchObject({
      code: "SDK_PROTOCOL_ERROR",
      message: "list_providers response had invalid provider data",
    });
    expect(requestMessages(pageWindow.port).map((message) => message.type)).toEqual(["list_providers"]);
  });

  it("gets settings from the extension", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.getSettings();
    const request = pageWindow.lastSent();

    expect(request).toMatchObject({
      type: "get_settings",
    });

    respondOk(pageWindow, request, {
      defaultProvider: "codex",
      defaultModels: { codex: "future-model" },
      providerSettings: { codex: { effortsArgs: { default: ["-m", "future-model"] } } },
      futureField: true,
    });
    await expect(promise).resolves.toEqual({
      defaultProvider: "codex",
    });
  });

  it("rejects invalid settings shapes from the extension", async () => {
    const pedelec = new Pedelec();

    const ollamaSettings = pedelec.getSettings();
    respondOk(pageWindow, pageWindow.lastSent(), {
      defaultProvider: "ollama",
    });
    await expect(ollamaSettings).resolves.toEqual({
      defaultProvider: "ollama",
    });

    const emptyModels = pedelec.getSettings();
    respondOk(pageWindow, pageWindow.lastSent(), {
      defaultProvider: null,
    });
    await expect(emptyModels).resolves.toEqual({ defaultProvider: null });
  });

  it("creates a session from the default provider and effort level", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.createSession();

    const settingsRequest = pageWindow.lastSent();
    expect(settingsRequest).toMatchObject({ type: "get_settings" });
    respondOk(pageWindow, settingsRequest, {
      defaultProvider: "codex",
    });
    await nextTick();

    const providersRequest = pageWindow.lastSent();
    expect(providersRequest).toMatchObject({ type: "list_providers" });
    respondOk(pageWindow, providersRequest, [
      { name: "Codex", code: "codex", available: true, isDefault: true, error: null },
    ]);
    await nextTick();

    const createRequest = pageWindow.lastSent();
    expect(createRequest).toMatchObject({
      type: "create_session",
      input: { provider: "codex", effortLevel: "default", skills: undefined },
    });
    respondOk(pageWindow, createRequest, { sessionId: "thread_default" });

    const session = await promise;
    expect(session.provider).toBe("codex");
    expect(session.effortLevel).toBe("default");
  });

  it("creates an ollama session with explicit effort level", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.createSession({
      provider: "ollama",
      effortLevel: "high",
    });
    const request = pageWindow.lastSent();

    expect(request).toMatchObject({
      type: "create_session",
      input: {
        provider: "ollama",
        effortLevel: "high",
        skills: undefined,
      },
    });
    respondOk(pageWindow, request, { sessionId: "thread_ollama" });

    const session = await promise;
    expect(session.provider).toBe("ollama");
    expect(session.effortLevel).toBe("high");
  });

  it("uses ollama as default provider with the default effort level", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.createSession();

    respondOk(pageWindow, pageWindow.lastSent(), {
      defaultProvider: "ollama",
    });
    await nextTick();
    respondOk(pageWindow, pageWindow.lastSent(), [
      { name: "Ollama", code: "ollama", available: true, isDefault: true, error: null },
    ]);
    await nextTick();

    const createRequest = pageWindow.lastSent();
    expect(createRequest).toMatchObject({
      type: "create_session",
      input: { provider: "ollama", effortLevel: "default", skills: undefined },
    });
    respondOk(pageWindow, createRequest, { sessionId: "thread_ollama_default" });

    const session = await promise;
    expect(session.provider).toBe("ollama");
    expect(session.effortLevel).toBe("default");
  });

  it("forwards the selected effort level without reading provider settings", async () => {
    const pedelec = new Pedelec();
    const codexPromise = pedelec.createSession({ provider: "codex" });
    const codexCreate = pageWindow.lastSent();
    expect(codexCreate).toMatchObject({
      type: "create_session",
      input: { provider: "codex", effortLevel: "default", skills: undefined },
    });
    respondOk(pageWindow, codexCreate, { sessionId: "thread_codex_default_model" });
    expect((await codexPromise).effortLevel).toBe("default");

    const antigravityPromise = pedelec.createSession({ provider: "antigravity", effortLevel: "low" });
    const antigravityCreate = pageWindow.lastSent();
    expect(antigravityCreate).toMatchObject({
      type: "create_session",
      input: { provider: "antigravity", effortLevel: "low", skills: undefined },
    });
    respondOk(pageWindow, antigravityCreate, { sessionId: "thread_antigravity_no_default_model" });
    expect((await antigravityPromise).effortLevel).toBe("low");

    const ollamaPromise = pedelec.createSession({ provider: "ollama", effortLevel: "high" });
    const ollamaCreate = pageWindow.lastSent();
    expect(ollamaCreate).toMatchObject({
      type: "create_session",
      input: { provider: "ollama", effortLevel: "high", skills: undefined },
    });
    respondOk(pageWindow, ollamaCreate, { sessionId: "thread_ollama_default_model" });
    expect((await ollamaPromise).effortLevel).toBe("high");
  });

  it("normalizes an omitted explicit-provider effort level to default", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.createSession({ provider: "antigravity" });
    const createRequest = pageWindow.lastSent();
    expect(createRequest).toMatchObject({
      type: "create_session",
      input: { provider: "antigravity", effortLevel: "default", skills: undefined },
    });
    respondOk(pageWindow, createRequest, { sessionId: "thread_antigravity_no_model" });
    expect((await promise).effortLevel).toBe("default");
  });

  it("forwards high effort for an explicit provider", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.createSession({ provider: "codex", effortLevel: "high" });
    const request = pageWindow.lastSent();

    expect(request).toMatchObject({
      type: "create_session",
      input: { provider: "codex", effortLevel: "high", skills: undefined },
    });
    respondOk(pageWindow, request, { sessionId: "thread_user_model" });

    expect((await promise).effortLevel).toBe("high");
  });

  it("sends explicit autoEndOnDisconnect lifecycle options", async () => {
    const pedelec = new Pedelec();
    const keepAlivePromise = pedelec.createSession({
      provider: "codex",
      effortLevel: "high",
      autoEndOnDisconnect: false,
    });
    const keepAliveRequest = pageWindow.lastSent();

    expect(keepAliveRequest).toMatchObject({
      type: "create_session",
      input: {
        provider: "codex",
        effortLevel: "high",
        skills: undefined,
        autoEndOnDisconnect: false,
      },
    });
    respondOk(pageWindow, keepAliveRequest, { sessionId: "thread_keep_alive" });
    await keepAlivePromise;

    const pageScopedPromise = pedelec.createSession({
      provider: "codex",
      effortLevel: "high",
      autoEndOnDisconnect: true,
    });
    const pageScopedRequest = pageWindow.lastSent();

    expect(pageScopedRequest).toMatchObject({
      type: "create_session",
      input: {
        provider: "codex",
        effortLevel: "high",
        skills: undefined,
        autoEndOnDisconnect: true,
      },
    });
    respondOk(pageWindow, pageScopedRequest, { sessionId: "thread_page_scoped" });
    await pageScopedPromise;
  });

  it("returns clear errors when default provider is missing or unavailable", async () => {
    const pedelec = new Pedelec();
    const missing = pedelec.createSession();
    respondOk(pageWindow, pageWindow.lastSent(), { defaultProvider: null });
    await expect(missing).rejects.toMatchObject({ code: "DEFAULT_PROVIDER_NOT_SET" });

    const unavailable = pedelec.createSession();
    respondOk(pageWindow, pageWindow.lastSent(), { defaultProvider: "codex" });
    await nextTick();
    respondOk(pageWindow, pageWindow.lastSent(), [
      { name: "Codex", code: "codex", available: false, isDefault: true, error: "missing" },
    ]);
    await expect(unavailable).rejects.toMatchObject({ code: "DEFAULT_PROVIDER_UNAVAILABLE" });
  });

  it("rejects runtime model-only createSession input", async () => {
    const pedelec = new Pedelec();

    await expect(pedelec.createSession({ model: "gpt-5" } as any)).rejects.toMatchObject({
      code: "INVALID_INPUT",
    });
  });

  it("resumes an existing session", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.resumeSession("thread_resume");
    const request = pageWindow.lastSent();

    expect(request).toMatchObject({
      type: "resume_session",
      sessionId: "thread_resume",
    });
    respondOk(pageWindow, request, { sessionId: "thread_resume" });

    expect((await promise).sessionId).toBe("thread_resume");
  });

  it("keeps resumeSession reattachment-only for an ended session", async () => {
    const pedelec = new Pedelec();
    const { session, createRequest } = await createProviderSession(pedelec, pageWindow);
    pageWindow.emitFromExtension({
      source: "pedelec-sdk-extension",
      channelId: createRequest.channelId,
      type: "ended",
      sessionId: session.sessionId,
      seq: 1,
    });

    const resume = pedelec.resumeSession(session.sessionId);
    const request = pageWindow.lastSent();
    expect(request.type).toBe("resume_session");
    emitSnapshot(pageWindow, request, {
      threadId: session.sessionId,
      status: "ended",
      latestSeq: 2,
    });
    respondOk(pageWindow, request, { sessionId: session.sessionId });

    await expect(resume).resolves.toBe(session);
    expect(session.getStatus()).toBe("ended");
    expect(requestMessages(pageWindow.port).some((message) => message.type === "reactivate_session")).toBe(false);
  });

  it("exposes monotonic session usage without invoking lifecycle callbacks", async () => {
    const pedelec = new Pedelec();
    const { session, createRequest } = await createProviderSession(pedelec, pageWindow);
    const statuses: string[] = [];
    const chats: string[] = [];
    const errors: unknown[] = [];
    session.onStatus((status) => statuses.push(status));
    session.onChat((text) => chats.push(text));
    session.onError((error) => errors.push(error));

    expect(session.usage.totalTokens).toBeUndefined();
    pageWindow.emitFromExtension({
      source: "pedelec-sdk-extension",
      channelId: createRequest.channelId,
      type: "usage_updated",
      sessionId: "thread_1",
      seq: 1,
      totalTokens: 12,
    });
    expect(session.usage.totalTokens).toBe(12);

    session.handleEvent({
      type: "usage_updated",
      channelId: createRequest.channelId,
      sessionId: "thread_1",
      totalTokens: 20,
    } as any);
    expect(session.usage.totalTokens).toBe(20);
    session.handleEvent({
      type: "usage_updated",
      channelId: createRequest.channelId,
      sessionId: "thread_1",
      totalTokens: 4,
    } as any);
    session.handleEvent({
      type: "usage_updated",
      channelId: createRequest.channelId,
      sessionId: "thread_1",
      totalTokens: -1,
    } as any);
    session.handleEvent({
      type: "usage_updated",
      channelId: createRequest.channelId,
      sessionId: "thread_1",
      totalTokens: Number.NaN,
    } as any);
    expect(session.usage.totalTokens).toBe(20);
    expect(statuses).toEqual([]);
    expect(chats).toEqual([]);
    expect(errors).toEqual([]);
  });

  it("filters stale usage events by the session sequence cursor", async () => {
    const pedelec = new Pedelec();
    const { session, createRequest } = await createProviderSession(pedelec, pageWindow);
    const event = (seq: number, totalTokens: number) => pageWindow.emitFromExtension({
      source: "pedelec-sdk-extension",
      channelId: createRequest.channelId,
      type: "usage_updated",
      sessionId: "thread_1",
      seq,
      totalTokens,
    });

    event(5, 50);
    event(4, 100);
    expect(session.usage.totalTokens).toBe(50);
    event(6, 60);
    expect(session.usage.totalTokens).toBe(60);
  });

  it("hydrates usage from a resume snapshot and preserves it when snapshots omit or lower it", async () => {
    const pedelec = new Pedelec();
    const resume = pedelec.resumeSession("thread_resume_usage");
    const request = pageWindow.lastSent();
    emitSnapshot(pageWindow, request, {
      threadId: "thread_resume_usage",
      status: "idle",
      latestSeq: 4,
      usage: { totalTokens: 42 },
    });
    respondOk(pageWindow, request, { sessionId: "thread_resume_usage" });
    const session = await resume;
    expect(session.usage.totalTokens).toBe(42);

    session.handleSnapshot({
      threadId: "thread_resume_usage",
      status: "idle",
      latestSeq: 5,
    } as any);
    expect(session.usage.totalTokens).toBe(42);
    session.handleSnapshot({
      threadId: "thread_resume_usage",
      status: "idle",
      latestSeq: 6,
      usage: { totalTokens: 7 },
    } as any);
    expect(session.usage.totalTokens).toBe(42);
  });

  it("resolves sendText only after operation_completed", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const statuses: string[] = [];
    session.onStatus((status) => statuses.push(status));

    let resolved = false;
    const send = session.sendText("hello").then(() => {
      resolved = true;
    });
    const request = pageWindow.lastSent();
    expect(request).toMatchObject({
      type: "send_text",
      sessionId: "thread_1",
      text: "hello",
    });

    respondOk(pageWindow, request);
    await nextTick();
    expect(resolved).toBe(false);

    emitEvent(pageWindow, request, { type: "operation_completed", sessionId: "thread_1", seq: 1 });
    await send;
    expect(resolved).toBe(true);
    expect(session.getStatus()).toBe("idle");
    expect(statuses).toEqual(["running", "idle"]);
  });

  it("rejects concurrent sendText with SESSION_BUSY", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);

    const first = session.sendText("one");
    const request = pageWindow.lastSent();
    respondOk(pageWindow, request);

    await expect(session.sendText("two")).rejects.toMatchObject({
      code: "SESSION_BUSY",
    });

    emitEvent(pageWindow, request, { type: "operation_completed", sessionId: "thread_1", seq: 1 });
    await first;
  });

  it("checks availability without probing Desktop when the extension is unavailable", async () => {
    delete (globalThis as any).chrome;
    const availability = await new Pedelec().checkAvailability();

    expect(availability).toEqual({
      available: false,
      extension: { available: false },
      approval: { approved: false, origin: "https://app.example.test" },
      desktop: { available: false, launchAttempted: false },
    });
  });

  it("does not probe Desktop before the origin is approved", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.checkAvailability();
    const approvalRequest = pageWindow.lastSent();
    expect(approvalRequest).toMatchObject({ type: "get_approval_status" });
    respondOk(pageWindow, approvalRequest, { installed: true, approved: false, origin: "https://app.example.test", appConnected: true });

    await expect(promise).resolves.toMatchObject({
      available: false,
      extension: { available: true },
      approval: { approved: false },
      desktop: { available: true, launchAttempted: true },
    });
    expect(requestMessages(pageWindow.port)).toHaveLength(1);
  });

  it("probes Desktop after approval and resolves Desktop failures", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.checkAvailability();
    const approvalRequest = pageWindow.lastSent();
    respondOk(pageWindow, approvalRequest, { installed: true, approved: true, origin: "https://app.example.test", appConnected: false });
    await nextTick();
    const settingsRequest = pageWindow.lastSent();
    expect(settingsRequest).toMatchObject({ type: "get_settings" });
    respondError(pageWindow, settingsRequest, "NATIVE_HOST_UNAVAILABLE");

    await expect(promise).resolves.toMatchObject({
      available: false,
      extension: { available: true },
      approval: { approved: true },
      desktop: { available: false, launchAttempted: true },
      error: { code: "NATIVE_HOST_UNAVAILABLE" },
    });
    expect(requestMessages(pageWindow.port).map((request) => request.type)).toEqual(["get_approval_status", "get_settings"]);
  });

  it("reports availability only when the settings probe succeeds", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.checkAvailability();
    respondOk(pageWindow, pageWindow.lastSent(), { installed: true, approved: true, origin: "https://app.example.test", appConnected: true });
    await nextTick();
    respondSettings(pageWindow, pageWindow.lastSent());

    await expect(promise).resolves.toEqual({
      available: true,
      extension: { available: true },
      approval: { approved: true, origin: "https://app.example.test" },
      desktop: { available: true, launchAttempted: true },
    });
  });

  it("normalizes approval query failures without probing Desktop", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.checkAvailability();
    respondError(pageWindow, pageWindow.lastSent(), "APPROVAL_STORAGE_ERROR");

    await expect(promise).resolves.toMatchObject({
      available: false,
      extension: { available: true },
      approval: { approved: false },
      desktop: { available: false, launchAttempted: false },
      error: { code: "APPROVAL_STORAGE_ERROR" },
    });
    expect(requestMessages(pageWindow.port)).toHaveLength(1);
  });

  it("prepare sends prepare_session and suppresses prepare chat output", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const deltas: string[] = [];
    const messages: string[] = [];
    session.onChatDelta((delta) => deltas.push(delta));
    session.onChat((message) => messages.push(message));

    const prepare = session.prepare();
    const request = pageWindow.lastSent();
    expect(request).toMatchObject({
      type: "prepare_session",
      sessionId: "thread_1",
    });
    respondOk(pageWindow, request);
    emitEvent(pageWindow, request, {
      type: "chat_delta",
      sessionId: "thread_1",
      seq: 1,
      text: "PEDELEC_",
    });
    emitEvent(pageWindow, request, {
      type: "chat_message",
      sessionId: "thread_1",
      seq: 2,
      text: "PEDELEC_PREPARED",
    });
    emitEvent(pageWindow, request, { type: "operation_completed", sessionId: "thread_1", seq: 3 });

    await prepare;
    expect(deltas).toEqual([]);
    expect(messages).toEqual([]);
    expect(session.getStatus()).toBe("idle");
  });

  it("coalesces in-flight prepare calls and skips later prepare after success", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);

    const first = session.prepare();
    const second = session.prepare();
    expect(second).toBe(first);
    const request = pageWindow.lastSent();
    expect(request).toMatchObject({ type: "prepare_session" });
    respondOk(pageWindow, request);
    emitEvent(pageWindow, request, { type: "operation_completed", sessionId: "thread_1", seq: 1 });
    await Promise.all([first, second]);

    const sentCount = pageWindow.port.sent.length;
    await session.prepare();
    expect(pageWindow.port.sent.length).toBe(sentCount);
  });

  it("sendText waits for in-flight prepare and falls back after prepare failure", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);

    const prepare = session.prepare();
    const prepareRequest = pageWindow.lastSent();
    const send = session.sendText("after prepare");
    expect(pageWindow.lastSent()).toBe(prepareRequest);

    respondError(pageWindow, prepareRequest, "PREPARE_FAILED");
    await expect(prepare).rejects.toMatchObject({ code: "PREPARE_FAILED" });
    await nextTick();

    const sendRequest = pageWindow.lastSent();
    expect(sendRequest).toMatchObject({
      type: "send_text",
      sessionId: "thread_1",
      text: "after prepare",
    });
    respondOk(pageWindow, sendRequest);
    emitEvent(pageWindow, sendRequest, { type: "operation_completed", sessionId: "thread_1", seq: 1 });
    await send;
  });

  it("prepare error followed by idle does not poison later sendText", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);

    const prepare = session.prepare();
    const prepareRequest = pageWindow.lastSent();
    emitEvent(pageWindow, prepareRequest, {
      type: "error",
      sessionId: "thread_1",
      seq: 1,
      error: { code: "PROVIDER_COMMAND_FAILED", message: "prepare failed" },
    });
    emitEvent(pageWindow, prepareRequest, {
      type: "status_changed",
      sessionId: "thread_1",
      seq: 2,
      status: "idle",
    });
    respondError(pageWindow, prepareRequest, "PROVIDER_COMMAND_FAILED");

    await expect(prepare).rejects.toMatchObject({ code: "PROVIDER_COMMAND_FAILED" });
    expect(session.getStatus()).toBe("idle");

    const send = session.sendText("hello");
    const sendRequest = pageWindow.lastSent();
    expect(sendRequest).toMatchObject({ type: "send_text", text: "hello" });
    respondOk(pageWindow, sendRequest);
    emitEvent(pageWindow, sendRequest, { type: "operation_completed", sessionId: "thread_1", seq: 3 });
    await send;
  });

  it("rejects prepare on an invalid acknowledgment before idle and suppresses prepare chat", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const deltas: string[] = [];
    const messages: string[] = [];
    session.onChatDelta((delta) => deltas.push(delta));
    session.onChat((message) => messages.push(message));

    const prepare = session.prepare();
    const request = pageWindow.lastSent();
    respondOk(pageWindow, request);
    emitEvent(pageWindow, request, {
      type: "chat_delta",
      sessionId: "thread_1",
      seq: 1,
      text: "Sure, PEDELEC_",
    });
    emitEvent(pageWindow, request, {
      type: "chat_message",
      sessionId: "thread_1",
      seq: 2,
      text: "Sure, PEDELEC_PREPARED",
    });
    emitEvent(pageWindow, request, {
      type: "error",
      sessionId: "thread_1",
      seq: 3,
      error: {
        code: "PREPARE_ACK_INVALID",
        message: "provider did not acknowledge session preparation",
        details: { assistantOutput: "Sure, PEDELEC_PREPARED" },
      },
    });
    emitEvent(pageWindow, request, {
      type: "status_changed",
      sessionId: "thread_1",
      seq: 4,
      status: "idle",
    });
    emitEvent(pageWindow, request, {
      type: "operation_completed",
      sessionId: "thread_1",
      seq: 5,
      operationId: request.operationId,
      operationKind: "prepare",
      success: false,
      error: {
        code: "PREPARE_ACK_INVALID",
        message: "provider did not acknowledge session preparation",
      },
    });

    await expect(prepare).rejects.toMatchObject({ code: "PREPARE_ACK_INVALID" });
    expect(deltas).toEqual([]);
    expect(messages).toEqual([]);
    expect(session.getStatus()).toBe("idle");
  });

  it("prepare handles tool calls and includes prepare turn context", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const contexts: ToolCallContext[] = [];
    session.onTool("get_app_state", (_args, ctx) => {
      contexts.push(ctx);
      return { ok: true };
    });

    const prepare = session.prepare();
    const request = pageWindow.lastSent();
    respondOk(pageWindow, request);
    emitEvent(pageWindow, request, {
      type: "tool_call",
      sessionId: "thread_1",
      seq: 1,
      toolRequestId: "tool_prepare",
      tool: "get_app_state",
      args: {},
    });
    await nextTick();
    expect(pageWindow.lastSent()).toMatchObject({
      type: "submit_tool_result",
      toolRequestId: "tool_prepare",
      result: { ok: true },
    });
    respondOk(pageWindow, pageWindow.lastSent());
    emitEvent(pageWindow, request, { type: "operation_completed", sessionId: "thread_1", seq: 2 });
    await prepare;
    expect(contexts[0]).toMatchObject({
      type: "tool_call",
      turnKind: "prepare",
    });
  });

  it("passes context metadata to completed chat and delta callbacks", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const chatTexts: string[] = [];
    const deltaTexts: string[] = [];
    const chatContexts: any[] = [];
    const deltaContexts: any[] = [];
    const statusContexts: any[] = [];
    session.onChat((text, ctx) => {
      chatTexts.push(text);
      chatContexts.push(ctx);
    });
    session.onChatDelta((text, ctx) => {
      deltaTexts.push(text);
      deltaContexts.push(ctx);
    });
    session.onStatus((_status, ctx) => statusContexts.push(ctx));

    const firstTurn = await startTurn(session, pageWindow);
    expect(statusContexts[0]).toMatchObject({
      type: "sdk_status_changed",
      source: "sdk",
      status: "running",
      previousStatus: "idle",
      sessionId: "thread_1",
      provider: "codex",
    });

    emitEvent(pageWindow, firstTurn.request, {
      type: "chat_delta",
      sessionId: "thread_1",
      seq: 1,
      text: "hel",
    });
    emitEvent(pageWindow, firstTurn.request, {
      type: "chat_message",
      sessionId: "thread_1",
      seq: 2,
      text: "hello",
    });
    emitEvent(pageWindow, firstTurn.request, { type: "operation_completed", sessionId: "thread_1", seq: 3 });
    await firstTurn.send;

    expect(deltaTexts).toEqual(["hel"]);
    expect(chatTexts).toEqual(["hello"]);
    const firstTurnId = statusContexts[0].turnId;
    expect(firstTurnId).toMatch(/^turn_/);
    expect(statusContexts[0].turnStartedAt).toEqual(expect.any(Number));
    expect(statusContexts[0].eventEmittedAt).toEqual(expect.any(Number));
    expect(deltaContexts[0]).toMatchObject({
      type: "chat_delta",
      source: "core",
      sessionId: "thread_1",
      turnId: firstTurnId,
      turnStartedAt: statusContexts[0].turnStartedAt,
      eventReceivedAt: expect.any(Number),
      eventEmittedAt: expect.any(Number),
    });
    expect(chatContexts[0]).toMatchObject({
      type: "chat_message",
      source: "core",
      sessionId: "thread_1",
      turnId: firstTurnId,
      turnStartedAt: statusContexts[0].turnStartedAt,
      eventReceivedAt: expect.any(Number),
      eventEmittedAt: expect.any(Number),
    });
    expect(statusContexts.at(-1)).toMatchObject({
      type: "status_changed",
      source: "core",
      status: "idle",
      previousStatus: "running",
      turnId: firstTurnId,
      turnStartedAt: statusContexts[0].turnStartedAt,
    });
    expect("seq" in deltaContexts[0]).toBe(false);
    expect("seq" in chatContexts[0]).toBe(false);

    const secondTurn = await startTurn(session, pageWindow);
    emitEvent(pageWindow, secondTurn.request, { type: "operation_completed", sessionId: "thread_1", seq: 4 });
    await secondTurn.send;
    expect(statusContexts.at(-2).turnId).not.toBe(firstTurnId);
  });

  it("routes both chat event types to the matching session and drops duplicate seq", async () => {
    const pedelec = new Pedelec();
    const { session: first } = await createProviderSession(pedelec, pageWindow, "codex", "thread_1");
    const { session: second, createRequest } = await createProviderSession(pedelec, pageWindow, "antigravity", "thread_2");
    const channelId = createRequest.channelId;
    const firstDeltas: string[] = [];
    const secondDeltas: string[] = [];
    const firstMessages: string[] = [];
    const secondMessages: string[] = [];
    first.onChatDelta((text) => firstDeltas.push(text));
    second.onChatDelta((text) => secondDeltas.push(text));
    first.onChat((text) => firstMessages.push(text));
    second.onChat((text) => secondMessages.push(text));

    const firstTurn = await startTurn(first, pageWindow);
    const secondTurn = await startTurn(second, pageWindow);
    pageWindow.emitFromExtension({ source: "pedelec-sdk-extension", channelId, type: "chat_delta", sessionId: "thread_1", seq: 1, operationId: firstTurn.request.operationId, text: "a" });
    pageWindow.emitFromExtension({ source: "pedelec-sdk-extension", channelId, type: "chat_delta", sessionId: "thread_2", seq: 1, operationId: secondTurn.request.operationId, text: "b" });
    pageWindow.emitFromExtension({ source: "pedelec-sdk-extension", channelId, type: "chat_delta", sessionId: "thread_1", seq: 1, operationId: firstTurn.request.operationId, text: "duplicate delta" });
    pageWindow.emitFromExtension({ source: "pedelec-sdk-extension", channelId, type: "chat_message", sessionId: "thread_1", seq: 2, operationId: firstTurn.request.operationId, text: "alpha" });
    pageWindow.emitFromExtension({ source: "pedelec-sdk-extension", channelId, type: "chat_message", sessionId: "thread_2", seq: 2, operationId: secondTurn.request.operationId, text: "beta" });
    pageWindow.emitFromExtension({ source: "pedelec-sdk-extension", channelId, type: "chat_message", sessionId: "thread_1", seq: 2, operationId: firstTurn.request.operationId, text: "duplicate message" });

    expect(firstDeltas).toEqual(["a"]);
    expect(secondDeltas).toEqual(["b"]);
    expect(firstMessages).toEqual(["alpha"]);
    expect(secondMessages).toEqual(["beta"]);

    emitEvent(pageWindow, firstTurn.request, { type: "operation_completed", sessionId: "thread_1", seq: 3 });
    emitEvent(pageWindow, secondTurn.request, { type: "operation_completed", sessionId: "thread_2", seq: 3 });
    await firstTurn.send;
    await secondTurn.send;
  });

  it("ignores messages from another source or channel", async () => {
    const pedelec = new Pedelec();
    const { session, createRequest } = await createProviderSession(pedelec, pageWindow);
    const text: string[] = [];
    session.onChatDelta((delta) => text.push(delta));

    pageWindow.emitFromOtherSource({
      source: "pedelec-sdk-extension",
      channelId: createRequest.channelId,
      type: "chat_delta",
      sessionId: "thread_1",
      seq: 1,
      text: "wrong source",
    });
    pageWindow.emitFromExtension({
      source: "pedelec-sdk-extension",
      channelId: "other_channel",
      type: "chat_delta",
      sessionId: "thread_1",
      seq: 2,
      text: "wrong channel",
    });

    expect(text).toEqual([]);
  });

  it("normalizes skills, registers inline handlers, and lets named onTool override them", async () => {
    const pedelec = new Pedelec();
    const inlineContexts: ToolCallContext[] = [];
    const namedContexts: ToolCallContext[] = [];
    const create = pedelec.createSession({
      provider: "codex",
      skills: {
        guidance: "Use update_counter.",
        tools: [
          defineTool({
            name: "update_counter",
            description: "Update counter.",
            argsSchema: {
              type: "object",
              properties: { delta: { type: "number" } },
              required: ["delta"],
            },
            timeoutMs: 3000,
            handler: (args: any, ctx) => {
              inlineContexts.push(ctx);
              return { source: "inline", delta: args.delta };
            },
          }),
        ],
      },
    });
    const createRequest = pageWindow.lastSent();
    expect(createRequest.input.skills).toEqual({
      guidance: "Use update_counter.",
      tools: [
        {
          name: "update_counter",
          description: "Update counter.",
          argsSchema: {
            type: "object",
            properties: { delta: { type: "number" } },
            required: ["delta"],
          },
          timeoutMs: 3000,
        },
      ],
    });
    expect(JSON.stringify(createRequest.input.skills)).not.toContain("handler");
    respondOk(pageWindow, createRequest, { sessionId: "thread_skills" });
    const session = await create;
    const disposeOverride = session.onTool("update_counter", (args: any, ctx) => {
      namedContexts.push(ctx);
      return {
        source: "named",
        delta: args.delta,
      };
    });

    const turn = await startTurn(session, pageWindow);
    emitEvent(pageWindow, createRequest, {
      type: "tool_call",
      sessionId: "thread_skills",
      seq: 1,
      operationId: turn.request.operationId,
      toolRequestId: "tool_override",
      tool: "update_counter",
      args: { delta: 2 },
    });
    await nextTick();
    expect(pageWindow.lastSent()).toMatchObject({
      type: "submit_tool_result",
      result: { source: "named", delta: 2 },
    });
    expect(namedContexts[0]).toMatchObject({
      type: "tool_call",
      source: "core",
      sessionId: "thread_skills",
      toolRequestId: "tool_override",
      tool: "update_counter",
      turnId: expect.stringMatching(/^turn_/),
      turnStartedAt: expect.any(Number),
      eventReceivedAt: expect.any(Number),
      eventEmittedAt: expect.any(Number),
    });
    expect("seq" in namedContexts[0]).toBe(false);
    respondOk(pageWindow, pageWindow.lastSent());

    disposeOverride();
    emitEvent(pageWindow, createRequest, {
      type: "tool_call",
      sessionId: "thread_skills",
      seq: 2,
      operationId: turn.request.operationId,
      toolRequestId: "tool_inline",
      tool: "update_counter",
      args: { delta: 3 },
    });
    await nextTick();
    expect(pageWindow.lastSent()).toMatchObject({
      type: "submit_tool_result",
      result: { source: "inline", delta: 3 },
    });
    expect(inlineContexts[0]).toMatchObject({
      type: "tool_call",
      source: "core",
      toolRequestId: "tool_inline",
      tool: "update_counter",
      turnId: namedContexts[0].turnId,
      turnStartedAt: namedContexts[0].turnStartedAt,
    });
    respondOk(pageWindow, pageWindow.lastSent());
    emitEvent(pageWindow, turn.request, { type: "operation_completed", sessionId: "thread_skills", seq: 3 });
    await turn.send;
  });

  it("rejects invalid skills at runtime", async () => {
    const pedelec = new Pedelec();
    const validBase = {
      guidance: "Use tools.",
      tools: [
        defineTool({
          name: "good_tool",
          description: "Good tool.",
          argsSchema: {
            type: "object",
            properties: {},
            required: [],
          },
        }),
      ],
    };

    await expect(
      pedelec.createSession({
        provider: "codex",
        skills: { ...validBase, tools: [{ ...validBase.tools[0], name: "bad/name" }] },
      } as any)
    ).rejects.toMatchObject({ code: "INVALID_INPUT" });
    await expect(
      pedelec.createSession({
        provider: "codex",
        skills: { ...validBase, tools: [validBase.tools[0], validBase.tools[0]] },
      } as any)
    ).rejects.toMatchObject({ code: "INVALID_INPUT" });
    await expect(
      pedelec.createSession({
        provider: "codex",
        skills: { ...validBase, tools: [{ ...validBase.tools[0], timeoutMs: 0 }] },
      } as any)
    ).rejects.toMatchObject({ code: "INVALID_INPUT" });
  });

  it("deep clones argsSchema before sending the manifest", async () => {
    const pedelec = new Pedelec();
    const argsSchema = {
      type: "object",
      required: ["delta"],
      properties: {
        delta: {
          type: "number",
          description: "Original delta.",
        },
      },
    } as const;
    const create = pedelec.createSession({
      provider: "codex",
      skills: {
        guidance: "Use update_counter.",
        tools: [
          defineTool({
            name: "update_counter",
            description: "Update counter.",
            argsSchema,
          }),
        ],
      },
    });
    const createRequest = pageWindow.lastSent();
    (argsSchema.properties.delta as { description: string }).description = "Mutated delta.";

    expect(createRequest.input.skills.tools[0].argsSchema.properties.delta.description).toBe(
      "Original delta."
    );
    respondOk(pageWindow, createRequest, { sessionId: "thread_clone" });
    await create;
  });

  it("rejects legacy input tool definitions at runtime", async () => {
    const pedelec = new Pedelec();

    await expect(
      pedelec.createSession({
        provider: "codex",
        skills: {
          guidance: "Use legacy_tool.",
          tools: [
            {
              name: "legacy_tool",
              description: "Legacy tool.",
              input: { delta: "number" },
              argsSchema: {
                type: "object",
                properties: {},
                required: [],
              },
            },
          ],
        },
      } as any)
    ).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "tool input is no longer supported; use argsSchema",
    });
  });

  it("rejects missing, non-object, non-object-root, and non-serializable argsSchema values", async () => {
    const pedelec = new Pedelec();
    const circular: any = { type: "object" };
    circular.self = circular;

    const makeSkills = (argsSchema: unknown) => ({
      guidance: "Use bad_tool.",
      tools: [
        {
          name: "bad_tool",
          description: "Bad tool.",
          ...(argsSchema === undefined ? {} : { argsSchema }),
        },
      ],
    });

    await expect(
      pedelec.createSession({ provider: "codex", skills: makeSkills(undefined) } as any)
    ).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "tool argsSchema must be an object",
    });
    await expect(
      pedelec.createSession({ provider: "codex", skills: makeSkills([]) } as any)
    ).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "tool argsSchema must be an object",
    });
    await expect(
      pedelec.createSession({ provider: "codex", skills: makeSkills({ type: "array" }) } as any)
    ).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "tool argsSchema must describe an object",
    });
    await expect(
      pedelec.createSession({ provider: "codex", skills: makeSkills({ type: "object", value: 1n }) } as any)
    ).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "tool argsSchema must be serializable",
    });
    await expect(
      pedelec.createSession({ provider: "codex", skills: makeSkills(circular) } as any)
    ).rejects.toMatchObject({
      code: "INVALID_INPUT",
      message: "tool argsSchema must be serializable",
    });
  });

  it("submits async tool handler results", async () => {
    const pedelec = new Pedelec();
    const { session, createRequest } = await createProviderSession(pedelec, pageWindow);
    const channelId = createRequest.channelId;
    const contexts: ToolCallContext[] = [];
    session.onTool(async (tool, args, ctx) => {
      contexts.push(ctx);
      return { ok: true, tool, args };
    });

    const turn = await startTurn(session, pageWindow);
    pageWindow.emitFromExtension({
      source: "pedelec-sdk-extension",
      channelId,
      type: "tool_call",
      sessionId: "thread_1",
      seq: 1,
      operationId: turn.request.operationId,
      toolRequestId: "tool_1",
      tool: "get_current_page",
      args: { url: "https://example.test" },
    });
    await nextTick();

    expect(pageWindow.lastSent()).toMatchObject({
      type: "submit_tool_result",
      sessionId: "thread_1",
      toolRequestId: "tool_1",
      result: {
        ok: true,
        tool: "get_current_page",
        args: { url: "https://example.test" },
      },
    });
    expect(contexts[0]).toMatchObject({
      type: "tool_call",
      source: "core",
      sessionId: "thread_1",
      toolRequestId: "tool_1",
      tool: "get_current_page",
      turnId: expect.stringMatching(/^turn_/),
      turnStartedAt: expect.any(Number),
      eventReceivedAt: expect.any(Number),
      eventEmittedAt: expect.any(Number),
    });
    respondOk(pageWindow, pageWindow.lastSent());
    emitEvent(pageWindow, turn.request, { type: "operation_completed", sessionId: "thread_1", seq: 2 });
    await turn.send;
  });

  it("submits an error result when the tool handler is missing or throws", async () => {
    const pedelec = new Pedelec();
    const { session, createRequest } = await createProviderSession(pedelec, pageWindow);
    const channelId = createRequest.channelId;

    const turn = await startTurn(session, pageWindow);
    pageWindow.emitFromExtension({
      source: "pedelec-sdk-extension",
      channelId,
      type: "tool_call",
      sessionId: "thread_1",
      seq: 1,
      operationId: turn.request.operationId,
      toolRequestId: "tool_missing",
      tool: "missing",
      args: {},
    });
    await nextTick();
    expect(pageWindow.lastSent().result.error).toMatchObject({
      code: "TOOL_HANDLER_NOT_FOUND",
    });
    respondOk(pageWindow, pageWindow.lastSent());

    session.onTool(() => {
      throw new Error("boom");
    });
    pageWindow.emitFromExtension({
      source: "pedelec-sdk-extension",
      channelId,
      type: "tool_call",
      sessionId: "thread_1",
      seq: 2,
      operationId: turn.request.operationId,
      toolRequestId: "tool_throw",
      tool: "throws",
      args: {},
    });
    await nextTick();
    expect(pageWindow.lastSent().result.error).toMatchObject({
      code: "TOOL_HANDLER_ERROR",
      message: "boom",
    });
    respondOk(pageWindow, pageWindow.lastSent());
    emitEvent(pageWindow, turn.request, { type: "operation_completed", sessionId: "thread_1", seq: 3 });
    await turn.send;
  });

  it("blocks future sendText after ended", async () => {
    const pedelec = new Pedelec();
    const { session, createRequest } = await createProviderSession(pedelec, pageWindow);
    const channelId = createRequest.channelId;
    const ended: string[] = [];
    session.onEnded(() => ended.push("ended"));

    pageWindow.emitFromExtension({ source: "pedelec-sdk-extension", channelId, type: "ended", sessionId: "thread_1", seq: 1 });

    await expect(session.sendText("hello")).rejects.toMatchObject({
      code: "SESSION_ENDED",
    });
    expect(session.getStatus()).toBe("ended");
    expect(ended).toEqual(["ended"]);
  });

  it("fires onEnded once per transition into ended across repeated resume cycles", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    let endedCount = 0;
    session.onEnded(() => {
      endedCount += 1;
    });

    const firstEnd = session.end();
    const firstEndRequest = pageWindow.lastSent();
    respondOk(pageWindow, firstEndRequest);
    await firstEnd;
    expect(endedCount).toBe(1);

    const resume = session.resume();
    const resumeRequest = pageWindow.lastSent();
    emitSnapshot(pageWindow, resumeRequest, {
      threadId: session.sessionId,
      status: "idle",
      latestSeq: 2,
    });
    respondOk(pageWindow, resumeRequest, { sessionId: session.sessionId });
    await resume;
    expect(session.getStatus()).toBe("idle");

    const secondEnd = session.end();
    const secondEndRequest = pageWindow.lastSent();
    respondOk(pageWindow, secondEndRequest);
    await secondEnd;
    expect(endedCount).toBe(2);

    session.handleEvent({
      type: "ended",
      sessionId: session.sessionId,
      seq: 4,
    });
    expect(endedCount).toBe(2);
  });

  it("reactivates the same ended handle only after an authoritative idle snapshot", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const statuses: string[] = [];
    const chats: string[] = [];
    session.onStatus((status) => statuses.push(status));
    session.onChat((text) => chats.push(text));
    const prepared = session.prepare();
    const prepareRequest = pageWindow.lastSent();
    respondOk(pageWindow, prepareRequest);
    emitEvent(pageWindow, prepareRequest, {
      type: "operation_completed",
      sessionId: session.sessionId,
      seq: 1,
    });
    await prepared;
    const sessionCreatedAt = session.sessionCreatedAt;

    const end = session.end();
    const endRequest = pageWindow.lastSent();
    respondOk(pageWindow, endRequest);
    await end;

    const resume = session.resume();
    const resumeRequest = pageWindow.lastSent();
    expect(resumeRequest).toMatchObject({
      type: "reactivate_session",
      sessionId: session.sessionId,
      autoEndOnDisconnect: true,
    });
    expect(session.getStatus()).toBe("ended");

    emitSnapshot(pageWindow, resumeRequest, {
      threadId: session.sessionId,
      status: "idle",
      latestSeq: 3,
      usage: { totalTokens: 42 },
    });
    respondOk(pageWindow, resumeRequest, { sessionId: session.sessionId });
    await resume;

    expect(session.getStatus()).toBe("idle");
    expect(session.usage.totalTokens).toBe(42);
    expect(session.provider).toBe("codex");
    expect(session.effortLevel).toBe("default");
    expect(session.sessionCreatedAt).toBe(sessionCreatedAt);
    expect(statuses).toContain("ended");
    expect(statuses.at(-1)).toBe("idle");
    const sentBeforePreparedRetry = pageWindow.port.sent.length;
    await session.prepare();
    expect(pageWindow.port.sent.length).toBe(sentBeforePreparedRetry);
    expect(await session.resume()).toBeUndefined();
    const send = session.sendText("continue");
    const sendRequest = pageWindow.lastSent();
    respondOk(pageWindow, sendRequest);
    emitEvent(pageWindow, sendRequest, {
      type: "chat_message",
      sessionId: session.sessionId,
      operationId: sendRequest.operationId,
      seq: 4,
      text: "continued",
    });
    emitEvent(pageWindow, sendRequest, {
      type: "operation_completed",
      sessionId: session.sessionId,
      seq: 5,
    });
    await send;
    expect(chats).toEqual(["continued"]);
  });

  it("shares concurrent resume requests and clears the guard for a second cycle", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const firstEnd = session.end();
    respondOk(pageWindow, pageWindow.lastSent());
    await firstEnd;

    const first = session.resume();
    const firstResumeRequest = pageWindow.lastSent();
    expect(requestMessages(pageWindow.port).filter((request) => request.type === "reactivate_session")).toHaveLength(1);
    emitSnapshot(pageWindow, firstResumeRequest, {
      threadId: session.sessionId,
      status: "idle",
      latestSeq: 2,
    });
    const second = session.resume();
    expect(first).toBe(second);
    respondOk(pageWindow, firstResumeRequest, { sessionId: session.sessionId });
    await first;

    const secondEnd = session.end();
    respondOk(pageWindow, pageWindow.lastSent());
    await secondEnd;
    const third = session.resume();
    const secondResumeRequest = pageWindow.lastSent();
    expect(secondResumeRequest.type).toBe("reactivate_session");
    emitSnapshot(pageWindow, secondResumeRequest, {
      threadId: session.sessionId,
      status: "idle",
      latestSeq: 4,
    });
    respondOk(pageWindow, secondResumeRequest, { sessionId: session.sessionId });
    await third;
    expect(session.getStatus()).toBe("idle");
  });

  it("rejects deterministic reactivation failures without falsely setting idle", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const end = session.end();
    respondOk(pageWindow, pageWindow.lastSent());
    await end;

    const resume = session.resume();
    const request = pageWindow.lastSent();
    respondError(pageWindow, request, "WORKSPACE_OPEN_FAILED");
    await expect(resume).rejects.toMatchObject({ code: "WORKSPACE_OPEN_FAILED" });
    expect(session.getStatus()).toBe("ended");

    const retry = session.resume();
    expect(pageWindow.lastSent().type).toBe("reactivate_session");
    respondError(pageWindow, pageWindow.lastSent(), "WORKSPACE_OPEN_FAILED");
    await expect(retry).rejects.toMatchObject({ code: "WORKSPACE_OPEN_FAILED" });
  });

  it("passes context metadata to error and ended callbacks", async () => {
    const pedelec = new Pedelec();
    const { session, createRequest } = await createProviderSession(pedelec, pageWindow);
    const errorContexts: any[] = [];
    const endedContexts: any[] = [];
    session.onError((_error, ctx) => errorContexts.push(ctx));
    session.onEnded((ctx) => endedContexts.push(ctx));

    const turn = await startTurn(session, pageWindow);
    emitEvent(pageWindow, turn.request, {
      type: "error",
      sessionId: "thread_1",
      seq: 1,
      operationId: turn.request.operationId,
      error: { code: "PROVIDER_ERROR", message: "provider failed" },
    });
    emitEvent(pageWindow, turn.request, {
      type: "operation_completed",
      sessionId: "thread_1",
      seq: 2,
      operationId: turn.request.operationId,
      operationKind: "user",
      success: false,
      error: { code: "PROVIDER_ERROR", message: "provider failed" },
    });

    await expect(turn.send).rejects.toMatchObject({ code: "PROVIDER_ERROR" });
    expect(errorContexts[0]).toMatchObject({
      type: "error",
      source: "core",
      sessionId: "thread_1",
      turnId: expect.stringMatching(/^turn_/),
      turnStartedAt: expect.any(Number),
      eventReceivedAt: expect.any(Number),
      eventEmittedAt: expect.any(Number),
    });

    pageWindow.emitFromExtension({
      source: "pedelec-sdk-extension",
      channelId: createRequest.channelId,
      type: "ended",
      sessionId: "thread_1",
      seq: 3,
    });

    expect(endedContexts[0]).toMatchObject({
      type: "ended",
      source: "core",
      sessionId: "thread_1",
      eventReceivedAt: expect.any(Number),
      eventEmittedAt: expect.any(Number),
    });
  });

  it("marks SDK local request failures with sdk_error context", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const contexts: any[] = [];
    session.onError((_error, ctx) => contexts.push(ctx));

    const send = expect(session.sendText("hello")).rejects.toMatchObject({ code: "THREAD_BUSY" });
    respondError(pageWindow, pageWindow.lastSent(), "THREAD_BUSY");

    await send;
    expect(contexts[0]).toMatchObject({
      type: "sdk_error",
      source: "sdk",
      sessionId: "thread_1",
      turnId: expect.stringMatching(/^turn_/),
      turnStartedAt: expect.any(Number),
      eventEmittedAt: expect.any(Number),
    });
    expect(contexts[0].eventReceivedAt).toBeUndefined();
  });

  it("marks local end() callbacks with sdk_ended context", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const contexts: any[] = [];
    session.onEnded((ctx) => contexts.push(ctx));

    const end = session.end();
    respondOk(pageWindow, pageWindow.lastSent());
    await end;

    expect(contexts[0]).toMatchObject({
      type: "sdk_ended",
      source: "sdk",
      sessionId: "thread_1",
      eventEmittedAt: expect.any(Number),
    });
    expect(contexts[0].turnId).toBeUndefined();
  });

  it("rejects pending sends and fires onError on extension disconnect", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const errors: string[] = [];
    session.onError((error) => errors.push(error.code));

    const send = session.sendText("hello");
    const request = pageWindow.lastSent();
    respondOk(pageWindow, request);
    pageWindow.port.disconnect();

    await expect(send).rejects.toMatchObject({
      code: "EXTENSION_DISCONNECTED",
    });
    expect(errors).toEqual(["EXTENSION_DISCONNECTED"]);
  });

  it("rejects request failures and emits onError", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const errors: string[] = [];
    session.onError((error) => errors.push(error.code));

    const send = expect(session.sendText("hello")).rejects.toMatchObject({ code: "THREAD_BUSY" });
    respondError(pageWindow, pageWindow.lastSent(), "THREAD_BUSY");

    await send;
    expect(errors).toEqual(["THREAD_BUSY"]);
  });

  it("handles session error events before send_text response", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const send = expect(session.sendText("hello")).rejects.toMatchObject({
      code: "PROVIDER_ERROR",
    });
    const request = pageWindow.lastSent();

    emitEvent(pageWindow, request, {
      type: "error",
      sessionId: "thread_1",
      seq: 1,
      operationId: request.operationId,
      error: { code: "PROVIDER_ERROR", message: "provider failed" },
    });
    respondOk(pageWindow, request);
    emitEvent(pageWindow, request, {
      type: "operation_completed",
      sessionId: "thread_1",
      seq: 2,
      operationId: request.operationId,
      operationKind: "user",
      success: false,
      error: { code: "PROVIDER_ERROR", message: "provider failed" },
    });

    await send;
  });

  it("settles only from the matching operation completion and ignores stale tails", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);

    const first = session.sendText("first");
    const firstRequest = pageWindow.lastSent();
    respondOk(pageWindow, firstRequest);
    emitEvent(pageWindow, firstRequest, {
      type: "status_changed",
      sessionId: "thread_1",
      seq: 1,
      status: "idle",
    });
    let firstSettled = false;
    void first.then(() => { firstSettled = true; });
    await nextTick();
    expect(firstSettled).toBe(false);

    emitEvent(pageWindow, firstRequest, {
      type: "operation_completed",
      sessionId: "thread_1",
      seq: 2,
      operationKind: "user",
      success: true,
    });
    await first;

    const second = session.sendText("second");
    const secondRequest = pageWindow.lastSent();
    respondOk(pageWindow, secondRequest);
    emitEvent(pageWindow, firstRequest, {
      type: "status_changed",
      sessionId: "thread_1",
      seq: 3,
      operationId: firstRequest.operationId,
      status: "idle",
    });
    emitEvent(pageWindow, firstRequest, {
      type: "operation_completed",
      sessionId: "thread_1",
      seq: 4,
      operationId: firstRequest.operationId,
      operationKind: "user",
      success: true,
    });
    let secondSettled = false;
    void second.then(() => { secondSettled = true; });
    await nextTick();
    expect(secondSettled).toBe(false);

    emitEvent(pageWindow, secondRequest, {
      type: "operation_completed",
      sessionId: "thread_1",
      seq: 5,
      operationKind: "user",
      success: true,
    });
    await second;
  });

  it("keeps an operation reserved when completion arrives before its request response", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const send = session.sendText("hello");
    const request = pageWindow.lastSent();

    emitEvent(pageWindow, request, {
      type: "operation_completed",
      sessionId: "thread_1",
      seq: 1,
      operationKind: "user",
      success: true,
    });
    let settled = false;
    void send.then(() => { settled = true; });
    await nextTick();
    expect(settled).toBe(false);

    respondOk(pageWindow, request);
    await send;
  });

  it("hydrates resumeSession status and recovers a pending tool from the snapshot", async () => {
    const pedelec = new Pedelec();
    const resume = pedelec.resumeSession("thread_resume_active");
    const request = pageWindow.lastSent();
    emitSnapshot(pageWindow, request, {
      threadId: "thread_resume_active",
      status: "waitingToolResult",
      latestSeq: 12,
      activeOperation: {
        operationId: "core-operation",
        operationKind: "user",
        startedAt: new Date().toISOString(),
      },
      pendingToolRequest: {
        requestId: "tool-recovered",
        threadId: "thread_resume_active",
        operationId: "core-operation",
        toolName: "get_app_state",
        args: {},
        createdAt: new Date().toISOString(),
        timeoutMs: 30_000,
      },
    });
    respondOk(pageWindow, request, { sessionId: "thread_resume_active" });
    const session = await resume;
    expect(session.getStatus()).toBe("waiting_tool_result");

    session.onTool("get_app_state", () => ({ recovered: true }));
    await nextTick();
    const toolResultRequest = pageWindow.lastSent();
    expect(toolResultRequest).toMatchObject({
      type: "submit_tool_result",
      sessionId: "thread_resume_active",
      toolRequestId: "tool-recovered",
      result: { recovered: true },
    });
    respondOk(pageWindow, toolResultRequest);
  });

  it("retires an external active operation when a later snapshot is idle", async () => {
    const pedelec = new Pedelec();
    const resume = pedelec.resumeSession("thread_external_complete");
    const resumeRequest = pageWindow.lastSent();
    emitSnapshot(pageWindow, resumeRequest, {
      threadId: "thread_external_complete",
      status: "running",
      latestSeq: 1,
      activeOperation: {
        operationId: "operation-a",
        operationKind: "user",
        startedAt: new Date().toISOString(),
      },
    });
    respondOk(pageWindow, resumeRequest, { sessionId: "thread_external_complete" });
    const session = await resume;
    expect(session.getStatus()).toBe("running");

    emitSnapshot(pageWindow, resumeRequest, {
      threadId: "thread_external_complete",
      status: "idle",
      latestSeq: 2,
      lastCompletedOperation: {
        operationId: "operation-a",
        operationKind: "user",
        success: true,
        completedAt: new Date().toISOString(),
      },
    });
    expect(session.getStatus()).toBe("idle");

    const send = session.sendText("after recovery");
    const sendRequest = pageWindow.lastSent();
    expect(sendRequest).toMatchObject({ type: "send_text", text: "after recovery" });
    respondOk(pageWindow, sendRequest);
    emitEvent(pageWindow, sendRequest, {
      type: "operation_completed",
      sessionId: "thread_external_complete",
      seq: 3,
      success: true,
    });
    await send;
  });

  it("prunes an obsolete recovered tool before a handler is registered", async () => {
    const pedelec = new Pedelec();
    const resume = pedelec.resumeSession("thread_obsolete_tool");
    const resumeRequest = pageWindow.lastSent();
    emitSnapshot(pageWindow, resumeRequest, {
      threadId: "thread_obsolete_tool",
      status: "waitingToolResult",
      latestSeq: 1,
      activeOperation: {
        operationId: "operation-a",
        operationKind: "user",
        startedAt: new Date().toISOString(),
      },
      pendingToolRequest: {
        requestId: "tool-stale",
        threadId: "thread_obsolete_tool",
        operationId: "operation-a",
        toolName: "get_state",
        args: {},
        createdAt: new Date().toISOString(),
        timeoutMs: 30_000,
      },
    });
    respondOk(pageWindow, resumeRequest, { sessionId: "thread_obsolete_tool" });
    const session = await resume;

    emitSnapshot(pageWindow, resumeRequest, {
      threadId: "thread_obsolete_tool",
      status: "idle",
      latestSeq: 2,
      lastCompletedOperation: {
        operationId: "operation-a",
        operationKind: "user",
        success: true,
        completedAt: new Date().toISOString(),
      },
    });
    const sentBeforeHandler = requestMessages(pageWindow.port).length;
    session.onTool("get_state", () => ({ stale: true }));
    await nextTick();
    expect(requestMessages(pageWindow.port).length).toBe(sentBeforeHandler);
    expect(requestMessages(pageWindow.port).some((request) => request.type === "submit_tool_result")).toBe(false);
  });

  it("replaces an external operation and discards its recovered tool state", async () => {
    const pedelec = new Pedelec();
    const resume = pedelec.resumeSession("thread_operation_replace");
    const resumeRequest = pageWindow.lastSent();
    const snapshotTool = (operationId: string, requestId: string, toolName: string, latestSeq: number) => ({
      threadId: "thread_operation_replace",
      status: "waitingToolResult",
      latestSeq,
      activeOperation: {
        operationId,
        operationKind: "user" as const,
        startedAt: new Date().toISOString(),
      },
      pendingToolRequest: {
        requestId,
        threadId: "thread_operation_replace",
        operationId,
        toolName,
        args: {},
        createdAt: new Date().toISOString(),
        timeoutMs: 30_000,
      },
    });
    emitSnapshot(pageWindow, resumeRequest, snapshotTool("operation-a", "tool-a", "tool_a", 1));
    respondOk(pageWindow, resumeRequest, { sessionId: "thread_operation_replace" });
    const session = await resume;

    emitSnapshot(pageWindow, resumeRequest, snapshotTool("operation-b", "tool-b", "tool_b", 2));
    session.onTool("tool_a", () => ({ stale: true }));
    session.onTool("tool_b", () => ({ current: true }));
    await nextTick();
    const toolResultRequest = pageWindow.lastSent();
    expect(toolResultRequest).toMatchObject({ type: "submit_tool_result", toolRequestId: "tool-b" });
    expect(requestMessages(pageWindow.port).some((request) => request.toolRequestId === "tool-a")).toBe(false);
    respondOk(pageWindow, toolResultRequest);
  });

  it("reconciles local operations only from matching snapshot branches", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);

    const preserved = session.sendText("preserved");
    const preservedRequest = pageWindow.lastSent();
    respondOk(pageWindow, preservedRequest);
    emitSnapshot(pageWindow, preservedRequest, {
      threadId: "thread_1",
      status: "running",
      latestSeq: 1,
      activeOperation: {
        operationId: preservedRequest.operationId,
        operationKind: "user",
        startedAt: new Date().toISOString(),
      },
    });
    expect(session.getStatus()).toBe("running");
    let preservedSettled = false;
    void preserved.then(() => { preservedSettled = true; });
    await nextTick();
    expect(preservedSettled).toBe(false);

    emitSnapshot(pageWindow, preservedRequest, {
      threadId: "thread_1",
      status: "idle",
      latestSeq: 2,
      lastCompletedOperation: {
        operationId: preservedRequest.operationId,
        operationKind: "user",
        success: true,
        completedAt: new Date().toISOString(),
      },
    });
    await preserved;

    const mismatched = session.sendText("mismatched");
    const mismatchedRequest = pageWindow.lastSent();
    respondOk(pageWindow, mismatchedRequest);
    emitSnapshot(pageWindow, mismatchedRequest, {
      threadId: "thread_1",
      status: "running",
      latestSeq: 3,
      activeOperation: {
        operationId: "unrelated-operation",
        operationKind: "user",
        startedAt: new Date().toISOString(),
      },
    });
    await expect(mismatched).rejects.toMatchObject({ code: "SDK_LIFECYCLE_SYNC_ERROR" });
  });

  it("resolves listProviders after a delayed backend readiness response", async () => {
    const pedelec = new Pedelec();
    const providersPromise = pedelec.listProviders();
    const request = pageWindow.lastSent();
    let settled = false;
    void providersPromise.finally(() => {
      settled = true;
    });

    await new Promise<void>((resolve) => setTimeout(resolve, 10));
    expect(settled).toBe(false);

    respondOk(pageWindow, request, [
      { name: "Codex", code: "codex", available: true, isDefault: true, error: null },
    ]);
    await expect(providersPromise).resolves.toEqual([
      { name: "Codex", code: "codex", available: true, isDefault: true, error: null },
    ]);
  });

  it("times out when the extension does not respond", async () => {
    const pedelec = new Pedelec({ bridgeTimeoutMs: 1 });
    const create = pedelec.createSession({ provider: "codex" });

    await expect(create).rejects.toMatchObject({
      code: "SDK_BRIDGE_TIMEOUT",
    });
  });

  it("workspaceFolderPicker sends the workspace picker request and normalizes an empty folder", async () => {
    const pedelec = new Pedelec();
    const picker = pedelec.workspaceFolderPicker();
    const request = pageWindow.lastSent();
    expect(request).toMatchObject({ type: "pick_workspace_folder" });
    respondOk(pageWindow, request, {
      path: "C:\\workspace\\project",
      isEmptyFolder: true,
      hasWorkspaceConfig: false,
      futureField: true,
    });

    await expect(picker).resolves.toEqual({
      path: "C:\\workspace\\project",
      isEmptyFolder: true,
      hasWorkspaceConfig: false,
    });
  });

  it("workspaceFolderPicker normalizes an existing workspace folder", async () => {
    const pedelec = new Pedelec();
    const picker = pedelec.workspaceFolderPicker();
    respondOk(pageWindow, pageWindow.lastSent(), {
      path: "C:\\workspace\\project",
      isEmptyFolder: false,
      hasWorkspaceConfig: true,
      futureField: true,
    });

    await expect(picker).resolves.toEqual({
      path: "C:\\workspace\\project",
      isEmptyFolder: false,
      hasWorkspaceConfig: true,
    });
  });

  it("workspaceFolderPicker resolves cancellation as null", async () => {
    const pedelec = new Pedelec();
    const picker = pedelec.workspaceFolderPicker();
    respondOk(pageWindow, pageWindow.lastSent(), { path: null });

    await expect(picker).resolves.toBeNull();
  });

  it("workspaceFolderPicker rejects malformed responses with SDK_PROTOCOL_ERROR", async () => {
    const pedelec = new Pedelec();
    for (const result of [
      {},
      { path: 123, isEmptyFolder: true, hasWorkspaceConfig: false },
      { path: "C:\\workspace\\project", hasWorkspaceConfig: false },
      { path: "C:\\workspace\\project", isEmptyFolder: "true", hasWorkspaceConfig: false },
      { path: "C:\\workspace\\project", isEmptyFolder: false, hasWorkspaceConfig: "true" },
      { path: {} , isEmptyFolder: false, hasWorkspaceConfig: false },
      { path: [], isEmptyFolder: false, hasWorkspaceConfig: false },
      { path: "", isEmptyFolder: false, hasWorkspaceConfig: false },
    ]) {
      const picker = pedelec.workspaceFolderPicker();
      respondOk(pageWindow, pageWindow.lastSent(), result);
      await expect(picker).rejects.toMatchObject({
        code: "SDK_PROTOCOL_ERROR",
      });
    }
  });

  it("workspaceFolderPicker does not use bridgeTimeoutMs while the picker is open", async () => {
    const pedelec = new Pedelec({ bridgeTimeoutMs: 1 });
    const picker = pedelec.workspaceFolderPicker();
    await new Promise((resolve) => setTimeout(resolve, 10));

    let settled = false;
    void picker.then(() => {
      settled = true;
    });
    expect(settled).toBe(false);
    respondOk(pageWindow, pageWindow.lastSent(), {
      path: "C:\\workspace\\project",
      isEmptyFolder: true,
      hasWorkspaceConfig: false,
    });
    await expect(picker).resolves.toMatchObject({ path: "C:\\workspace\\project" });
  });

  it("workspaceFolderPicker still rejects when its extension Port disconnects", async () => {
    const pedelec = new Pedelec({ bridgeTimeoutMs: 1 });
    const picker = pedelec.workspaceFolderPicker();
    pageWindow.port.disconnect();

    await expect(picker).rejects.toMatchObject({
      code: "EXTENSION_DISCONNECTED",
    });
  });

  it("lists assets without changing session state and validates the response", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const list = session.listAssets();
    const request = pageWindow.lastSent();
    expect(request).toMatchObject({ type: "list_assets", sessionId: "thread_1" });
    respondOk(pageWindow, request, { assets: [
      { name: "upl_file.txt", path: "/upl_file.txt", sizeBytes: 4, modifiedAt: 1, futureField: true },
      { name: "report.json", path: "/results/report.json", sizeBytes: 10, modifiedAt: 2 },
    ], futureField: true });
    await expect(list).resolves.toEqual([
      { name: "upl_file.txt", path: "/upl_file.txt", sizeBytes: 4, modifiedAt: 1 },
      { name: "report.json", path: "/results/report.json", sizeBytes: 10, modifiedAt: 2 },
    ]);
    expect(session.getStatus()).toBe("idle");

    for (const asset of [
      { name: "report.json", path: "/results/other.json", sizeBytes: 1, modifiedAt: 1 },
      { name: "report.json", path: "/results/../report.json", sizeBytes: 1, modifiedAt: 1 },
      { name: "report.json", path: "/results//report.json", sizeBytes: 1, modifiedAt: 1 },
      { name: "report.json", path: "/results\\report.json", sizeBytes: 1, modifiedAt: 1 },
      { name: "nested/report.json", path: "/results/report.json", sizeBytes: 1, modifiedAt: 1 },
    ]) {
      const invalid = session.listAssets();
      respondOk(pageWindow, pageWindow.lastSent(), { assets: [asset] });
      await expect(invalid).rejects.toMatchObject({ code: "SDK_PROTOCOL_ERROR", message: "list_assets response had an invalid shape" });
    }
  });

  it("rejects listAssets locally after session end", async () => {
    const pedelec = new Pedelec();
    const { session, createRequest } = await createProviderSession(pedelec, pageWindow);
    pageWindow.emitFromExtension({ source: "pedelec-sdk-extension", channelId: createRequest.channelId, type: "ended", sessionId: "thread_1", seq: 1 });
    const sent = pageWindow.port.sent.length;
    await expect(session.listAssets()).rejects.toMatchObject({ code: "SESSION_ENDED", details: { sessionId: "thread_1" } });
    expect(pageWindow.port.sent).toHaveLength(sent);
  });
});

