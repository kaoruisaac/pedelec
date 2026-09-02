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

function tabsForQuery(chrome, queryInfo) {
  if (queryInfo?.active === true && queryInfo?.lastFocusedWindow === true) {
    if (chrome.currentTab) return [chrome.currentTab];
    if (chrome.currentTabId == null) return [];
    return [{ id: chrome.currentTabId }];
  }
  if (queryInfo?.active === true && queryInfo?.currentWindow === true && chrome.activeTab) {
    return [chrome.activeTab];
  }
  return [];
}

function setCurrentBrowserTab(chrome, tabId, tab = null) {
  chrome.currentTabId = tabId;
  chrome.currentTab = tab;
}

function lastFocusedTabQueries(chrome) {
  return chrome.tabsQueries.filter((query) => query?.lastFocusedWindow === true);
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
    tabsQueryCalls: 0,
    tabsQueries: [],
    currentTabId: null,
    currentTab: null,
    tabsQueryError: null,
    tabsQueryThrow: false,
    tabs: {
      query: (queryInfo, callback) => {
        chrome.tabsQueryCalls += 1;
        chrome.tabsQueries.push(queryInfo);
        if (chrome.tabsQueryThrow) {
          throw new Error("tabs.query failed");
        }
        if (chrome.tabsQueryError) {
          chrome.runtime.lastError = chrome.tabsQueryError;
          callback?.();
          return;
        }
        chrome.runtime.lastError = null;
        const tabs = tabsForQuery(chrome, queryInfo);
        if (typeof callback === "function") callback(tabs);
        return Promise.resolve(tabs);
      },
    },
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

function connectExternal(chrome, senderUrl = "https://app.example.test/page", tabId = null) {
  const port = new MockPort();
  port.name = "pedelec-sdk-external";
  port.sender = { url: senderUrl };
  if (tabId != null) {
    port.sender.tab = { id: tabId };
  }
  chrome.connectExternal(port);
  return port;
}

function connectPopup(background) {
  const popup = new MockPort();
  popup.name = "popup";
  background.handlePopupConnect(popup);
  return popup;
}

function lastProviderErrorState(popup) {
  return [...popup.sent].reverse().find((message) => message.type === "provider_error_state");
}

async function waitForProviderErrorState(popup, expected) {
  await waitFor(() => {
    const message = lastProviderErrorState(popup);
    if (!message) return false;
    if (expected == null) return message.providerError == null;
    return (
      message.providerError?.provider === expected.provider &&
      message.providerError?.message === expected.message
    );
  }, "provider_error_state was not reached");
}

function storedProviderError(chrome, tabId) {
  return chrome.storage.session.data.providerErrorByTab?.[String(tabId)] || null;
}

async function waitForStoredTabError(chrome, tabId, expected) {
  await waitFor(() => {
    const value = storedProviderError(chrome, tabId);
    if (expected == null) return value == null;
    return value?.provider === expected.provider && value?.message === expected.message;
  }, `stored provider error for tab ${tabId} was not reached`);
}

function providerErrorEvent(threadId, provider = "codex", message = "provider failed", seq = 1) {
  return {
    type: "error",
    source: "provider",
    threadId,
    provider,
    seq,
    error: { code: "PROVIDER_ERROR", message },
  };
}

function emitPageActivity(port, active) {
  port.emit({ type: "page_activity", active });
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

async function createSdkSession(background, sdkPort, nativePort, sessionId, autoEndOnDisconnect = true, channelId = "channel_a") {
  const requestId = `create_${sessionId}`;
  const minimumMessageCount = nativePort.sent.length + 1;
  sdkPort.emit({
    channelId,
    requestId,
    type: "create_session",
    input: { provider: "codex", autoEndOnDisconnect },
  });
  await respondToNative(background, nativePort, { threadId: sessionId }, minimumMessageCount);
  await respondToNative(background, nativePort, {}, minimumMessageCount + 1);
  await waitFor(() => sdkPort.sent.some((message) => message.requestId === requestId));
  return sdkPort.sent.find((message) => message.requestId === requestId);
}

test("create_session forwards an explicit workspace without inventing a path", async () => {
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
      workspace: { path: "C:\\workspace\\project-a" },
      autoEndOnDisconnect: true,
    },
  });
  const nativeCreate = await respondToNative(background, native, { threadId: "thread_custom" });
  assert.deepEqual(nativeCreate.workspace, { path: "C:\\workspace\\project-a" });

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
  assert.equal(nativeDefault.workspace, undefined);
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

test("approved pick_workspace_folder forwards the origin and returns folder inspection", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();
  const sdk = connectExternal(chrome);

  sdk.emit({ channelId: "channel_a", requestId: "pick_path", type: "pick_workspace_folder" });
  const request = await respondToNative(background, native, {
    path: "C:\\workspace\\project",
    isEmptyFolder: false,
    hasWorkspaceConfig: true,
  });
  assert.deepEqual(
    { type: request.type, callerOrigin: request.callerOrigin },
    { type: "pick_workspace_folder", callerOrigin: "https://app.example.test" },
  );
  await waitFor(() => sdk.sent.some((message) => message.requestId === "pick_path"));
  assert.deepEqual(sdk.sent.find((message) => message.requestId === "pick_path").result, {
    path: "C:\\workspace\\project",
    isEmptyFolder: false,
    hasWorkspaceConfig: true,
  });
  assert.deepEqual(background.getState().events, []);
  assert.equal(background.getState().error, null);
});

test("pick_workspace_folder cancellation is forwarded as a successful null result and keeps native work alive", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();
  const sdk = connectExternal(chrome);

  sdk.emit({ channelId: "channel_a", requestId: "pick_cancel", type: "pick_workspace_folder" });
  await waitFor(() => native.sent.length === 1);
  assert.equal(background.getNativeRequestCount(), 1);
  assert.equal(native.disconnectCount, 0);
  await respondToNative(background, native, { path: null });
  await waitFor(() => sdk.sent.some((message) => message.requestId === "pick_cancel"));
  const response = sdk.sent.find((message) => message.requestId === "pick_cancel");
  assert.equal(response.ok, true);
  assert.deepEqual(response.result, { path: null });
  assert.equal(native.disconnectCount, 1);
});

test("unapproved pick_workspace_folder enters approval and replays after approval", async () => {
  const chrome = createChrome({ approved: false });
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { approvalTimeoutMs: 1000, disableReconnect: true });
  background.start();
  const sdk = connectExternal(chrome);

  sdk.emit({ channelId: "channel_a", requestId: "pick_approval", type: "pick_workspace_folder" });
  await waitFor(() => background.getPendingApproval()?.requestCount === 1);
  assert.equal(native.sent.length, 0);

  const popup = new MockPort();
  popup.name = "popup";
  background.handlePopupConnect(popup);
  popup.emit({ type: "approve_origin", origin: "https://app.example.test" });
  const request = await respondToNative(background, native, { path: null });
  assert.equal(request.type, "pick_workspace_folder");
  assert.equal(request.callerOrigin, "https://app.example.test");
  await waitFor(() => sdk.sent.some((message) => message.requestId === "pick_approval"));
});

test("pick_workspace_folder native failures are returned as SDK errors without changing extension state", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();
  const sdk = connectExternal(chrome);

  sdk.emit({ channelId: "channel_a", requestId: "pick_failed", type: "pick_workspace_folder" });
  await waitFor(() => native.sent.length === 1);
  const request = native.sent[0];
  background.handleNativeMessage({
    type: "response",
    requestId: request.requestId,
    ok: false,
    error: { code: "DIRECTORY_PICKER_FAILED", message: "dialog failed" },
  });
  await waitFor(() => sdk.sent.some((message) => message.requestId === "pick_failed"));
  const response = sdk.sent.find((message) => message.requestId === "pick_failed");
  assert.equal(response.ok, false);
  assert.equal(response.error.code, "DIRECTORY_PICKER_FAILED");
  assert.deepEqual(background.getState().events, []);
  assert.equal(background.getState().error, null);
});

test("create_session forwards SDK version metadata separately from consumer input", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();
  const sdk = connectExternal(chrome);

  sdk.emit({
    channelId: "channel_a",
    requestId: "create_version",
    type: "create_session",
    callerSdkVersion: "mock-sdk-version",
    input: {
      provider: "codex",
      workspace: { path: "C:\\workspace\\project", callerSdkVersion: "consumer-value" },
    },
  });
  const request = await respondToNative(background, native, { threadId: "thread_version" });
  assert.equal(request.callerSdkVersion, "mock-sdk-version");
  assert.equal(request.workspace.callerSdkVersion, "consumer-value");
  await respondToNative(background, native, {}, 2);
});

test("legacy pick_sandbox_folder SDK request is rejected", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();
  const sdk = connectExternal(chrome);

  sdk.emit({ channelId: "channel_a", requestId: "pick_old", type: "pick_sandbox_folder" });
  await waitFor(() => sdk.sent.some((message) => message.requestId === "pick_old"));
  const response = sdk.sent.find((message) => message.requestId === "pick_old");
  assert.equal(response.ok, false);
  assert.equal(native.sent.length, 0);
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

  background.dispatchSdkThreadEvent({ threadId: "thread_resume", type: "assistant_delta", seq: 1, text: "res" });
  assert.deepEqual(portB.sent.at(-1), {
    channelId: "channel_b",
    sessionId: "thread_resume",
    seq: 1,
    type: "chat_delta",
    text: "res",
  });

  background.dispatchSdkThreadEvent({ threadId: "thread_resume", type: "assistant_message", seq: 2, text: "resumed" });
  assert.deepEqual(portB.sent.at(-1), {
    channelId: "channel_b",
    sessionId: "thread_resume",
    seq: 2,
    type: "chat_message",
    text: "resumed",
  });
});

test("external SDK port captures tab identity without tabs permission API lookup", async () => {
  const chrome = createChrome();
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();
  const queriesBefore = chrome.tabsQueryCalls;
  const sdk = connectExternal(chrome, "https://app.example.test/page", 17);
  emitPageActivity(sdk, true);
  assert.equal(background.getActiveSdkTabId(), 17);
  assert.equal(chrome.tabsQueryCalls, queriesBefore);
  assert.equal(sdk.sent.some((message) => message.type === "response" && !message.ok), false);
});

test("page activity selects the active Pedelec tab", async () => {
  const chrome = createChrome();
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();
  const tabA = connectExternal(chrome, "https://app.example.test/a", 11);
  const tabB = connectExternal(chrome, "https://app.example.test/b", 22);
  emitPageActivity(tabA, true);
  assert.equal(background.getActiveSdkTabId(), 11);
  emitPageActivity(tabB, true);
  assert.equal(background.getActiveSdkTabId(), 22);
  emitPageActivity(tabB, false);
  assert.equal(background.getActiveSdkTabId(), null);
});

test("two tabs on the same origin keep independent provider errors", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const tabA = connectExternal(chrome, "https://app.example.test/a", 11);
  const tabB = connectExternal(chrome, "https://app.example.test/b", 22);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(tabA, true);
  await createSdkSession(background, tabA, native, "thread_a");
  emitPageActivity(tabA, false);
  setCurrentBrowserTab(chrome, 22);
  emitPageActivity(tabB, true);
  await createSdkSession(background, tabB, native, "thread_b");

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "tab a failed"),
  });
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_b", "ollama", "tab b failed"),
  });
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "tab a failed" });
  await waitForStoredTabError(chrome, 22, { provider: "ollama", message: "tab b failed" });

  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, { provider: "ollama", message: "tab b failed" });
  emitPageActivity(tabB, false);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(tabA, true);
  await waitForProviderErrorState(popup, { provider: "codex", message: "tab a failed" });
});

test("multiple SDK ports in the same tab share one provider-error context", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const portA = connectExternal(chrome, "https://app.example.test/page", 17);
  const portB = connectExternal(chrome, "https://app.example.test/page", 17);
  setCurrentBrowserTab(chrome, 17);
  emitPageActivity(portA, true);
  emitPageActivity(portB, true);
  await createSdkSession(background, portA, native, "thread_one");
  await createSdkSession(background, portB, native, "thread_two", true, "channel_b");

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_two", "codex", "second session failed"),
  });
  await waitForStoredTabError(chrome, 17, { provider: "codex", message: "second session failed" });

  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, { provider: "codex", message: "second session failed" });

  portA.disconnect();
  await flush();
  assert.equal(background.getActiveSdkTabId(), 17);
  await waitForStoredTabError(chrome, 17, { provider: "codex", message: "second session failed" });
});

test("active-tab provider errors are visible to popup and open the popup", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/page", 17);
  setCurrentBrowserTab(chrome, 17);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_active");
  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, null);

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_active", "codex", "quota exceeded"),
  });
  await waitForProviderErrorState(popup, { provider: "codex", message: "quota exceeded" });
  await waitFor(() => chrome.openPopupCalls === 1, "openPopup was not called for the active tab");
  assert.deepEqual(sdk.sent.at(-1), {
    channelId: "channel_a",
    sessionId: "thread_active",
    seq: 1,
    type: "error",
    source: "provider",
    provider: "codex",
    error: { code: "PROVIDER_ERROR", message: "quota exceeded" },
  });
});

test("inactive-tab provider errors are retained without replacing the active popup or opening it", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const tabA = connectExternal(chrome, "https://app.example.test/a", 11);
  const tabB = connectExternal(chrome, "https://app.example.test/b", 22);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(tabA, true);
  await createSdkSession(background, tabA, native, "thread_a");
  await createSdkSession(background, tabB, native, "thread_b", true, "channel_b");
  assert.equal(background.getActiveSdkTabId(), 11);

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "active error"),
  });
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "active error" });
  await waitFor(() => chrome.openPopupCalls === 1);

  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, { provider: "codex", message: "active error" });

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_b", "ollama", "background error"),
  });
  await waitForStoredTabError(chrome, 22, { provider: "ollama", message: "background error" });
  await waitForProviderErrorState(popup, { provider: "codex", message: "active error" });
  assert.equal(chrome.openPopupCalls, 1);

  emitPageActivity(tabA, false);
  setCurrentBrowserTab(chrome, 22);
  emitPageActivity(tabB, true);
  await waitForProviderErrorState(popup, { provider: "ollama", message: "background error" });
  await waitFor(() => chrome.openPopupCalls === 2, "retained inactive-tab error did not auto-open on reactivation");
  assert.equal(background.getActiveSdkTabId(), 22);
});

test("switching to a non-Pedelec tab clears popup provider-error state", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/page", 17);
  setCurrentBrowserTab(chrome, 17);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_a");
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a"),
  });
  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, { provider: "codex", message: "provider failed" });

  setCurrentBrowserTab(chrome, 99);
  emitPageActivity(sdk, false);
  await waitForProviderErrorState(popup, null);
  assert.equal(background.getActiveSdkTabId(), null);
  await waitForStoredTabError(chrome, 17, { provider: "codex", message: "provider failed" });
});

test("a session routed to multiple tabs stores the provider error on every routed tab", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const tabA = connectExternal(chrome, "https://app.example.test/a", 11);
  const tabB = connectExternal(chrome, "https://app.example.test/b", 22);
  emitPageActivity(tabA, true);
  await createSdkSession(background, tabA, native, "thread_shared", false);
  const nativeMessagesBeforeResume = native.sent.length;
  tabB.emit({
    channelId: "channel_b",
    requestId: "resume_shared",
    type: "resume_session",
    sessionId: "thread_shared",
  });
  await respondToNative(background, native, {}, nativeMessagesBeforeResume + 1);
  await waitFor(() => tabB.sent.some((message) => message.requestId === "resume_shared"));

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_shared", "codex", "shared failure"),
  });
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "shared failure" });
  await waitForStoredTabError(chrome, 22, { provider: "codex", message: "shared failure" });
});

test("unrouted provider errors do not become popup state and still dispatch later events", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/page", 17);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_routed");
  const sentBefore = sdk.sent.length;

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_unknown", "codex", "unrouted"),
  });
  await flush();
  assert.equal(storedProviderError(chrome, 17), null);
  assert.equal(chrome.storage.session.data.latestProviderError, undefined);
  assert.equal(sdk.sent.length, sentBefore);
  assert.equal(chrome.openPopupCalls, 0);

  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, null);

  background.handleNativeMessage({
    type: "thread_event",
    event: { threadId: "thread_routed", type: "assistant_delta", seq: 2, text: "still routed" },
  });
  await waitFor(() => sdk.sent.some((message) => message.type === "chat_delta" && message.text === "still routed"));
});

test("dismiss clears only the currently active tab provider error", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const tabA = connectExternal(chrome, "https://app.example.test/a", 11);
  const tabB = connectExternal(chrome, "https://app.example.test/b", 22);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(tabA, true);
  await createSdkSession(background, tabA, native, "thread_a");
  emitPageActivity(tabA, false);
  setCurrentBrowserTab(chrome, 22);
  emitPageActivity(tabB, true);
  await createSdkSession(background, tabB, native, "thread_b");

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "keep me"),
  });
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_b", "ollama", "dismiss me"),
  });
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "keep me" });
  await waitForStoredTabError(chrome, 22, { provider: "ollama", message: "dismiss me" });

  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, { provider: "ollama", message: "dismiss me" });
  popup.emit({ type: "dismiss_provider_error" });
  await waitForProviderErrorState(popup, null);
  await waitForStoredTabError(chrome, 22, null);
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "keep me" });
});

test("disconnecting the last SDK port for a tab clears its retained provider error", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/page", 17);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_a");
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a"),
  });
  await waitForStoredTabError(chrome, 17, { provider: "codex", message: "provider failed" });

  sdk.disconnect();
  await waitForStoredTabError(chrome, 17, null);
  assert.equal(background.getActiveSdkTabId(), null);

  const reconnected = connectExternal(chrome, "https://app.example.test/page", 17);
  emitPageActivity(reconnected, true);
  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, null);
  assert.equal(storedProviderError(chrome, 17), null);
});

test("provider-error storage failures remain non-fatal to thread event routing", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/page", 17);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_a");
  chrome.storage.session.set = () => {
    throw new Error("session storage failed");
  };

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "still dispatched"),
  });
  await waitFor(() =>
    sdk.sent.some(
      (message) =>
        message.type === "error" &&
        message.sessionId === "thread_a" &&
        message.error?.message === "still dispatched"
    )
  );
  background.handleNativeMessage({
    type: "thread_event",
    event: { threadId: "thread_a", type: "assistant_delta", seq: 2, text: "after storage failure" },
  });
  await waitFor(() =>
    sdk.sent.some((message) => message.type === "chat_delta" && message.text === "after storage failure")
  );
});

test("ports without a tab id cannot participate in popup provider-error routing", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome);
  emitPageActivity(sdk, true);
  assert.equal(background.getActiveSdkTabId(), null);
  await createSdkSession(background, sdk, native, "thread_untabbed");
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_untabbed"),
  });
  await waitFor(() =>
    sdk.sent.some((message) => message.type === "error" && message.sessionId === "thread_untabbed")
  );
  await flush();
  assert.equal(chrome.storage.session.data.providerErrorByTab, undefined);
  assert.equal(chrome.storage.session.data.latestProviderError, undefined);
  assert.equal(chrome.openPopupCalls, 0);

  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, null);
});

test("a newer provider error replaces the previous error for the same tab", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/page", 17);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_a");
  await createSdkSession(background, sdk, native, "thread_b");
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "first"),
  });
  await waitForStoredTabError(chrome, 17, { provider: "codex", message: "first" });
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_b", "ollama", "second"),
  });
  await waitForStoredTabError(chrome, 17, { provider: "ollama", message: "second" });
});

test("stale SDK activity in an unfocused window does not leak provider errors into the current tab", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/a", 11);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_a");
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "window a failed"),
  });
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "window a failed" });
  assert.equal(background.getActiveSdkTabId(), 11);

  setCurrentBrowserTab(chrome, 99);
  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, null);
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "window a failed" });
  assert.equal(background.getActiveSdkTabId(), 11);
  assert.deepEqual(lastFocusedTabQueries(chrome).at(-1), { active: true, lastFocusedWindow: true });

  const openPopupCalls = chrome.openPopupCalls;
  setCurrentBrowserTab(chrome, 11);
  const returnedPopup = connectPopup(background);
  await waitForProviderErrorState(returnedPopup, { provider: "codex", message: "window a failed" });
  assert.equal(chrome.openPopupCalls, openPopupCalls);
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "window a failed" });
});

test("provider errors from an unfocused Pedelec tab are stored without auto-opening", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/a", 11);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_a");
  assert.equal(background.getActiveSdkTabId(), 11);

  setCurrentBrowserTab(chrome, 99);
  const sentBefore = sdk.sent.length;
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "background window failed"),
  });
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "background window failed" });
  await waitFor(() =>
    sdk.sent.some(
      (message) =>
        message.type === "error" &&
        message.sessionId === "thread_a" &&
        message.error?.message === "background window failed"
    )
  );
  assert.ok(sdk.sent.length > sentBefore);
  assert.equal(chrome.openPopupCalls, 0);
  assert.equal(background.getActiveSdkTabId(), 11);
});

test("toolbar page blur still resolves popup provider errors from the current tab", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/page", 11);
  const sensitiveTab = {
    id: 11,
    get url() {
      throw new Error("url should not be read");
    },
    get pendingUrl() {
      throw new Error("pendingUrl should not be read");
    },
    get title() {
      throw new Error("title should not be read");
    },
    get favIconUrl() {
      throw new Error("favIconUrl should not be read");
    },
  };
  setCurrentBrowserTab(chrome, 11, sensitiveTab);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_a");
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "keep visible"),
  });
  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, { provider: "codex", message: "keep visible" });

  const statesBeforeBlur = popup.sent.filter((message) => message.type === "provider_error_state").length;
  emitPageActivity(sdk, false);
  await waitFor(() => background.getActiveSdkTabId() === null);
  await waitFor(
    () => popup.sent.filter((message) => message.type === "provider_error_state").length > statesBeforeBlur,
    "blur did not refresh popup provider-error state"
  );
  await waitForProviderErrorState(popup, { provider: "codex", message: "keep visible" });
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "keep visible" });
});

test("dismiss targets the actual current browser tab instead of stale SDK activity", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const tabA = connectExternal(chrome, "https://app.example.test/a", 11);
  const tabB = connectExternal(chrome, "https://app.example.test/b", 22);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(tabA, true);
  await createSdkSession(background, tabA, native, "thread_a");
  emitPageActivity(tabA, false);
  setCurrentBrowserTab(chrome, 22);
  emitPageActivity(tabB, true);
  await createSdkSession(background, tabB, native, "thread_b");
  emitPageActivity(tabB, false);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(tabA, true);
  assert.equal(background.getActiveSdkTabId(), 11);

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "keep tab 11"),
  });
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_b", "ollama", "tab 22 error"),
  });
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "keep tab 11" });
  await waitForStoredTabError(chrome, 22, { provider: "ollama", message: "tab 22 error" });

  setCurrentBrowserTab(chrome, 99);
  const otherTabPopup = connectPopup(background);
  await waitForProviderErrorState(otherTabPopup, null);
  otherTabPopup.emit({ type: "dismiss_provider_error" });
  await waitForProviderErrorState(otherTabPopup, null);
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "keep tab 11" });
  await waitForStoredTabError(chrome, 22, { provider: "ollama", message: "tab 22 error" });
  assert.equal(background.getActiveSdkTabId(), 11);

  setCurrentBrowserTab(chrome, 22);
  const tabBPopup = connectPopup(background);
  await waitForProviderErrorState(tabBPopup, { provider: "ollama", message: "tab 22 error" });
  tabBPopup.emit({ type: "dismiss_provider_error" });
  await waitForProviderErrorState(tabBPopup, null);
  await waitForStoredTabError(chrome, 22, null);
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "keep tab 11" });
});

test("current tab query failure fails closed without affecting SDK routing", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/page", 11);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_a");
  assert.equal(background.getActiveSdkTabId(), 11);

  chrome.tabsQueryError = { message: "tabs.query failed" };
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "still stored"),
  });
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "still stored" });
  await waitFor(() =>
    sdk.sent.some(
      (message) =>
        message.type === "error" &&
        message.sessionId === "thread_a" &&
        message.error?.message === "still stored"
    )
  );
  assert.equal(chrome.openPopupCalls, 0);

  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, null);
  popup.emit({ type: "dismiss_provider_error" });
  await waitForProviderErrorState(popup, null);
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "still stored" });

  background.handleNativeMessage({
    type: "thread_event",
    event: { threadId: "thread_a", type: "assistant_delta", seq: 2, text: "still routed" },
  });
  await waitFor(() => sdk.sent.some((message) => message.type === "chat_delta" && message.text === "still routed"));

  chrome.tabsQueryError = null;
  chrome.tabsQueryThrow = true;
  const throwingPopup = connectPopup(background);
  await waitForProviderErrorState(throwingPopup, null);
  throwingPopup.emit({ type: "dismiss_provider_error" });
  await waitForProviderErrorState(throwingPopup, null);
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "still stored" });
});

test("active-tab provider errors auto-open immediately once and do not reopen on reactivation", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/page", 17);
  setCurrentBrowserTab(chrome, 17);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_active");

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_active", "codex", "quota exceeded"),
  });
  await waitForStoredTabError(chrome, 17, { provider: "codex", message: "quota exceeded" });
  await waitFor(() => chrome.openPopupCalls === 1, "openPopup was not called for the active tab");
  assert.equal(storedProviderError(chrome, 17)?.autoPopupConsumed, true);

  emitPageActivity(sdk, false);
  setCurrentBrowserTab(chrome, 99);
  emitPageActivity(sdk, true);
  await flush();
  assert.equal(chrome.openPopupCalls, 1);

  setCurrentBrowserTab(chrome, 17);
  emitPageActivity(sdk, true);
  await flush();
  assert.equal(chrome.openPopupCalls, 1);
  await waitForStoredTabError(chrome, 17, { provider: "codex", message: "quota exceeded" });
});

test("inactive-tab provider errors auto-open once when the tab becomes active", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const tabA = connectExternal(chrome, "https://app.example.test/a", 11);
  const tabB = connectExternal(chrome, "https://app.example.test/b", 22);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(tabA, true);
  await createSdkSession(background, tabA, native, "thread_a");
  await createSdkSession(background, tabB, native, "thread_b", true, "channel_b");

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_b", "ollama", "background error"),
  });
  await waitForStoredTabError(chrome, 22, { provider: "ollama", message: "background error" });
  assert.equal(chrome.openPopupCalls, 0);
  assert.equal(storedProviderError(chrome, 22)?.autoPopupConsumed, false);

  emitPageActivity(tabA, false);
  setCurrentBrowserTab(chrome, 22);
  emitPageActivity(tabB, true);
  await waitFor(() => chrome.openPopupCalls === 1, "openPopup was not called after reactivation");
  assert.equal(storedProviderError(chrome, 22)?.autoPopupConsumed, true);

  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, { provider: "ollama", message: "background error" });
  assert.deepEqual(Object.keys(lastProviderErrorState(popup).providerError).sort(), ["message", "provider"]);

  emitPageActivity(tabB, false);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(tabA, true);
  emitPageActivity(tabA, false);
  setCurrentBrowserTab(chrome, 22);
  emitPageActivity(tabB, true);
  emitPageActivity(tabB, true);
  await flush();
  assert.equal(chrome.openPopupCalls, 1);
  await waitForProviderErrorState(popup, { provider: "ollama", message: "background error" });
});

test("a newer provider error can auto-open again even with identical text", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/page", 17);
  setCurrentBrowserTab(chrome, 17);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_a");

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "same message", 1),
  });
  await waitFor(() => chrome.openPopupCalls === 1);
  assert.equal(storedProviderError(chrome, 17)?.autoPopupConsumed, true);

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "same message", 2),
  });
  await waitFor(() => chrome.openPopupCalls === 2, "identical text with a new occurrence did not auto-open");
  assert.equal(storedProviderError(chrome, 17)?.seq, 2);
  assert.equal(storedProviderError(chrome, 17)?.autoPopupConsumed, true);

  emitPageActivity(sdk, false);
  setCurrentBrowserTab(chrome, 99);
  setCurrentBrowserTab(chrome, 17);
  emitPageActivity(sdk, true);
  await flush();
  assert.equal(chrome.openPopupCalls, 2);
});

test("only the latest inactive-tab provider error auto-opens on reactivation", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const tabA = connectExternal(chrome, "https://app.example.test/a", 11);
  const tabB = connectExternal(chrome, "https://app.example.test/b", 22);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(tabA, true);
  await createSdkSession(background, tabA, native, "thread_a");
  await createSdkSession(background, tabB, native, "thread_b", true, "channel_b");

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_b", "codex", "first inactive", 1),
  });
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_b", "ollama", "latest inactive", 2),
  });
  await waitForStoredTabError(chrome, 22, { provider: "ollama", message: "latest inactive" });
  assert.equal(chrome.openPopupCalls, 0);
  assert.equal(storedProviderError(chrome, 22)?.seq, 2);

  const popup = connectPopup(background);
  emitPageActivity(tabA, false);
  setCurrentBrowserTab(chrome, 22);
  emitPageActivity(tabB, true);
  await waitFor(() => chrome.openPopupCalls === 1, "latest inactive error did not auto-open once");
  await waitForProviderErrorState(popup, { provider: "ollama", message: "latest inactive" });
  assert.equal(storedProviderError(chrome, 22)?.autoPopupConsumed, true);

  emitPageActivity(tabB, false);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(tabA, true);
  emitPageActivity(tabA, false);
  setCurrentBrowserTab(chrome, 22);
  emitPageActivity(tabB, true);
  await flush();
  assert.equal(chrome.openPopupCalls, 1);
});

test("stale page activity from an unfocused window does not consume a pending auto-popup", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/a", 11);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_a");

  setCurrentBrowserTab(chrome, 99);
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "unfocused failure"),
  });
  await waitForStoredTabError(chrome, 11, { provider: "codex", message: "unfocused failure" });
  assert.equal(chrome.openPopupCalls, 0);

  emitPageActivity(sdk, true);
  await flush();
  assert.equal(chrome.openPopupCalls, 0);
  assert.equal(storedProviderError(chrome, 11)?.autoPopupConsumed, false);

  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(sdk, true);
  await waitFor(() => chrome.openPopupCalls === 1, "pending error did not auto-open after becoming current");
  assert.equal(storedProviderError(chrome, 11)?.autoPopupConsumed, true);

  emitPageActivity(sdk, false);
  setCurrentBrowserTab(chrome, 99);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(sdk, true);
  await flush();
  assert.equal(chrome.openPopupCalls, 1);
});

test("dismiss and last-port disconnect clear auto-popup notification metadata", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const tabA = connectExternal(chrome, "https://app.example.test/a", 11);
  const tabB = connectExternal(chrome, "https://app.example.test/b", 22);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(tabA, true);
  await createSdkSession(background, tabA, native, "thread_a");
  await createSdkSession(background, tabB, native, "thread_b", true, "channel_b");

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "dismiss me"),
  });
  await waitFor(() => chrome.openPopupCalls === 1);

  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, { provider: "codex", message: "dismiss me" });
  popup.emit({ type: "dismiss_provider_error" });
  await waitForStoredTabError(chrome, 11, null);
  await waitForProviderErrorState(popup, null);

  emitPageActivity(tabA, false);
  setCurrentBrowserTab(chrome, 22);
  emitPageActivity(tabB, true);
  emitPageActivity(tabB, false);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(tabA, true);
  await flush();
  assert.equal(chrome.openPopupCalls, 1);
  assert.equal(storedProviderError(chrome, 11), null);

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_b", "ollama", "cleared by disconnect", 3),
  });
  await waitForStoredTabError(chrome, 22, { provider: "ollama", message: "cleared by disconnect" });
  assert.equal(storedProviderError(chrome, 22)?.autoPopupConsumed, false);

  tabB.disconnect();
  await waitForStoredTabError(chrome, 22, null);

  const reconnected = connectExternal(chrome, "https://app.example.test/b", 22);
  emitPageActivity(tabA, false);
  setCurrentBrowserTab(chrome, 22);
  emitPageActivity(reconnected, true);
  await flush();
  assert.equal(chrome.openPopupCalls, 1);
  assert.equal(storedProviderError(chrome, 22), null);
});

test("one-shot auto-popup state survives in-memory cache reconstruction", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/page", 17);
  setCurrentBrowserTab(chrome, 17);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_a");
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "persisted once"),
  });
  await waitFor(() => chrome.openPopupCalls === 1);
  assert.equal(storedProviderError(chrome, 17)?.autoPopupConsumed, true);

  const reconstructed = createBackground(chrome, { disableReconnect: true });
  const resumed = new MockPort();
  resumed.name = "pedelec-sdk-external";
  resumed.sender = { url: "https://app.example.test/page", tab: { id: 17 } };
  reconstructed.handleSdkExternalConnect(resumed);
  setCurrentBrowserTab(chrome, 17);
  const popup = connectPopup(reconstructed);
  await waitForProviderErrorState(popup, { provider: "codex", message: "persisted once" });
  assert.equal(storedProviderError(chrome, 17)?.autoPopupConsumed, true);

  emitPageActivity(resumed, false);
  setCurrentBrowserTab(chrome, 99);
  emitPageActivity(resumed, true);
  await waitForProviderErrorState(popup, null);
  assert.equal(chrome.openPopupCalls, 1);

  setCurrentBrowserTab(chrome, 17);
  emitPageActivity(resumed, true);
  await waitForProviderErrorState(popup, { provider: "codex", message: "persisted once" });
  assert.equal(chrome.openPopupCalls, 1);
  assert.equal(storedProviderError(chrome, 17)?.autoPopupConsumed, true);
});

test("failed openPopup leaves an unseen error eligible for a later auto-open", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  chrome.action.openPopup = async () => {
    chrome.openPopupCalls += 1;
    throw new Error("popup unavailable");
  };
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const sdk = connectExternal(chrome, "https://app.example.test/page", 17);
  setCurrentBrowserTab(chrome, 17);
  emitPageActivity(sdk, true);
  await createSdkSession(background, sdk, native, "thread_a");
  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_a", "codex", "retry me"),
  });
  await waitFor(() => chrome.openPopupCalls === 1);
  await waitForStoredTabError(chrome, 17, { provider: "codex", message: "retry me" });
  assert.equal(storedProviderError(chrome, 17)?.autoPopupConsumed, false);

  chrome.action.openPopup = async () => {
    chrome.openPopupCalls += 1;
  };
  emitPageActivity(sdk, false);
  setCurrentBrowserTab(chrome, 99);
  setCurrentBrowserTab(chrome, 17);
  emitPageActivity(sdk, true);
  await waitFor(() => chrome.openPopupCalls === 2, "failed openPopup was not retried on reactivation");
  assert.equal(storedProviderError(chrome, 17)?.autoPopupConsumed, true);
});

test("manual popup display does not consume a pending auto-popup", async () => {
  const chrome = createChrome();
  const native = new MockPort();
  chrome.nativePortQueue.push(native);
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();

  const tabA = connectExternal(chrome, "https://app.example.test/a", 11);
  const tabB = connectExternal(chrome, "https://app.example.test/b", 22);
  setCurrentBrowserTab(chrome, 11);
  emitPageActivity(tabA, true);
  await createSdkSession(background, tabA, native, "thread_a");
  await createSdkSession(background, tabB, native, "thread_b", true, "channel_b");

  background.handleNativeMessage({
    type: "thread_event",
    event: providerErrorEvent("thread_b", "ollama", "pending until active"),
  });
  await waitForStoredTabError(chrome, 22, { provider: "ollama", message: "pending until active" });

  setCurrentBrowserTab(chrome, 22);
  const popup = connectPopup(background);
  await waitForProviderErrorState(popup, { provider: "ollama", message: "pending until active" });
  assert.equal(chrome.openPopupCalls, 0);
  assert.equal(storedProviderError(chrome, 22)?.autoPopupConsumed, false);

  emitPageActivity(tabA, false);
  emitPageActivity(tabB, true);
  await waitFor(() => chrome.openPopupCalls === 1, "manual popup open consumed the auto-popup opportunity");
  assert.equal(storedProviderError(chrome, 22)?.autoPopupConsumed, true);
});

test("provider-error reconciliation does not add tabs or host permissions", () => {
  const fs = require("node:fs");
  const path = require("node:path");
  const manifest = JSON.parse(fs.readFileSync(path.join(__dirname, "manifest.json"), "utf8"));
  assert.equal(manifest.permissions.includes("tabs"), false);
  assert.equal(Array.isArray(manifest.host_permissions), false);
  assert.equal("host_permissions" in manifest, false);
  assert.equal(manifest.permissions.includes("activeTab"), true);
});
