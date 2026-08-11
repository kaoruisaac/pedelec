const test = require("node:test");
const assert = require("node:assert/strict");
const { createBackground } = require("./background.js");

class MockPort {
  constructor() {
    this.sent = [];
    this.messageListeners = [];
    this.disconnectListeners = [];
    this.disconnectCount = 0;
    this.onMessage = {
      addListener: (listener) => this.messageListeners.push(listener),
    };
    this.onDisconnect = {
      addListener: (listener) => this.disconnectListeners.push(listener),
    };
  }

  postMessage(message) {
    this.sent.push(message);
  }

  emit(message) {
    for (const listener of this.messageListeners) listener(message);
  }

  disconnect() {
    this.disconnectCount += 1;
    for (const listener of this.disconnectListeners.slice()) listener();
  }
}

function createStorage(initial = {}) {
  const data = { ...initial };
  return {
    data,
    get(key, callback) {
      const result = key === null ? { ...data } : { [key]: data[key] };
      callback(result);
    },
    set(value, callback) {
      Object.assign(data, value);
      callback?.();
    },
    remove(key, callback) {
      delete data[key];
      callback?.();
    },
  };
}

function createChrome({ approved = true } = {}) {
  const externalListeners = [];
  const nativePorts = [];
  const nativePortQueue = [];
  const local = createStorage({
    approvedOrigins: approved ? [{ origin: "https://app.example.test", approvedAt: 1 }] : [],
  });
  const session = createStorage();
  const chrome = {
    nativePorts,
    nativePortQueue,
    openPopupCalls: 0,
    storage: { local, session },
    action: {
      openPopup: async () => {
        chrome.openPopupCalls += 1;
      },
    },
    i18n: { getMessage: () => "Unknown provider error" },
    tabs: { query: (_query, callback) => callback([]) },
    runtime: {
      lastError: null,
      onConnect: { addListener: () => {} },
      onConnectExternal: { addListener: (listener) => externalListeners.push(listener) },
      connectNative: () => {
        const port = nativePortQueue.shift() || new MockPort();
        nativePorts.push(port);
        return port;
      },
    },
    connectExternal(port) {
      for (const listener of externalListeners) listener(port);
    },
  };
  return chrome;
}

function flush() {
  return new Promise((resolve) => setImmediate(resolve));
}

async function waitFor(predicate, message = "condition was not reached") {
  for (let attempt = 0; attempt < 50; attempt += 1) {
    if (predicate()) return;
    await flush();
  }
  assert.fail(message);
}

function connectExternal(chrome, senderUrl = "https://app.example.test/page") {
  const port = new MockPort();
  port.name = "pedelec-sdk-external";
  port.sender = { url: senderUrl };
  chrome.connectExternal(port);
  return port;
}

async function respondToNative(background, nativePort, result, minimumMessageCount = 1) {
  await waitFor(() => nativePort.sent.length >= minimumMessageCount, "native request was not sent");
  const request = nativePort.sent.at(-1);
  background.handleNativeMessage({
    type: "response",
    requestId: request.requestId,
    ok: true,
    result,
  });
  await flush();
  return request;
}

async function createSdkSession(background, sdkPort, nativePort, sessionId, autoEndOnDisconnect = true) {
  sdkPort.emit({
    channelId: "channel_a",
    requestId: `create_${sessionId}`,
    type: "create_session",
    input: { provider: "codex", autoEndOnDisconnect },
  });
  await respondToNative(background, nativePort, { threadId: sessionId });
  await respondToNative(background, nativePort, {}, 2);
  await waitFor(() => sdkPort.sent.some((message) => message.requestId === `create_${sessionId}`));
  return sdkPort.sent.find((message) => message.requestId === `create_${sessionId}`);
}

test("create_session forwards an explicit sandbox without inventing a path", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();
  const sdk = connectExternal(chrome);

  sdk.emit({
    channelId: "channel_a",
    requestId: "create_custom",
    type: "create_session",
    input: {
      provider: "codex",
      sandbox: { path: "C:\\workspace\\project-a" },
      autoEndOnDisconnect: true,
    },
  });
  const nativeCreate = await respondToNative(background, native, { threadId: "thread_custom" });
  assert.deepEqual(nativeCreate.sandbox, { path: "C:\\workspace\\project-a" });

  await respondToNative(background, native, {}, 2);
  await waitFor(() => sdk.sent.some((message) => message.requestId === "create_custom"));

  const defaultSdk = connectExternal(chrome);
  defaultSdk.emit({
    channelId: "channel_b",
    requestId: "create_default",
    type: "create_session",
    input: { provider: "codex" },
  });
  const nativeDefault = await respondToNative(background, native, { threadId: "thread_default" }, 3);
  assert.equal(nativeDefault.sandbox, undefined);
  await respondToNative(background, native, {}, 4);
});

test("create_session forwards effortLevel and never forwards model", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();
  const sdk = connectExternal(chrome);

  sdk.emit({
    channelId: "channel_a",
    requestId: "create_effort",
    type: "create_session",
    input: { provider: "codex", effortLevel: "high", model: "should-not-forward" },
  });
  const nativeCreate = await respondToNative(background, native, { threadId: "thread_effort" });
  assert.equal(nativeCreate.effortLevel, "high");
  assert.equal(nativeCreate.model, undefined);
});

test("SDK settings projection strips provider settings and effort args", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();
  const sdk = connectExternal(chrome);

  sdk.emit({ channelId: "channel_a", requestId: "settings_projection", type: "get_settings" });
  const request = await respondToNative(background, native, {
    defaultProvider: "codex",
    providerSettings: { codex: { effortsArgs: { default: ["-m", "secret-model"] } } },
  });
  await waitFor(() => sdk.sent.some((message) => message.requestId === "settings_projection"));
  const response = sdk.sent.find((message) => message.requestId === "settings_projection");
  assert.deepEqual(response.result, { defaultProvider: "codex" });
  assert.equal(request.type, "get_settings");
});

test("native idle shutdown is preserved and the next SDK request reconnects lazily", async () => {
  const chrome = createChrome();
  const nativeA = new MockPort();
  const nativeB = new MockPort();
  chrome.nativePortQueue.push(nativeA, nativeB);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();
  const sdk = connectExternal(chrome);

  sdk.emit({ channelId: "channel_a", requestId: "approval_a", type: "get_approval_status" });
  await respondToNative(background, nativeA, { connected: true });
  await waitFor(() => sdk.sent.some((message) => message.requestId === "approval_a"));
  assert.equal(nativeA.disconnectCount, 1);
  assert.equal(background.getState().connected, false);

  sdk.emit({ channelId: "channel_a", requestId: "approval_b", type: "get_approval_status" });
  await respondToNative(background, nativeB, { connected: true });
  await waitFor(() => sdk.sent.some((message) => message.requestId === "approval_b"));
  assert.equal(chrome.nativePorts.length, 2);
});

test("a replacement external Port still enforces origin approval", async () => {
  const chrome = createChrome({ approved: false });
  const background = createBackground(chrome, { approvalTimeoutMs: 50, disableReconnect: true });
  background.start();

  const portA = connectExternal(chrome);
  portA.emit({ channelId: "channel_a", requestId: "settings_a", type: "get_settings" });
  await waitFor(() => background.getPendingApproval()?.origin === "https://app.example.test");
  assert.equal(chrome.nativePorts.length, 0);
  portA.disconnect();
  assert.equal(background.getPendingApproval(), null);

  const portB = connectExternal(chrome);
  portB.emit({ channelId: "channel_b", requestId: "settings_b", type: "get_settings" });
  await waitFor(() => background.getPendingApproval()?.requestCount === 1);
  assert.equal(chrome.nativePorts.length, 0);
  portB.disconnect();
});

test("disconnecting an SDK Port removes its route and auto-ends the default session", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();
  const sdk = connectExternal(chrome);

  await createSdkSession(background, sdk, native, "thread_auto_end", true);
  assert.equal(background.getSdkRouteCount(), 1);
  sdk.disconnect();
  await waitFor(() => native.sent.some((message) => message.type === "end_thread"));
  await respondToNative(background, native, {});
  await waitFor(() => background.getSdkRouteCount() === 0 && background.getActiveThreadCount() === 0);
});

test("autoEndOnDisconnect false keeps the native session but requires explicit resume", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();
  const portA = connectExternal(chrome);

  await createSdkSession(background, portA, native, "thread_resume", false);
  portA.disconnect();
  await flush();
  assert.equal(background.getSdkRouteCount(), 0);
  assert.equal(background.getActiveThreadCount(), 1);

  const portB = connectExternal(chrome);
  const nativeMessagesBeforeResume = native.sent.length;
  portB.emit({ channelId: "channel_b", requestId: "resume_1", type: "resume_session", sessionId: "thread_resume" });
  await respondToNative(background, native, {}, nativeMessagesBeforeResume + 1);
  await waitFor(() => portB.sent.some((message) => message.requestId === "resume_1"));
  assert.equal(background.getSdkRouteCount(), 1);

  background.dispatchSdkThreadEvent({ threadId: "thread_resume", type: "assistant_message", seq: 1, text: "resumed" });
  assert.deepEqual(portB.sent.at(-1), {
    channelId: "channel_b",
    sessionId: "thread_resume",
    seq: 1,
    type: "chat_delta",
    text: "resumed",
  });
});
