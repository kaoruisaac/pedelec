const assert = require("node:assert/strict");
const test = require("node:test");
const { createBackground } = require("./background.js");
const manifest = require("./manifest.json");

class MockPort {
  constructor(name) {
    this.name = name;
    this.sent = [];
    this.disconnectCallCount = 0;
    this.messageListeners = [];
    this.disconnectListeners = [];
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
    for (const listener of this.messageListeners) {
      listener(message);
    }
  }

  disconnect() {
    this.disconnectCallCount += 1;
    for (const listener of this.disconnectListeners) {
      listener();
    }
  }

  lastSent() {
    return this.sent.at(-1);
  }
}

function createChromeMock() {
  const nativePort = new MockPort("native");
  let connectNativeCallCount = 0;
  let onConnectListener = null;
  let onConnectExternalListener = null;
  let openPopupCallCount = 0;
  let activeTabUrl = "https://active.example.test/page";
  let activeTabId = 1;
  const storageData = {};
  const sessionStorageData = {};
  const chrome = {
    runtime: {
      lastError: null,
      onConnect: { addListener: (listener) => {
        onConnectListener = listener;
      } },
      onConnectExternal: { addListener: (listener) => {
        onConnectExternalListener = listener;
      } },
      connectNative: () => {
        connectNativeCallCount += 1;
        return nativePort;
      },
    },
    storage: {
      local: {
        get: (key, callback) => {
          const result = typeof key === "string" ? { [key]: storageData[key] } : { ...storageData };
          callback?.(result);
          return Promise.resolve(result);
        },
        set: (value, callback) => {
          Object.assign(storageData, value);
          callback?.();
          return Promise.resolve();
        },
      },
      session: {
        get: (key, callback) => {
          const result = typeof key === "string" ? { [key]: sessionStorageData[key] } : { ...sessionStorageData };
          callback?.(result);
          return Promise.resolve(result);
        },
        set: (value, callback) => {
          Object.assign(sessionStorageData, value);
          callback?.();
          return Promise.resolve();
        },
        remove: (key, callback) => {
          delete sessionStorageData[key];
          callback?.();
          return Promise.resolve();
        },
      },
    },
    action: {
      openPopup: () => {
        openPopupCallCount += 1;
        return Promise.resolve();
      },
    },
    tabs: {
      query: (_queryInfo, callback) => {
        const tabs = activeTabUrl ? [{ id: activeTabId, url: activeTabUrl }] : [];
        callback?.(tabs);
        return Promise.resolve(tabs);
      },
    },
  };
  return {
    chrome,
    nativePort,
    storageData,
    sessionStorageData,
    getConnectNativeCallCount: () => connectNativeCallCount,
    getOpenPopupCallCount: () => openPopupCallCount,
    setActiveTabUrl: (url) => {
      activeTabUrl = url;
    },
    setActiveTab: (id, url = activeTabUrl) => {
      activeTabId = id;
      activeTabUrl = url;
    },
    emitRuntimeConnect: (port) => onConnectListener?.(port),
    emitRuntimeExternalConnect: (port) => onConnectExternalListener?.(port),
  };
}

function respondNative(nativePort, request, result = {}) {
  nativePort.emit({
    type: "response",
    requestId: request.requestId,
    ok: true,
    result,
  });
}

function tick() {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

async function createSdkSession(background, nativePort, sessionId = "thread_sdk") {
  const sdkPort = new MockPort("pedelec-sdk-internal");
  background.handleSdkConnect(sdkPort);
  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create",
    input: { provider: "codex", skills: undefined },
  });
  assert.equal(nativePort.lastSent().type, "create_thread");
  respondNative(nativePort, nativePort.lastSent(), { threadId: sessionId });
  await tick();
  assert.equal(nativePort.lastSent().type, "subscribe_thread");
  respondNative(nativePort, nativePort.lastSent(), { subscribed: true });
  await tick();
  assert.deepEqual(sdkPort.lastSent(), {
    channelId: "channel_1",
    type: "response",
    requestId: "sdk_create",
    ok: true,
    result: { sessionId },
  });
  return sdkPort;
}

async function createExternalSdkSession(background, nativePort, storageData, {
  sessionId = "thread_sdk",
  origin = "https://example.test",
  tabId = 1,
} = {}) {
  storageData.approvedOrigins = [{ origin, approvedAt: 1 }];
  const sdkPort = createExternalSdkPort(origin, tabId);
  background.handleSdkExternalConnect(sdkPort);
  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create",
    input: { provider: "codex", skills: undefined },
  });
  respondNative(nativePort, nativePort.lastSent(), { threadId: sessionId });
  await tick();
  respondNative(nativePort, nativePort.lastSent(), { subscribed: true });
  await tick();
  return sdkPort;
}

function createExternalSdkPort(origin = "https://example.test", tabId = 1) {
  const port = new MockPort("pedelec-sdk-external");
  port.sender = { url: `${origin}/app`, tab: { id: tabId } };
  return port;
}

function createPopupPort(tabId = 1) {
  const port = new MockPort("popup");
  port.sender = { tab: { id: tabId } };
  return port;
}

test("manifest uses external connections without all-url content script injection", () => {
  assert.equal(manifest.content_scripts, undefined);
  assert.ok(manifest.permissions.includes("storage"));
  assert.deepEqual(manifest.externally_connectable.matches, [
    "https://*/*",
    "http://localhost/*",
    "http://127.0.0.1/*",
  ]);
});

test("background start does not connect native host", () => {
  const { chrome, getConnectNativeCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });

  background.start();

  assert.equal(getConnectNativeCallCount(), 0);
  assert.equal(background.getState().connected, false);
  assert.equal(background.getState().error, null);
});

test("SDK port connect does not connect native host", () => {
  const { chrome, getConnectNativeCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = new MockPort("pedelec-sdk-internal");

  background.handleSdkConnect(sdkPort);

  assert.equal(getConnectNativeCallCount(), 0);
  assert.equal(sdkPort.sent.length, 0);
});

test("external SDK port connect does not connect native host", () => {
  const { chrome, getConnectNativeCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = createExternalSdkPort();

  background.handleSdkExternalConnect(sdkPort);

  assert.equal(getConnectNativeCallCount(), 0);
  assert.equal(sdkPort.sent.length, 0);
});

test("unapproved external create_session opens approval without native host", async () => {
  const { chrome, getConnectNativeCallCount, getOpenPopupCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = createExternalSdkPort("https://app.example.test");
  const popupPort = new MockPort("popup");
  background.handlePopupConnect(popupPort);
  background.handleSdkExternalConnect(sdkPort);

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create",
    input: { provider: "codex", skills: undefined },
  });
  await tick();
  await tick();

  assert.equal(getConnectNativeCallCount(), 0);
  assert.equal(getOpenPopupCallCount(), 1);
  assert.deepEqual(background.getPendingApproval(), {
    origin: "https://app.example.test",
    requestedAt: background.getPendingApproval().requestedAt,
    requestCount: 1,
  });
  assert.deepEqual(popupPort.sent.findLast((message) => message.type === "approval_state").approvalState, {
    origin: "https://app.example.test",
    requestedAt: background.getPendingApproval().requestedAt,
    requestCount: 1,
    approved: false,
    pending: true,
  });
});

test("unapproved external resume_session opens approval without native host", async () => {
  const { chrome, getConnectNativeCallCount, getOpenPopupCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = createExternalSdkPort("https://app.example.test");
  background.handleSdkExternalConnect(sdkPort);

  sdkPort.emit({
    type: "resume_session",
    channelId: "channel_1",
    requestId: "sdk_resume",
    sessionId: "thread_existing",
  });
  await tick();

  assert.equal(getConnectNativeCallCount(), 0);
  assert.equal(getOpenPopupCallCount(), 1);
  assert.equal(background.getPendingApproval().origin, "https://app.example.test");
});

test("approved external create_session continues to native host", async () => {
  const { chrome, nativePort, storageData } = createChromeMock();
  storageData.approvedOrigins = [{ origin: "https://app.example.test", approvedAt: 1 }];
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = createExternalSdkPort("https://app.example.test");
  background.handleSdkExternalConnect(sdkPort);

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create",
    input: { provider: "codex", skills: undefined },
  });
  await tick();
  assert.equal(nativePort.lastSent().type, "create_thread");
  respondNative(nativePort, nativePort.lastSent(), { threadId: "thread_approved" });
  await tick();
  respondNative(nativePort, nativePort.lastSent(), { subscribed: true });
  await tick();

  assert.deepEqual(sdkPort.lastSent(), {
    channelId: "channel_1",
    type: "response",
    requestId: "sdk_create",
    ok: true,
    result: { sessionId: "thread_approved" },
  });
});

test("popup approve stores origin and resumes pending create_session", async () => {
  const { chrome, nativePort, storageData } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = createExternalSdkPort("https://app.example.test");
  const popupPort = new MockPort("popup");
  background.handleSdkExternalConnect(sdkPort);
  background.handlePopupConnect(popupPort);

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create",
    input: { provider: "codex", skills: undefined },
  });
  await tick();
  assert.equal(nativePort.sent.length, 0);

  popupPort.emit({ type: "approve_origin", origin: "https://app.example.test" });
  await tick();
  assert.equal(storageData.approvedOrigins[0].origin, "https://app.example.test");
  assert.equal(nativePort.lastSent().type, "create_thread");
  respondNative(nativePort, nativePort.lastSent(), { threadId: "thread_approved" });
  await tick();
  respondNative(nativePort, nativePort.lastSent(), { subscribed: true });
  await tick();

  assert.deepEqual(sdkPort.lastSent(), {
    channelId: "channel_1",
    type: "response",
    requestId: "sdk_create",
    ok: true,
    result: { sessionId: "thread_approved" },
  });
});

test("popup reject fails pending create_session", async () => {
  const { chrome, getConnectNativeCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = createExternalSdkPort("https://app.example.test");
  const popupPort = new MockPort("popup");
  background.handleSdkExternalConnect(sdkPort);
  background.handlePopupConnect(popupPort);

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create",
    input: { provider: "codex", skills: undefined },
  });
  await tick();
  popupPort.emit({ type: "reject_pending_approval" });
  await tick();

  assert.equal(getConnectNativeCallCount(), 0);
  assert.deepEqual(sdkPort.lastSent(), {
    channelId: "channel_1",
    type: "response",
    requestId: "sdk_create",
    ok: false,
    error: { code: "APPROVAL_REJECTED", message: "Pedelec approval was rejected." },
  });
});

test("popup close fails pending create_session", async () => {
  const { chrome, getConnectNativeCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = createExternalSdkPort("https://app.example.test");
  const popupPort = new MockPort("popup");
  background.handleSdkExternalConnect(sdkPort);
  background.handlePopupConnect(popupPort);

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create",
    input: { provider: "codex", skills: undefined },
  });
  await tick();
  popupPort.disconnect();
  await tick();

  assert.equal(getConnectNativeCallCount(), 0);
  assert.equal(sdkPort.lastSent().ok, false);
  assert.equal(sdkPort.lastSent().error.code, "APPROVAL_REJECTED");
});

test("approval timeout fails pending create_session", async () => {
  const { chrome, getConnectNativeCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true, approvalTimeoutMs: 1 });
  const sdkPort = createExternalSdkPort("https://app.example.test");
  background.handleSdkExternalConnect(sdkPort);

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create",
    input: { provider: "codex", skills: undefined },
  });
  await new Promise((resolve) => setTimeout(resolve, 5));

  assert.equal(getConnectNativeCallCount(), 0);
  assert.equal(sdkPort.lastSent().ok, false);
  assert.equal(sdkPort.lastSent().error.code, "APPROVAL_TIMEOUT");
});

test("openPopup failure fails pending create_session", async () => {
  const { chrome, getConnectNativeCallCount } = createChromeMock();
  chrome.action.openPopup = () => Promise.reject(new Error("blocked"));
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = createExternalSdkPort("https://app.example.test");
  background.handleSdkExternalConnect(sdkPort);

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create",
    input: { provider: "codex", skills: undefined },
  });
  await tick();

  assert.equal(getConnectNativeCallCount(), 0);
  assert.equal(sdkPort.lastSent().ok, false);
  assert.equal(sdkPort.lastSent().error.code, "OPEN_POPUP_FAILED");
});

test("pending approval for one origin does not approve another origin", async () => {
  const { chrome, getConnectNativeCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const firstPort = createExternalSdkPort("https://a.example.test");
  const secondPort = createExternalSdkPort("https://b.example.test");
  background.handleSdkExternalConnect(firstPort);
  background.handleSdkExternalConnect(secondPort);

  firstPort.emit({
    type: "create_session",
    channelId: "channel_a",
    requestId: "sdk_create_a",
    input: { provider: "codex", skills: undefined },
  });
  await tick();
  secondPort.emit({
    type: "create_session",
    channelId: "channel_b",
    requestId: "sdk_create_b",
    input: { provider: "codex", skills: undefined },
  });
  await tick();

  assert.equal(getConnectNativeCallCount(), 0);
  assert.equal(secondPort.lastSent().ok, false);
  assert.equal(secondPort.lastSent().error.code, "CREATE_SESSION_NOT_APPROVED");
});

test("get_approval_status does not connect native host", async () => {
  const { chrome, storageData, getConnectNativeCallCount } = createChromeMock();
  storageData.approvedOrigins = [{ origin: "https://app.example.test", approvedAt: 1 }];
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = createExternalSdkPort("https://app.example.test");
  background.handleSdkExternalConnect(sdkPort);

  sdkPort.emit({
    type: "get_approval_status",
    channelId: "channel_1",
    requestId: "sdk_approval",
  });
  await tick();

  assert.equal(getConnectNativeCallCount(), 0);
  assert.deepEqual(sdkPort.lastSent(), {
    channelId: "channel_1",
    type: "response",
    requestId: "sdk_approval",
    ok: true,
    result: {
      installed: true,
      approved: true,
      origin: "https://app.example.test",
    },
  });
});

test("popup connect and get_state do not connect native host", async () => {
  const { chrome, getConnectNativeCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const popupPort = new MockPort("popup");

  background.handlePopupConnect(popupPort);
  popupPort.emit({ type: "get_state" });
  await tick();

  assert.equal(getConnectNativeCallCount(), 0);
  assert.equal(popupPort.sent.filter((message) => message.type === "state").length, 2);
  assert.equal(popupPort.sent.findLast((message) => message.type === "state").state.connected, false);
});

test("popup approval state returns active tab origin when no pending exists", async () => {
  const { chrome } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const popupPort = new MockPort("popup");

  background.handlePopupConnect(popupPort);
  popupPort.emit({ type: "get_popup_approval_state" });
  await tick();

  assert.deepEqual(popupPort.sent.findLast((message) => message.type === "approval_state"), {
    type: "approval_state",
    approvalState: {
      origin: "https://active.example.test",
      approved: false,
      pending: false,
    },
  });
});

test("popup approval state returns approved active tab origin", async () => {
  const { chrome, storageData } = createChromeMock();
  storageData.approvedOrigins = [{ origin: "https://active.example.test", approvedAt: 1 }];
  const background = createBackground(chrome, { disableReconnect: true });
  const popupPort = new MockPort("popup");

  background.handlePopupConnect(popupPort);
  popupPort.emit({ type: "get_popup_approval_state" });
  await tick();

  assert.equal(popupPort.sent.findLast((message) => message.type === "approval_state").approvalState.approved, true);
});

test("popup approval state ignores unsupported active tab URLs", async () => {
  const { chrome, setActiveTabUrl } = createChromeMock();
  setActiveTabUrl("chrome://extensions");
  const background = createBackground(chrome, { disableReconnect: true });
  const popupPort = new MockPort("popup");

  background.handlePopupConnect(popupPort);
  popupPort.emit({ type: "get_popup_approval_state" });
  await tick();

  assert.deepEqual(popupPort.sent.findLast((message) => message.type === "approval_state"), {
    type: "approval_state",
    approvalState: {
      origin: null,
      approved: false,
      pending: false,
    },
  });
});

test("approving active tab origin stores approval without native host", async () => {
  const { chrome, storageData, getConnectNativeCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const popupPort = new MockPort("popup");
  background.handlePopupConnect(popupPort);

  popupPort.emit({ type: "approve_origin", origin: "https://active.example.test" });
  await tick();

  assert.equal(getConnectNativeCallCount(), 0);
  assert.equal(storageData.approvedOrigins[0].origin, "https://active.example.test");
  assert.equal(popupPort.sent.findLast((message) => message.type === "approval_state").approvalState.approved, true);
});

test("revoking active tab origin removes approval without native host", async () => {
  const { chrome, storageData, getConnectNativeCallCount } = createChromeMock();
  storageData.approvedOrigins = [
    { origin: "https://active.example.test", approvedAt: 1 },
    { origin: "https://other.example.test", approvedAt: 2 },
  ];
  const background = createBackground(chrome, { disableReconnect: true });
  const popupPort = new MockPort("popup");
  background.handlePopupConnect(popupPort);

  popupPort.emit({ type: "revoke_origin", origin: "https://active.example.test" });
  await tick();

  assert.equal(getConnectNativeCallCount(), 0);
  assert.deepEqual(storageData.approvedOrigins, [{ origin: "https://other.example.test", approvedAt: 2 }]);
  assert.equal(popupPort.sent.findLast((message) => message.type === "approval_state").approvalState.approved, false);
});

test("SDK list_providers routes to native host", async () => {
  const { chrome, nativePort, getConnectNativeCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = new MockPort("pedelec-sdk-internal");
  background.handleSdkConnect(sdkPort);
  assert.equal(getConnectNativeCallCount(), 0);

  sdkPort.emit({
    type: "list_providers",
    channelId: "channel_1",
    requestId: "sdk_providers",
  });
  assert.equal(getConnectNativeCallCount(), 1);
  assert.equal(nativePort.lastSent().type, "list_providers");
  respondNative(nativePort, nativePort.lastSent(), [
    { name: "OpenCode", code: "opencode", path: null, available: false, error: "program was not found in PATH" },
  ]);
  await tick();

  assert.deepEqual(sdkPort.lastSent(), {
    channelId: "channel_1",
    type: "response",
    requestId: "sdk_providers",
    ok: true,
    result: [
      { name: "OpenCode", code: "opencode", path: null, available: false, error: "program was not found in PATH" },
    ],
  });
  assert.equal(nativePort.disconnectCallCount, 1);
  assert.equal(background.getState().connected, false);
  assert.equal(background.getState().error, null);
});

test("SDK get_settings routes to native host", async () => {
  const { chrome, nativePort, getConnectNativeCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = new MockPort("pedelec-sdk-internal");
  background.handleSdkConnect(sdkPort);
  assert.equal(getConnectNativeCallCount(), 0);

  sdkPort.emit({
    type: "get_settings",
    channelId: "channel_1",
    requestId: "sdk_settings",
  });
  assert.equal(getConnectNativeCallCount(), 1);
  assert.equal(nativePort.lastSent().type, "get_settings");
  respondNative(nativePort, nativePort.lastSent(), {
    defaultProvider: "codex",
    defaultModels: {
      codex: "gpt-5",
    },
  });
  await tick();

  assert.deepEqual(sdkPort.lastSent(), {
    channelId: "channel_1",
    type: "response",
    requestId: "sdk_settings",
    ok: true,
    result: {
      defaultProvider: "codex",
      defaultModels: {
        codex: "gpt-5",
      },
    },
  });
  assert.equal(nativePort.disconnectCallCount, 1);
});

test("SDK create_session forwards opencode provider", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = new MockPort("pedelec-sdk-internal");
  background.handleSdkConnect(sdkPort);
  const skills = {
    guidance: "Use tools.",
    tools: [
      {
        name: "get_app_state",
        description: "Get app state.",
        argsSchema: { type: "object", properties: {}, required: [], additionalProperties: false },
        timeoutMs: 60000,
      },
    ],
  };

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create",
    input: { provider: "opencode", model: "ollama/qwen2.5-coder:14b", skills },
  });
  assert.equal(nativePort.lastSent().type, "create_thread");
  assert.equal(nativePort.lastSent().provider, "opencode");
  assert.equal(nativePort.lastSent().model, "ollama/qwen2.5-coder:14b");
  assert.deepEqual(nativePort.lastSent().skills, skills);
  assert.equal("skillsUrls" in nativePort.lastSent(), false);
});

test("SDK create_session forwards cursor provider", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = new MockPort("pedelec-sdk-internal");
  background.handleSdkConnect(sdkPort);

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create",
    input: { provider: "cursor", model: "gpt-5", skills: undefined },
  });
  assert.equal(nativePort.lastSent().type, "create_thread");
  assert.equal(nativePort.lastSent().provider, "cursor");
  assert.equal(nativePort.lastSent().model, "gpt-5");
});

test("SDK prepare_session forwards prepare_thread", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = await createSdkSession(background, nativePort);

  sdkPort.emit({
    type: "prepare_session",
    channelId: "channel_1",
    requestId: "sdk_prepare",
    sessionId: "thread_sdk",
  });
  await tick();

  assert.equal(nativePort.lastSent().type, "prepare_thread");
  assert.equal(nativePort.lastSent().threadId, "thread_sdk");
  respondNative(nativePort, nativePort.lastSent(), { threadId: "thread_sdk", prepared: true });
  await tick();
  assert.deepEqual(sdkPort.lastSent(), {
    channelId: "channel_1",
    type: "response",
    requestId: "sdk_prepare",
    ok: true,
    result: {},
  });
});

test("SDK list_assets forwards the session route and returns Core assets", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = await createSdkSession(background, nativePort);
  sdkPort.emit({ type: "list_assets", channelId: "channel_1", requestId: "sdk_list", sessionId: "thread_sdk" });
  await tick();
  assert.deepEqual(nativePort.lastSent(), { requestId: nativePort.lastSent().requestId, type: "list_assets", threadId: "thread_sdk" });
  respondNative(nativePort, nativePort.lastSent(), { assets: [{ name: "upl_a.txt", path: "input/upl_a.txt", sizeBytes: 1, modifiedAt: 1 }] });
  await tick();
  assert.deepEqual(sdkPort.lastSent(), { channelId: "channel_1", type: "response", requestId: "sdk_list", ok: true, result: { assets: [{ name: "upl_a.txt", path: "input/upl_a.txt", sizeBytes: 1, modifiedAt: 1 }] } });
});

test("unapproved external prepare_session opens approval without native host", async () => {
  const { chrome, nativePort, getConnectNativeCallCount, getOpenPopupCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = createExternalSdkPort("https://app.example.test");
  background.handleSdkExternalConnect(sdkPort);

  sdkPort.emit({
    type: "prepare_session",
    channelId: "channel_1",
    requestId: "sdk_prepare",
    sessionId: "thread_sdk",
  });
  await tick();
  await tick();

  assert.equal(getOpenPopupCallCount(), 1);
  assert.equal(getConnectNativeCallCount(), 0);
  assert.equal(nativePort.sent.length, 0);
  assert.equal(sdkPort.sent.length, 0);
  assert.deepEqual(background.getPendingApproval(), {
    origin: "https://app.example.test",
    requestedAt: background.getPendingApproval().requestedAt,
    requestCount: 1,
  });
});

test("SDK create_session forwards claude provider", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = new MockPort("pedelec-sdk-internal");
  background.handleSdkConnect(sdkPort);

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create",
    input: { provider: "claude", model: "sonnet", skills: undefined },
  });
  assert.equal(nativePort.lastSent().type, "create_thread");
  assert.equal(nativePort.lastSent().provider, "claude");
  assert.equal(nativePort.lastSent().model, "sonnet");
});

test("routes SDK thread events without mutating popup state", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = await createSdkSession(background, nativePort);

  background.handleNativeMessage({
    type: "thread_event",
    event: {
      type: "assistant_message",
      seq: 1,
      threadId: "thread_sdk",
      text: "hello",
    },
  });

  assert.deepEqual(sdkPort.lastSent(), {
    channelId: "channel_1",
    type: "chat_delta",
    sessionId: "thread_sdk",
    seq: 1,
    text: "hello",
  });
  assert.equal(background.getState().threadId, null);
  assert.equal(background.getState().events.length, 0);
});

test("provider errors persist, open the popup, and continue to the SDK route", async () => {
  const { chrome, nativePort, sessionStorageData, getOpenPopupCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = await createSdkSession(background, nativePort);

  background.handleNativeMessage({
    type: "thread_event",
    event: {
      type: "error", threadId: "thread_sdk", seq: 1, source: "provider", provider: "codex",
      error: { code: "PROVIDER_COMMAND_FAILED", message: "Authentication failed" },
    },
  });
  await tick();

  assert.deepEqual(sessionStorageData.latestProviderError, {
    provider: "codex",
    message: "Authentication failed",
  });
  assert.equal(getOpenPopupCallCount(), 1);
  assert.deepEqual(sdkPort.lastSent(), {
    channelId: "channel_1", type: "error", sessionId: "thread_sdk", seq: 1,
    source: "provider", provider: "codex",
    error: { code: "PROVIDER_COMMAND_FAILED", message: "Authentication failed" },
  });
  assert.equal(background.getState().threadId, null);
});

test("provider error preserves the provider title and only retains the latest error", async () => {
  const { chrome, sessionStorageData } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const popupPort = new MockPort("popup");
  background.handlePopupConnect(popupPort);

  for (const [provider, message] of [["CoDeX-custom", "First error"], ["cursor", "Second error"]]) {
    background.handleNativeMessage({
      type: "thread_event",
      event: { type: "error", source: "provider", provider, error: { message } },
    });
  }
  await tick();
  await tick();

  assert.deepEqual(sessionStorageData.latestProviderError, { provider: "cursor", message: "Second error" });
  const providerStates = popupPort.sent.filter((message) => message.type === "provider_error_state");
  assert.deepEqual(providerStates.at(-1), {
    type: "provider_error_state",
    providerError: { provider: "cursor", message: "Second error" },
  });
  assert.equal(providerStates.some((message) => message.providerError?.provider === "CoDeX-custom"), true);
});

test("core and legacy errors do not trigger the provider error popup", async () => {
  const { chrome, nativePort, sessionStorageData, getOpenPopupCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = await createSdkSession(background, nativePort);

  for (const event of [
    { type: "error", threadId: "thread_sdk", seq: 1, source: "core", error: { message: "core" } },
    { type: "error", threadId: "thread_sdk", seq: 2, error: { message: "legacy" } },
    { type: "assistant_message", threadId: "thread_sdk", seq: 3, source: "provider", text: "not an error" },
  ]) {
    background.handleNativeMessage({ type: "thread_event", event });
  }
  await tick();

  assert.equal(sessionStorageData.latestProviderError, undefined);
  assert.equal(getOpenPopupCallCount(), 0);
  assert.deepEqual(sdkPort.lastSent(), {
    channelId: "channel_1", type: "chat_delta", sessionId: "thread_sdk", seq: 3, text: "not an error",
  });
});

test("popup connect reads a stored provider error and dismiss removes it for every popup", async () => {
  const { chrome, sessionStorageData } = createChromeMock();
  sessionStorageData.latestProviderError = { provider: "cursor", message: "Login required" };
  const background = createBackground(chrome, { disableReconnect: true });
  const firstPopupPort = new MockPort("popup");
  const secondPopupPort = new MockPort("popup");
  background.handlePopupConnect(firstPopupPort);
  background.handlePopupConnect(secondPopupPort);
  await tick();

  assert.deepEqual(firstPopupPort.sent.findLast((message) => message.type === "provider_error_state"), {
    type: "provider_error_state",
    providerError: { provider: "cursor", message: "Login required" },
  });
  firstPopupPort.emit({ type: "dismiss_provider_error" });
  await tick();

  assert.equal(sessionStorageData.latestProviderError, undefined);
  for (const port of [firstPopupPort, secondPopupPort]) {
    assert.deepEqual(port.sent.findLast((message) => message.type === "provider_error_state"), {
      type: "provider_error_state",
      providerError: null,
    });
    assert.equal(port.disconnectCallCount, 0);
  }
});

test("provider error popup failure keeps the event dispatch and stored error intact", async () => {
  const { chrome, nativePort, sessionStorageData } = createChromeMock();
  chrome.action.openPopup = () => Promise.reject(new Error("blocked"));
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = await createSdkSession(background, nativePort);

  background.handleNativeMessage({
    type: "thread_event",
    event: {
      type: "error", threadId: "thread_sdk", seq: 1, source: "provider", provider: "codex",
      error: "Login required",
    },
  });
  await tick();
  await tick();

  assert.deepEqual(sessionStorageData.latestProviderError, { provider: "codex", message: "Login required" });
  assert.equal(sdkPort.lastSent().type, "error");
  assert.equal(background.getState().error, null);
});

test("SDK create_session lazy connects and keeps native host while active", async () => {
  const { chrome, nativePort, getConnectNativeCallCount } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });

  await createSdkSession(background, nativePort);

  assert.equal(getConnectNativeCallCount(), 1);
  assert.equal(background.getActiveThreadCount(), 1);
  assert.equal(background.getState().connected, true);
  assert.equal(nativePort.disconnectCallCount, 0);
});

test("maps Core statuses to SDK statuses", () => {
  const { chrome } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });

  assert.deepEqual(
    background.sdkEventFromThreadEvent({
      type: "status_changed",
      seq: 3,
      threadId: "thread_sdk",
      status: "waitingToolResult",
    }),
    {
      type: "status_changed",
      sessionId: "thread_sdk",
      seq: 3,
      status: "waiting_tool_result",
    }
  );
});

test("maps provider error source and provider to SDK events", () => {
  const { chrome } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });

  assert.deepEqual(
    background.sdkEventFromThreadEvent({
      type: "error", seq: 4, threadId: "thread_sdk", source: "provider", provider: "cursor",
      error: { code: "PROVIDER_COMMAND_FAILED", message: "provider command failed" },
    }),
    {
      type: "error", sessionId: "thread_sdk", seq: 4, source: "provider", provider: "cursor",
      error: { code: "PROVIDER_COMMAND_FAILED", message: "provider command failed" },
    }
  );
});

test("maps core errors without a provider and accepts legacy errors", () => {
  const { chrome } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });

  const core = background.sdkEventFromThreadEvent({
    type: "error", seq: 5, threadId: "thread_sdk", source: "core",
    error: { code: "INTERNAL_ERROR", message: "Pedelec internal operation failed" },
  });
  assert.deepEqual(core, {
    type: "error", sessionId: "thread_sdk", seq: 5, source: "core",
    error: { code: "INTERNAL_ERROR", message: "Pedelec internal operation failed" },
  });
  assert.equal(Object.hasOwn(core, "provider"), false);

  assert.doesNotThrow(() => background.sdkEventFromThreadEvent({
    type: "error", seq: 6, threadId: "thread_sdk", error: { code: "LEGACY", message: "legacy" },
  }));
});

test("SDK port disconnect auto-ends default lifecycle session", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = await createSdkSession(background, nativePort);

  sdkPort.disconnect();
  assert.equal(nativePort.lastSent().type, "end_thread");
  assert.equal(nativePort.lastSent().threadId, "thread_sdk");
  respondNative(nativePort, nativePort.lastSent(), {});
  await tick();

  assert.equal(background.getSdkRouteCount(), 0);
  assert.equal(background.getActiveThreadCount(), 0);
  assert.equal(nativePort.disconnectCallCount, 1);
});

test("SDK port disconnect does not auto-end when lifecycle opts out", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = new MockPort("pedelec-sdk-internal");
  background.handleSdkConnect(sdkPort);

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create",
    input: { provider: "codex", skills: undefined, autoEndOnDisconnect: false },
  });
  respondNative(nativePort, nativePort.lastSent(), { threadId: "thread_keep_alive" });
  await tick();
  respondNative(nativePort, nativePort.lastSent(), { subscribed: true });
  await tick();
  const sentBeforeDisconnect = nativePort.sent.length;

  sdkPort.disconnect();

  assert.equal(background.getSdkRouteCount(), 0);
  assert.equal(nativePort.sent.length, sentBeforeDisconnect);
  assert.equal(nativePort.sent.some((message) => message.type === "end_thread"), false);
  assert.equal(background.getActiveThreadCount(), 1);
});

test("SDK auto-end waits until the final route disconnects", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const firstPort = await createSdkSession(background, nativePort);
  const secondPort = new MockPort("pedelec-sdk-internal");
  background.handleSdkConnect(secondPort);

  secondPort.emit({
    type: "resume_session",
    channelId: "channel_2",
    requestId: "sdk_resume",
    sessionId: "thread_sdk",
  });
  assert.equal(nativePort.lastSent().type, "subscribe_thread");
  respondNative(nativePort, nativePort.lastSent(), { subscribed: true });
  await tick();

  const sentBeforeFirstDisconnect = nativePort.sent.length;
  firstPort.disconnect();
  assert.equal(nativePort.sent.length, sentBeforeFirstDisconnect);
  assert.equal(background.getSdkRouteCount(), 1);

  secondPort.disconnect();
  assert.equal(nativePort.lastSent().type, "end_thread");
  assert.equal(nativePort.lastSent().threadId, "thread_sdk");
  respondNative(nativePort, nativePort.lastSent(), {});
  await tick();

  assert.equal(background.getSdkRouteCount(), 0);
  assert.equal(background.getActiveThreadCount(), 0);
});

test("end_session calls end_thread", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = await createSdkSession(background, nativePort);

  sdkPort.emit({
    type: "end_session",
    channelId: "channel_1",
    requestId: "sdk_end",
    sessionId: "thread_sdk",
  });
  await tick();
  assert.equal(nativePort.lastSent().type, "end_thread");
  assert.equal(nativePort.lastSent().threadId, "thread_sdk");
  respondNative(nativePort, nativePort.lastSent(), {});
  await tick();

  assert.deepEqual(sdkPort.lastSent(), {
    channelId: "channel_1",
    type: "response",
    requestId: "sdk_end",
    ok: true,
    result: {},
  });
  assert.equal(background.getActiveThreadCount(), 0);
  assert.equal(nativePort.disconnectCallCount, 1);
  assert.equal(background.getState().connected, false);
});

test("SDK disconnect after manual end does not duplicate cleanup", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = await createSdkSession(background, nativePort);

  sdkPort.emit({
    type: "end_session",
    channelId: "channel_1",
    requestId: "sdk_end",
    sessionId: "thread_sdk",
  });
  await tick();
  respondNative(nativePort, nativePort.lastSent(), {});
  await tick();
  const endThreadCount = nativePort.sent.filter((message) => message.type === "end_thread").length;

  sdkPort.disconnect();

  assert.equal(nativePort.sent.filter((message) => message.type === "end_thread").length, endThreadCount);
  assert.equal(background.getSdkRouteCount(), 0);
});

test("auto-end failure does not prevent other SDK sessions from operating", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const firstPort = await createSdkSession(background, nativePort, "thread_failing_auto_end");
  const secondPort = await createSdkSession(background, nativePort, "thread_other");
  const originalPostMessage = nativePort.postMessage.bind(nativePort);
  nativePort.postMessage = (message) => {
    if (message?.type === "end_thread" && message.threadId === "thread_failing_auto_end") {
      throw new Error("end failed");
    }
    originalPostMessage(message);
  };

  firstPort.disconnect();
  await tick();
  assert.equal(background.getState().error, "AUTO_END_SESSION_FAILED: end failed");

  secondPort.emit({
    type: "list_providers",
    channelId: "channel_1",
    requestId: "sdk_providers_after_failure",
  });
  assert.equal(nativePort.lastSent().type, "list_providers");
  respondNative(nativePort, nativePort.lastSent(), []);
  await tick();

  assert.deepEqual(secondPort.lastSent(), {
    channelId: "channel_1",
    type: "response",
    requestId: "sdk_providers_after_failure",
    ok: true,
    result: [],
  });
  assert.equal(background.getActiveThreadCount(), 2);
});

test("create_session after idle disconnect opens a fresh native port before old disconnect event", async () => {
  const nativePorts = [];
  const chrome = {
    runtime: {
      lastError: null,
      onConnect: { addListener: () => {} },
      connectNative: () => {
        const port = new MockPort("native");
        port.disconnect = function disconnectWithoutEvent() {
          this.disconnectCallCount += 1;
        };
        nativePorts.push(port);
        return port;
      },
    },
  };
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = new MockPort("pedelec-sdk-internal");
  background.handleSdkConnect(sdkPort);

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create_1",
    input: { provider: "codex", skills: undefined },
  });
  assert.equal(nativePorts.length, 1);
  respondNative(nativePorts[0], nativePorts[0].lastSent(), { threadId: "thread_1" });
  await tick();
  respondNative(nativePorts[0], nativePorts[0].lastSent(), { subscribed: true });
  await tick();

  sdkPort.emit({
    type: "end_session",
    channelId: "channel_1",
    requestId: "sdk_end_1",
    sessionId: "thread_1",
  });
  await tick();
  respondNative(nativePorts[0], nativePorts[0].lastSent(), {});
  await tick();

  assert.equal(nativePorts[0].disconnectCallCount, 1);
  assert.equal(background.getState().connected, false);

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_2",
    requestId: "sdk_create_2",
    input: { provider: "codex", skills: undefined },
  });

  assert.equal(nativePorts.length, 2);
  assert.equal(nativePorts[1].lastSent().type, "create_thread");
  assert.equal(sdkPort.lastSent().type, "response");
  assert.equal(sdkPort.lastSent().requestId, "sdk_end_1");
  respondNative(nativePorts[1], nativePorts[1].lastSent(), { threadId: "thread_2" });
  await tick();
  respondNative(nativePorts[1], nativePorts[1].lastSent(), { subscribed: true });
  await tick();

  assert.deepEqual(sdkPort.lastSent(), {
    channelId: "channel_2",
    type: "response",
    requestId: "sdk_create_2",
    ok: true,
    result: { sessionId: "thread_2" },
  });
  assert.equal(background.getActiveThreadCount(), 1);
  assert.equal(background.getState().connected, true);
});

test("multiple SDK sessions keep native host until final session ends", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = new MockPort("pedelec-sdk-internal");
  background.handleSdkConnect(sdkPort);

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_1",
    requestId: "sdk_create_a",
    input: { provider: "codex", skills: undefined },
  });
  respondNative(nativePort, nativePort.lastSent(), { threadId: "thread_a" });
  await tick();
  respondNative(nativePort, nativePort.lastSent(), { subscribed: true });
  await tick();

  sdkPort.emit({
    type: "create_session",
    channelId: "channel_2",
    requestId: "sdk_create_b",
    input: { provider: "codex", skills: undefined },
  });
  respondNative(nativePort, nativePort.lastSent(), { threadId: "thread_b" });
  await tick();
  respondNative(nativePort, nativePort.lastSent(), { subscribed: true });
  await tick();

  assert.equal(background.getActiveThreadCount(), 2);

  sdkPort.emit({
    type: "end_session",
    channelId: "channel_1",
    requestId: "sdk_end_a",
    sessionId: "thread_a",
  });
  await tick();
  respondNative(nativePort, nativePort.lastSent(), {});
  await tick();

  assert.equal(background.getActiveThreadCount(), 1);
  assert.equal(nativePort.disconnectCallCount, 0);

  sdkPort.emit({
    type: "end_session",
    channelId: "channel_2",
    requestId: "sdk_end_b",
    sessionId: "thread_b",
  });
  await tick();
  respondNative(nativePort, nativePort.lastSent(), {});
  await tick();

  assert.equal(background.getActiveThreadCount(), 0);
  assert.equal(nativePort.disconnectCallCount, 1);
});

test("thread ended event removes SDK route and disconnects when idle", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome, { disableReconnect: true });
  const sdkPort = await createSdkSession(background, nativePort);

  background.handleNativeMessage({
    type: "thread_event",
    event: {
      type: "ended",
      seq: 2,
      threadId: "thread_sdk",
    },
  });

  assert.deepEqual(sdkPort.lastSent(), {
    channelId: "channel_1",
    type: "ended",
    sessionId: "thread_sdk",
    seq: 2,
  });
  assert.equal(background.getSdkRouteCount(), 0);
  assert.equal(background.getActiveThreadCount(), 0);
  assert.equal(nativePort.disconnectCallCount, 1);
  assert.equal(background.getState().connected, false);
  assert.equal(background.getState().error, null);
});

test("intentional idle disconnect does not notify SDK error or schedule reconnect", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome);
  const sdkPort = new MockPort("pedelec-sdk-internal");
  background.handleSdkConnect(sdkPort);

  sdkPort.emit({
    type: "list_providers",
    channelId: "channel_1",
    requestId: "sdk_providers",
  });
  respondNative(nativePort, nativePort.lastSent(), []);
  await tick();

  assert.equal(nativePort.disconnectCallCount, 1);
  assert.equal(background.hasReconnectTimer(), false);
  assert.equal(sdkPort.sent.some((message) => message.type === "error"), false);
  assert.equal(background.getState().error, null);
});

test("unexpected disconnect reconnects only while active sessions remain", async () => {
  const { chrome, nativePort } = createChromeMock();
  const background = createBackground(chrome);
  await createSdkSession(background, nativePort);

  chrome.runtime.lastError = { message: "crashed" };
  background.handleNativeDisconnect();

  assert.equal(background.hasReconnectTimer(), true);
  assert.equal(background.getState().connected, false);
  assert.equal(background.getState().error, "crashed");

  background.handleNativeMessage({
    type: "thread_event",
    event: {
      type: "ended",
      seq: 3,
      threadId: "thread_sdk",
    },
  });

  assert.equal(background.getActiveThreadCount(), 0);
  assert.equal(background.hasReconnectTimer(), false);
});

test("unexpected disconnect without active sessions does not reconnect", () => {
  const { chrome } = createChromeMock();
  const background = createBackground(chrome);

  chrome.runtime.lastError = { message: "closed" };
  background.handleNativeDisconnect();

  assert.equal(background.hasReconnectTimer(), false);
});

