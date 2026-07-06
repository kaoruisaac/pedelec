import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { PEDELEC_EXTENSION_ID } from "./extension-id";
import { Pedelec, defineTool } from "./index";

type Listener<T> = (value: T) => void;

class MockWindow {
  location = { origin: "https://app.example.test" };
  port = new MockRuntimePort();
  connectCalls: Array<{ extensionId: string; connectInfo: { name: string } }> = [];

  postMessage(_message: any, _targetOrigin: string): void {
    throw new Error("window.postMessage should not be used by the SDK transport");
  }

  addEventListener(_type: string, _listener: Listener<MessageEvent>): void {
    throw new Error("window.addEventListener should not be used by the SDK transport");
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
  settings = { defaultProvider: null, defaultModels: {} }
): void {
  respondOk(pageWindow, request, settings);
}

function emitEvent(pageWindow: MockWindow, request: any, event: any): void {
  pageWindow.emitFromExtension({
    source: "pedelec-sdk-extension",
    channelId: request.channelId,
    ...event,
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
  const settingsRequest = pageWindow.lastSent();
  expect(settingsRequest).toMatchObject({ type: "get_settings" });
  respondSettings(pageWindow, settingsRequest);
  await nextTick();
  const createRequest = pageWindow.lastSent();
  expect(createRequest).toMatchObject({
    type: "create_session",
    input: { provider, skills: undefined },
  });
  respondOk(pageWindow, createRequest, { sessionId });
  return { session: await create, createRequest };
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

  it("posts runtime messages and creates a session", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.createSession({
      provider: "codex",
      model: "gpt-5",
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
        model: "gpt-5",
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
    expect(session.model).toBe("gpt-5");
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
    });
    await expect(promise).resolves.toEqual({
      installed: true,
      approved: true,
      origin: "https://app.example.test",
    });
  });

  it("returns unavailable approval status when the extension cannot connect", async () => {
    delete (globalThis as any).chrome;
    const pedelec = new Pedelec();

    await expect(pedelec.getApprovalStatus()).resolves.toEqual({
      installed: false,
      approved: false,
      origin: "https://app.example.test",
    });
  });

  it("creates an opencode session and lists providers", async () => {
    const pedelec = new Pedelec();
    const createPromise = pedelec.createSession({
      provider: "opencode",
      model: "ollama/qwen2.5-coder:14b",
    });
    const createRequest = pageWindow.lastSent();

    expect(createRequest).toMatchObject({
      type: "create_session",
      input: {
        provider: "opencode",
        model: "ollama/qwen2.5-coder:14b",
        skills: undefined,
      },
    });

    respondOk(pageWindow, createRequest, { sessionId: "thread_opencode" });
    const session = await createPromise;
    expect(session.provider).toBe("opencode");
    expect(session.model).toBe("ollama/qwen2.5-coder:14b");

    const listPromise = pedelec.listProviders();
    const listRequest = pageWindow.lastSent();
    expect(listRequest).toMatchObject({
      type: "list_providers",
    });

    respondOk(pageWindow, listRequest, [
      { name: "OpenCode", code: "opencode", path: null, available: false, error: "program was not found in PATH" },
    ]);
    await expect(listPromise).resolves.toEqual([
      { name: "OpenCode", code: "opencode", path: null, available: false, error: "program was not found in PATH" },
    ]);
  });

  it("creates a cursor session and lists cursor provider", async () => {
    const pedelec = new Pedelec();
    const createPromise = pedelec.createSession({
      provider: "cursor",
      model: "gpt-5",
    });
    const createRequest = pageWindow.lastSent();

    expect(createRequest).toMatchObject({
      type: "create_session",
      input: {
        provider: "cursor",
        model: "gpt-5",
        skills: undefined,
      },
    });

    respondOk(pageWindow, createRequest, { sessionId: "thread_cursor" });
    const session = await createPromise;
    expect(session.provider).toBe("cursor");
    expect(session.model).toBe("gpt-5");

    const listPromise = pedelec.listProviders();
    const listRequest = pageWindow.lastSent();
    respondOk(pageWindow, listRequest, [
      { name: "Cursor", code: "cursor", path: null, available: false, error: "program was not found in PATH" },
    ]);

    await expect(listPromise).resolves.toEqual([
      { name: "Cursor", code: "cursor", path: null, available: false, error: "program was not found in PATH" },
    ]);
  });

  it("creates a claude session and lists claude provider", async () => {
    const pedelec = new Pedelec();
    const createPromise = pedelec.createSession({
      provider: "claude",
      model: "sonnet",
    });
    const createRequest = pageWindow.lastSent();

    expect(createRequest).toMatchObject({
      type: "create_session",
      input: {
        provider: "claude",
        model: "sonnet",
        skills: undefined,
      },
    });

    respondOk(pageWindow, createRequest, { sessionId: "thread_claude" });
    const session = await createPromise;
    expect(session.provider).toBe("claude");
    expect(session.model).toBe("sonnet");

    const listPromise = pedelec.listProviders();
    const listRequest = pageWindow.lastSent();
    respondOk(pageWindow, listRequest, [
      { name: "Claude Code", code: "claude", path: null, available: false, error: "program was not found in PATH" },
    ]);

    await expect(listPromise).resolves.toEqual([
      { name: "Claude Code", code: "claude", path: null, available: false, error: "program was not found in PATH" },
    ]);
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
      defaultModels: { codex: "gpt-5" },
    });
    await expect(promise).resolves.toEqual({
      defaultProvider: "codex",
      defaultModels: { codex: "gpt-5" },
    });
  });

  it("rejects invalid settings shapes from the extension", async () => {
    const pedelec = new Pedelec();
    const legacy = pedelec.getSettings();
    respondOk(pageWindow, pageWindow.lastSent(), { defaultProvider: "codex", defaultModel: "gpt-5" });
    await expect(legacy).rejects.toMatchObject({ code: "SDK_PROTOCOL_ERROR" });

    const ollamaSettings = pedelec.getSettings();
    respondOk(pageWindow, pageWindow.lastSent(), {
      defaultProvider: "ollama",
      defaultModels: { ollama: "qwen" },
    });
    await expect(ollamaSettings).resolves.toEqual({
      defaultProvider: "ollama",
      defaultModels: { ollama: "qwen" },
    });

    const illegalKey = pedelec.getSettings();
    respondOk(pageWindow, pageWindow.lastSent(), {
      defaultProvider: "codex",
      defaultModels: { unknown: "qwen" },
    });
    await expect(illegalKey).rejects.toMatchObject({ code: "SDK_PROTOCOL_ERROR" });

    const nonStringValue = pedelec.getSettings();
    respondOk(pageWindow, pageWindow.lastSent(), {
      defaultProvider: "codex",
      defaultModels: { codex: 123 },
    });
    await expect(nonStringValue).rejects.toMatchObject({ code: "SDK_PROTOCOL_ERROR" });

    const emptyModels = pedelec.getSettings();
    respondOk(pageWindow, pageWindow.lastSent(), {
      defaultProvider: null,
      defaultModels: {},
    });
    await expect(emptyModels).resolves.toEqual({ defaultProvider: null, defaultModels: {} });
  });

  it("creates a session from default provider and model", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.createSession();

    const settingsRequest = pageWindow.lastSent();
    expect(settingsRequest).toMatchObject({ type: "get_settings" });
    respondOk(pageWindow, settingsRequest, {
      defaultProvider: "codex",
      defaultModels: { codex: "gpt-5" },
    });
    await nextTick();

    const providersRequest = pageWindow.lastSent();
    expect(providersRequest).toMatchObject({ type: "list_providers" });
    respondOk(pageWindow, providersRequest, [
      { name: "Codex", code: "codex", path: "/bin/codex", available: true, error: null },
    ]);
    await nextTick();

    const createRequest = pageWindow.lastSent();
    expect(createRequest).toMatchObject({
      type: "create_session",
      input: { provider: "codex", model: "gpt-5", skills: undefined },
    });
    respondOk(pageWindow, createRequest, { sessionId: "thread_default" });

    const session = await promise;
    expect(session.provider).toBe("codex");
    expect(session.model).toBe("gpt-5");
  });

  it("creates an ollama session with explicit model", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.createSession({
      provider: "ollama",
      model: "qwen3-14b-32k:latest",
    });
    const request = pageWindow.lastSent();

    expect(request).toMatchObject({
      type: "create_session",
      input: {
        provider: "ollama",
        model: "qwen3-14b-32k:latest",
        skills: undefined,
      },
    });
    respondOk(pageWindow, request, { sessionId: "thread_ollama" });

    const session = await promise;
    expect(session.provider).toBe("ollama");
    expect(session.model).toBe("qwen3-14b-32k:latest");
  });

  it("uses ollama as default provider with default model", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.createSession();

    respondOk(pageWindow, pageWindow.lastSent(), {
      defaultProvider: "ollama",
      defaultModels: { ollama: "qwen3-14b-32k:latest" },
    });
    await nextTick();
    respondOk(pageWindow, pageWindow.lastSent(), [
      { name: "Ollama", code: "ollama", path: "/bin/pedelec-agent", available: true, error: null },
    ]);
    await nextTick();

    const createRequest = pageWindow.lastSent();
    expect(createRequest).toMatchObject({
      type: "create_session",
      input: { provider: "ollama", model: "qwen3-14b-32k:latest", skills: undefined },
    });
    respondOk(pageWindow, createRequest, { sessionId: "thread_ollama_default" });

    const session = await promise;
    expect(session.provider).toBe("ollama");
    expect(session.model).toBe("qwen3-14b-32k:latest");
  });

  it("applies the selected provider's default model", async () => {
    const pedelec = new Pedelec();
    const codexPromise = pedelec.createSession({ provider: "codex" });
    respondOk(pageWindow, pageWindow.lastSent(), {
      defaultProvider: "codex",
      defaultModels: { codex: "gpt-5", gemini: "gemini-2.5-pro" },
    });
    await nextTick();
    const codexCreate = pageWindow.lastSent();
    expect(codexCreate).toMatchObject({
      type: "create_session",
      input: { provider: "codex", model: "gpt-5", skills: undefined },
    });
    respondOk(pageWindow, codexCreate, { sessionId: "thread_codex_default_model" });
    expect((await codexPromise).model).toBe("gpt-5");

    const geminiPromise = pedelec.createSession({ provider: "gemini" });
    respondOk(pageWindow, pageWindow.lastSent(), {
      defaultProvider: "codex",
      defaultModels: { codex: "gpt-5", gemini: "gemini-2.5-pro" },
    });
    await nextTick();
    const geminiCreate = pageWindow.lastSent();
    expect(geminiCreate).toMatchObject({
      type: "create_session",
      input: { provider: "gemini", model: "gemini-2.5-pro", skills: undefined },
    });
    respondOk(pageWindow, geminiCreate, { sessionId: "thread_gemini_no_default_model" });
    expect((await geminiPromise).model).toBe("gemini-2.5-pro");

    const ollamaPromise = pedelec.createSession({ provider: "ollama" });
    respondOk(pageWindow, pageWindow.lastSent(), {
      defaultProvider: "codex",
      defaultModels: { codex: "gpt-5", ollama: "qwen3-14b-32k:latest" },
    });
    await nextTick();
    const ollamaCreate = pageWindow.lastSent();
    expect(ollamaCreate).toMatchObject({
      type: "create_session",
      input: { provider: "ollama", model: "qwen3-14b-32k:latest", skills: undefined },
    });
    respondOk(pageWindow, ollamaCreate, { sessionId: "thread_ollama_default_model" });
    expect((await ollamaPromise).model).toBe("qwen3-14b-32k:latest");
  });

  it("omits model when the selected provider has no default model", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.createSession({ provider: "gemini" });
    respondOk(pageWindow, pageWindow.lastSent(), {
      defaultProvider: "codex",
      defaultModels: { codex: "gpt-5" },
    });
    await nextTick();

    const createRequest = pageWindow.lastSent();
    expect(createRequest).toMatchObject({
      type: "create_session",
      input: { provider: "gemini", skills: undefined },
    });
    expect(createRequest.input.model).toBeUndefined();
    respondOk(pageWindow, createRequest, { sessionId: "thread_gemini_no_model" });
    expect((await promise).model).toBeUndefined();
  });

  it("does not overwrite user supplied model with default model", async () => {
    const pedelec = new Pedelec();
    const promise = pedelec.createSession({ provider: "codex", model: "user-model" });
    const request = pageWindow.lastSent();

    expect(request).toMatchObject({
      type: "create_session",
      input: { provider: "codex", model: "user-model", skills: undefined },
    });
    respondOk(pageWindow, request, { sessionId: "thread_user_model" });

    expect((await promise).model).toBe("user-model");
  });

  it("sends explicit autoEndOnDisconnect lifecycle options", async () => {
    const pedelec = new Pedelec();
    const keepAlivePromise = pedelec.createSession({
      provider: "codex",
      model: "gpt-5",
      autoEndOnDisconnect: false,
    });
    const keepAliveRequest = pageWindow.lastSent();

    expect(keepAliveRequest).toMatchObject({
      type: "create_session",
      input: {
        provider: "codex",
        model: "gpt-5",
        skills: undefined,
        autoEndOnDisconnect: false,
      },
    });
    respondOk(pageWindow, keepAliveRequest, { sessionId: "thread_keep_alive" });
    await keepAlivePromise;

    const pageScopedPromise = pedelec.createSession({
      provider: "codex",
      model: "gpt-5",
      autoEndOnDisconnect: true,
    });
    const pageScopedRequest = pageWindow.lastSent();

    expect(pageScopedRequest).toMatchObject({
      type: "create_session",
      input: {
        provider: "codex",
        model: "gpt-5",
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
    respondOk(pageWindow, pageWindow.lastSent(), { defaultProvider: null, defaultModels: {} });
    await expect(missing).rejects.toMatchObject({ code: "DEFAULT_PROVIDER_NOT_SET" });

    const unavailable = pedelec.createSession();
    respondOk(pageWindow, pageWindow.lastSent(), { defaultProvider: "codex", defaultModels: {} });
    await nextTick();
    respondOk(pageWindow, pageWindow.lastSent(), [
      { name: "Codex", code: "codex", path: null, available: false, error: "missing" },
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

  it("resolves sendText only after done", async () => {
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

    emitEvent(pageWindow, request, { type: "done", sessionId: "thread_1", seq: 1 });
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

    emitEvent(pageWindow, request, { type: "done", sessionId: "thread_1", seq: 1 });
    await first;
  });

  it("routes chat deltas to the matching session and drops duplicate seq", async () => {
    const pedelec = new Pedelec();
    const { session: first } = await createProviderSession(pedelec, pageWindow, "codex", "thread_1");
    const { session: second, createRequest } = await createProviderSession(pedelec, pageWindow, "gemini", "thread_2");
    const channelId = createRequest.channelId;
    const firstText: string[] = [];
    const secondText: string[] = [];
    first.onChat((text) => firstText.push(text));
    second.onChat((text) => secondText.push(text));

    pageWindow.emitFromExtension({ source: "pedelec-sdk-extension", channelId, type: "chat_delta", sessionId: "thread_1", seq: 1, text: "a" });
    pageWindow.emitFromExtension({ source: "pedelec-sdk-extension", channelId, type: "chat_delta", sessionId: "thread_2", seq: 1, text: "b" });
    pageWindow.emitFromExtension({ source: "pedelec-sdk-extension", channelId, type: "chat_delta", sessionId: "thread_1", seq: 1, text: "duplicate" });

    expect(firstText).toEqual(["a"]);
    expect(secondText).toEqual(["b"]);
  });

  it("ignores messages from another source or channel", async () => {
    const pedelec = new Pedelec();
    const { session, createRequest } = await createProviderSession(pedelec, pageWindow);
    const text: string[] = [];
    session.onChat((delta) => text.push(delta));

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
            handler: (args: any) => ({ source: "inline", delta: args.delta }),
          }),
        ],
      },
    });
    respondSettings(pageWindow, pageWindow.lastSent());
    await nextTick();
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
    const disposeOverride = session.onTool("update_counter", (args: any) => ({
      source: "named",
      delta: args.delta,
    }));

    emitEvent(pageWindow, createRequest, {
      type: "tool_call",
      sessionId: "thread_skills",
      seq: 1,
      toolRequestId: "tool_override",
      tool: "update_counter",
      args: { delta: 2 },
    });
    await nextTick();
    expect(pageWindow.lastSent()).toMatchObject({
      type: "submit_tool_result",
      result: { source: "named", delta: 2 },
    });
    respondOk(pageWindow, pageWindow.lastSent());

    disposeOverride();
    emitEvent(pageWindow, createRequest, {
      type: "tool_call",
      sessionId: "thread_skills",
      seq: 2,
      toolRequestId: "tool_inline",
      tool: "update_counter",
      args: { delta: 3 },
    });
    await nextTick();
    expect(pageWindow.lastSent()).toMatchObject({
      type: "submit_tool_result",
      result: { source: "inline", delta: 3 },
    });
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
    respondSettings(pageWindow, pageWindow.lastSent());
    await nextTick();
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
    session.onTool(async (tool, args) => ({ ok: true, tool, args }));

    pageWindow.emitFromExtension({
      source: "pedelec-sdk-extension",
      channelId,
      type: "tool_call",
      sessionId: "thread_1",
      seq: 1,
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
  });

  it("submits an error result when the tool handler is missing or throws", async () => {
    const pedelec = new Pedelec();
    const { session, createRequest } = await createProviderSession(pedelec, pageWindow);
    const channelId = createRequest.channelId;

    pageWindow.emitFromExtension({
      source: "pedelec-sdk-extension",
      channelId,
      type: "tool_call",
      sessionId: "thread_1",
      seq: 1,
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
      toolRequestId: "tool_throw",
      tool: "throws",
      args: {},
    });
    await nextTick();
    expect(pageWindow.lastSent().result.error).toMatchObject({
      code: "TOOL_HANDLER_ERROR",
      message: "boom",
    });
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

  it("rejects pending sends and fires onError on extension disconnect", async () => {
    const pedelec = new Pedelec();
    const { session } = await createProviderSession(pedelec, pageWindow);
    const errors: string[] = [];
    session.onError((error) => errors.push(error.code));

    const send = session.sendText("hello");
    const request = pageWindow.lastSent();
    respondOk(pageWindow, request);
    pageWindow.emitFromExtension({
      source: "pedelec-sdk-extension",
      type: "error",
      error: { code: "EXTENSION_DISCONNECTED", message: "Pedelec extension disconnected." },
    });

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
      error: { code: "PROVIDER_ERROR", message: "provider failed" },
    });
    respondOk(pageWindow, request);

    await send;
  });

  it("times out when the extension does not respond", async () => {
    const pedelec = new Pedelec({ bridgeTimeoutMs: 1 });
    const create = pedelec.createSession({ provider: "codex" });

    await expect(create).rejects.toMatchObject({
      code: "SDK_BRIDGE_TIMEOUT",
    });
  });
});

