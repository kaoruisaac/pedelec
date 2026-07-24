const HOST_NAME = "cc.isaaclin.pedelec";
const DEMO_PROVIDER = "codex";
const DEMO_SKILLS = {
  guidance:
    "Use get_app_state when you need the current extension demo state. Use update_counter to change the visible counter.",
  tools: [
    {
      name: "get_app_state",
      description: "Get the current extension demo state.",
      argsSchema: {
        type: "object",
        properties: {},
        required: [],
        additionalProperties: false,
      },
      timeoutMs: 60000,
    },
    {
      name: "update_counter",
      description: "Update the visible counter by delta.",
      argsSchema: {
        type: "object",
        properties: {
          delta: { type: "number" },
        },
        required: ["delta"],
        additionalProperties: false,
      },
      timeoutMs: 3000,
    },
  ],
};
const INITIAL_RECONNECT_DELAY_MS = 1000;
const MAX_RECONNECT_DELAY_MS = 30000;
const MAX_EVENTS = 80;
const SDK_INTERNAL_PORT_NAME = "pedelec-sdk-internal";
const SDK_EXTERNAL_PORT_NAME = "pedelec-sdk-external";
const APPROVED_ORIGINS_STORAGE_KEY = "approvedOrigins";
const LATEST_PROVIDER_ERROR_STORAGE_KEY = "latestProviderError";
const DEFAULT_APPROVAL_TIMEOUT_MS = 60000;

function createBackground(runtimeChrome, options = {}) {
  let nativePort = null;
  let reconnectTimer = null;
  let reconnectDelayMs = INITIAL_RECONNECT_DELAY_MS;
  let nextRequestNumber = 1;
  let pendingIntentionalDisconnects = 0;
  let nativeOperationDepth = 0;

  const popupPorts = new Set();
  const pendingRequests = new Map();
  const activeThreadIds = new Set();
  const sdkPorts = new Set();
  const sdkChannelsByPort = new Map();
  const sdkContextsByPort = new Map();
  const sdkRoutesBySession = new Map();
  const sdkLifecycleBySession = new Map();
  const approvalTimeoutMs = options.approvalTimeoutMs ?? DEFAULT_APPROVAL_TIMEOUT_MS;
  let pendingApproval = null;
  let providerErrorOperation = Promise.resolve();

  let state = {
    connected: false,
    threadId: null,
    threadStatus: "notCreated",
    counter: 0,
    error: null,
    events: [],
  };

  function nextRequestId() {
    return `ext_${Date.now()}_${nextRequestNumber++}`;
  }

  function normalizeError(err, fallbackCode = "SDK_TRANSPORT_ERROR", fallbackMessage = "Pedelec transport error") {
    if (!err) return { code: fallbackCode, message: fallbackMessage };
    if (typeof err === "string") return { code: fallbackCode, message: err };
    if (err.code && err.message) return err;
    return {
      code: fallbackCode,
      message: err.message || fallbackMessage,
      details: err.details,
    };
  }

  function setState(partial) {
    state = { ...state, ...partial };
    broadcastState();
  }

  function appendEvent(event) {
    state = {
      ...state,
      events: [event, ...state.events].slice(0, MAX_EVENTS),
    };
    broadcastState();
  }

  function broadcastState() {
    const message = { type: "state", state };
    for (const port of popupPorts) {
      try {
        port.postMessage(message);
      } catch (_err) {
        popupPorts.delete(port);
      }
    }
  }

  function broadcastProviderErrorState(providerError) {
    const message = { type: "provider_error_state", providerError };
    for (const port of popupPorts) {
      try {
        port.postMessage(message);
      } catch (_err) {
        popupPorts.delete(port);
      }
    }
  }

  function scheduleReconnect() {
    if (options.disableReconnect || reconnectTimer || activeThreadIds.size === 0) return;

    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      if (activeThreadIds.size === 0) return;
      connectNative();
    }, reconnectDelayMs);

    reconnectDelayMs = Math.min(reconnectDelayMs * 2, MAX_RECONNECT_DELAY_MS);
  }

  function clearReconnectTimer() {
    if (!reconnectTimer) return;
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }

  function isNativeIdle() {
    return activeThreadIds.size === 0 && pendingRequests.size === 0 && nativeOperationDepth === 0;
  }

  function maybeDisconnectNativeIfIdle() {
    if (!nativePort || !isNativeIdle()) return;

    const port = nativePort;
    nativePort = null;
    pendingIntentionalDisconnects += 1;
    clearReconnectTimer();
    reconnectDelayMs = INITIAL_RECONNECT_DELAY_MS;
    setState({
      connected: false,
      error: null,
    });
    try {
      port.disconnect();
    } catch (_err) {
      pendingIntentionalDisconnects = Math.max(0, pendingIntentionalDisconnects - 1);
    }
  }

  function getPendingApprovalState() {
    if (!pendingApproval) return null;
    return {
      origin: pendingApproval.origin,
      requestedAt: pendingApproval.requestedAt,
      requestCount: pendingApproval.requests.length,
    };
  }

  async function getPopupApprovalState() {
    let pending = getPendingApprovalState();
    if (pending?.origin) {
      return {
        ...pending,
        approved: false,
        pending: true,
      };
    }

    const origin = await getActiveTabOrigin();
    pending = getPendingApprovalState();
    if (pending?.origin) {
      return {
        ...pending,
        approved: false,
        pending: true,
      };
    }

    if (!origin) {
      return {
        origin: null,
        approved: false,
        pending: false,
      };
    }

    return {
      origin,
      approved: await isOriginApproved(origin),
      pending: false,
    };
  }

  async function postPopupApprovalState(port) {
    try {
      let approvalState = await getPopupApprovalState();
      const pending = getPendingApprovalState();
      if (!approvalState.pending && pending?.origin) {
        approvalState = {
          ...pending,
          approved: false,
          pending: true,
        };
      }
      port.postMessage({ type: "approval_state", approvalState });
    } catch (_err) {
      popupPorts.delete(port);
    }
  }

  function broadcastPopupApprovalState() {
    for (const port of popupPorts) {
      postPopupApprovalState(port);
    }
  }

  function storageAreaGet(area, key) {
    if (!area?.get) return Promise.resolve({});

    return new Promise((resolve, reject) => {
      let settled = false;
      const done = (result) => {
        if (settled) return;
        settled = true;
        const error = runtimeChrome.runtime?.lastError;
        if (error) reject(normalizeError(error, "STORAGE_ERROR", "Extension storage failed."));
        else resolve(result || {});
      };

      try {
        const maybePromise = area.get(key, done);
        if (maybePromise?.then) {
          maybePromise.then(done, reject);
        }
      } catch (err) {
        reject(normalizeError(err, "STORAGE_ERROR", "Extension storage failed."));
      }
    });
  }

  function storageAreaSet(area, value) {
    if (!area?.set) return Promise.resolve();

    return new Promise((resolve, reject) => {
      let settled = false;
      const done = () => {
        if (settled) return;
        settled = true;
        const error = runtimeChrome.runtime?.lastError;
        if (error) reject(normalizeError(error, "STORAGE_ERROR", "Extension storage failed."));
        else resolve();
      };

      try {
        const maybePromise = area.set(value, done);
        if (maybePromise?.then) {
          maybePromise.then(done, reject);
        }
      } catch (err) {
        reject(normalizeError(err, "STORAGE_ERROR", "Extension storage failed."));
      }
    });
  }

  function storageAreaRemove(area, key) {
    if (!area?.remove) return Promise.resolve();

    return new Promise((resolve, reject) => {
      let settled = false;
      const done = () => {
        if (settled) return;
        settled = true;
        const error = runtimeChrome.runtime?.lastError;
        if (error) reject(normalizeError(error, "STORAGE_ERROR", "Extension storage failed."));
        else resolve();
      };

      try {
        const maybePromise = area.remove(key, done);
        if (maybePromise?.then) {
          maybePromise.then(done, reject);
        }
      } catch (err) {
        reject(normalizeError(err, "STORAGE_ERROR", "Extension storage failed."));
      }
    });
  }

  function storageGet(key) {
    return storageAreaGet(runtimeChrome.storage?.local, key);
  }

  function storageSet(value) {
    return storageAreaSet(runtimeChrome.storage?.local, value);
  }

  function hasSessionStorage() {
    return Boolean(runtimeChrome.storage?.session);
  }

  function sessionStorageGet(key) {
    return storageAreaGet(runtimeChrome.storage?.session, key);
  }

  function sessionStorageSet(value) {
    return storageAreaSet(runtimeChrome.storage?.session, value);
  }

  function sessionStorageRemove(key) {
    return storageAreaRemove(runtimeChrome.storage?.session, key);
  }

  function isProviderErrorEvent(event) {
    return event?.type === "error" && event?.source === "provider";
  }

  function getUnknownProviderErrorMessage() {
    try {
      const message = runtimeChrome.i18n?.getMessage("unknownProviderError");
      return typeof message === "string" && message.trim() ? message : "Unknown provider error";
    } catch (_err) {
      return "Unknown provider error";
    }
  }

  function providerErrorStateFromEvent(event) {
    if (typeof event?.provider !== "string" || !event.provider.trim()) return null;

    let message = "";
    if (typeof event.error?.message === "string" && event.error.message.trim()) {
      message = event.error.message;
    } else if (typeof event.error === "string" && event.error.trim()) {
      message = event.error;
    } else {
      message = formatError(event.error);
    }

    return {
      provider: event.provider,
      message: typeof message === "string" && message.trim() ? message : getUnknownProviderErrorMessage(),
    };
  }

  function postPopupProviderErrorState(port) {
    if (!hasSessionStorage()) return;
    return enqueueProviderErrorOperation(async () => {
      try {
        const result = await sessionStorageGet(LATEST_PROVIDER_ERROR_STORAGE_KEY);
        if (!popupPorts.has(port)) return;
        port.postMessage({
          type: "provider_error_state",
          providerError: result?.[LATEST_PROVIDER_ERROR_STORAGE_KEY] || null,
        });
      } catch (_err) {
        // Provider errors are an optional popup side effect.
      }
    });
  }

  async function openProviderErrorPopup() {
    if (!runtimeChrome.action?.openPopup) return;
    try {
      await runtimeChrome.action.openPopup();
    } catch (_err) {
      // The error remains in session storage for the next manual popup open.
    }
  }

  function enqueueProviderErrorOperation(operation) {
    providerErrorOperation = providerErrorOperation
      .catch(() => {})
      .then(operation)
      .catch(() => {});
    return providerErrorOperation;
  }

  function handleProviderErrorPopupSideEffect(event) {
    if (!isProviderErrorEvent(event) || !hasSessionStorage()) return;
    const providerError = providerErrorStateFromEvent(event);
    if (!providerError) return;

    enqueueProviderErrorOperation(async () => {
      await sessionStorageSet({ [LATEST_PROVIDER_ERROR_STORAGE_KEY]: providerError });
      broadcastProviderErrorState(providerError);
      await openProviderErrorPopup();
    });
  }

  function dismissProviderError() {
    if (!hasSessionStorage()) return Promise.resolve();
    return enqueueProviderErrorOperation(async () => {
      await sessionStorageRemove(LATEST_PROVIDER_ERROR_STORAGE_KEY);
      broadcastProviderErrorState(null);
    });
  }

  async function readApprovedOrigins() {
    const result = await storageGet(APPROVED_ORIGINS_STORAGE_KEY);
    const records = result?.[APPROVED_ORIGINS_STORAGE_KEY];
    return Array.isArray(records)
      ? records.filter((record) => record && typeof record.origin === "string")
      : [];
  }

  async function isOriginApproved(origin) {
    if (!origin) return false;
    const records = await readApprovedOrigins();
    return records.some((record) => record.origin === origin);
  }

  async function approveOrigin(origin) {
    const records = await readApprovedOrigins();
    const withoutOrigin = records.filter((record) => record.origin !== origin);
    withoutOrigin.push({ origin, approvedAt: Date.now() });
    await storageSet({ [APPROVED_ORIGINS_STORAGE_KEY]: withoutOrigin });
  }

  async function revokeOrigin(origin) {
    const records = await readApprovedOrigins();
    await storageSet({
      [APPROVED_ORIGINS_STORAGE_KEY]: records.filter((record) => record.origin !== origin),
    });
  }

  function originFromSender(sender) {
    const url = sender?.url || sender?.origin;
    if (typeof url !== "string" || !url.trim()) return null;
    try {
      const origin = new URL(url).origin;
      return origin === "null" ? null : origin;
    } catch (_err) {
      return null;
    }
  }

  function originFromUrl(url) {
    if (typeof url !== "string" || !url.trim()) return null;
    try {
      const parsed = new URL(url);
      if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return null;
      return parsed.origin;
    } catch (_err) {
      return null;
    }
  }

  async function getActiveTabOrigin() {
    const tabsApi = runtimeChrome.tabs;
    if (!tabsApi?.query) return null;

    return new Promise((resolve) => {
      let settled = false;
      const done = (tabs) => {
        if (settled) return;
        settled = true;
        resolve(originFromUrl(Array.isArray(tabs) ? tabs[0]?.url : null));
      };

      try {
        const maybePromise = tabsApi.query({ active: true, currentWindow: true }, done);
        if (maybePromise?.then) {
          maybePromise.then(done, () => done([]));
        }
      } catch (_err) {
        done([]);
      }
    });
  }

  async function approvePopupOrigin(origin) {
    const normalizedOrigin = originFromUrl(origin);
    if (!normalizedOrigin) {
      throw {
        code: "INVALID_ORIGIN",
        message: "A valid http or https origin is required.",
      };
    }

    if (pendingApproval?.origin === normalizedOrigin) {
      await approvePendingApproval();
      return;
    }

    await approveOrigin(normalizedOrigin);
    broadcastPopupApprovalState();
  }

  async function revokePopupOrigin(origin) {
    const normalizedOrigin = originFromUrl(origin);
    if (!normalizedOrigin) {
      throw {
        code: "INVALID_ORIGIN",
        message: "A valid http or https origin is required.",
      };
    }

    await revokeOrigin(normalizedOrigin);
    broadcastPopupApprovalState();
  }

  async function openApprovalPopup() {
    if (!runtimeChrome.action?.openPopup) {
      throw {
        code: "OPEN_POPUP_FAILED",
        message: "Please click the Pedelec extension icon and approve this site.",
      };
    }

    try {
      await runtimeChrome.action.openPopup();
    } catch (err) {
      throw normalizeError(
        err,
        "OPEN_POPUP_FAILED",
        "Please click the Pedelec extension icon and approve this site."
      );
    }
  }

  async function withNativeOperation(operation) {
    nativeOperationDepth += 1;
    try {
      return await operation();
    } finally {
      nativeOperationDepth -= 1;
      maybeDisconnectNativeIfIdle();
    }
  }

  function connectNative() {
    if (nativePort) return true;

    try {
      nativePort = runtimeChrome.runtime.connectNative(HOST_NAME);
    } catch (err) {
      nativePort = null;
      setState({
        connected: false,
        error: err.message,
      });
      notifyAllSdkPorts({
        type: "error",
        error: normalizeError(err, "NATIVE_HOST_UNAVAILABLE", "Pedelec native host is not connected."),
      });
      scheduleReconnect();
      return false;
    }

    nativePort.onMessage.addListener(handleNativeMessage);
    nativePort.onDisconnect.addListener(handleNativeDisconnect);
    reconnectDelayMs = INITIAL_RECONNECT_DELAY_MS;
    setState({
      connected: true,
      error: null,
    });
    return true;
  }

  function handleNativeDisconnect() {
    if (pendingIntentionalDisconnects > 0) {
      pendingIntentionalDisconnects -= 1;
      return;
    }

    const err = runtimeChrome.runtime.lastError;
    const error = normalizeError(err, "NATIVE_CONNECTION_CLOSED", "Native host disconnected.");
    nativePort = null;
    for (const pending of pendingRequests.values()) {
      pending.reject(error);
    }
    pendingRequests.clear();
    setState({
      connected: false,
      error: error.message,
    });
    notifyAllSdkPorts({ type: "error", error });
    scheduleReconnect();
  }

  function handleNativeMessage(message) {
    reconnectDelayMs = INITIAL_RECONNECT_DELAY_MS;

    if (message?.type === "response") {
      const pending = pendingRequests.get(message.requestId);
      if (!pending) return;

      pendingRequests.delete(message.requestId);
      if (message.ok) {
        pending.resolve(message.result);
      } else {
        pending.reject(normalizeError(message.error, "IPC_UNAVAILABLE", "Native request failed"));
      }
      maybeDisconnectNativeIfIdle();
      return;
    }

    if (message?.type === "thread_event") {
      handleProviderErrorPopupSideEffect(message.event);
      applyThreadEvent(message.event);
      dispatchSdkThreadEvent(message.event);
      if (message.event?.type === "ended") {
        removeActiveThread(message.event.threadId);
        forgetSdkSession(message.event.threadId);
        maybeDisconnectNativeIfIdle();
      }
      return;
    }

    const text = `Unexpected native message: ${JSON.stringify(message)}`;
    setState({ error: text });
    notifyAllSdkPorts({
      type: "error",
      error: { code: "SDK_PROTOCOL_ERROR", message: text },
    });
  }

  function sendNativeRequest(type, payload = {}) {
    if (!connectNative()) {
      return Promise.reject({
        code: "NATIVE_HOST_UNAVAILABLE",
        message: "Pedelec native host is not connected.",
      });
    }

    const requestId = nextRequestId();
    const message = { type, requestId, ...payload };

    return new Promise((resolve, reject) => {
      pendingRequests.set(requestId, { resolve, reject });
      try {
        nativePort.postMessage(message);
      } catch (err) {
        pendingRequests.delete(requestId);
        const error = normalizeError(err, "NATIVE_CONNECTION_CLOSED", "Native host disconnected.");
        reject(error);
        nativePort = null;
        setState({
          connected: false,
          error: error.message,
        });
        notifyAllSdkPorts({ type: "error", error });
        scheduleReconnect();
        maybeDisconnectNativeIfIdle();
      }
    });
  }

  async function createThread() {
    return withNativeOperation(async () => {
      setState({
        error: null,
        events: [],
      });

      const result = await sendNativeRequest("create_thread", {
        provider: DEMO_PROVIDER,
        skills: DEMO_SKILLS,
      });
      const threadId = result?.threadId;
      if (!threadId) {
        throw new Error("create_thread response did not include threadId.");
      }

      try {
        await sendNativeRequest("subscribe_thread", { threadId });
      } catch (err) {
        await sendNativeRequest("end_thread", { threadId }).catch(() => {});
        throw err;
      }

      addActiveThread(threadId);
      setState({
        threadId,
        threadStatus: "idle",
      });
    });
  }

  async function sendText(message) {
    if (!state.threadId) {
      throw new Error("Create a thread first.");
    }
    await sendNativeRequest("send_text", {
      threadId: state.threadId,
      message,
    });
  }

  async function endThread() {
    if (!state.threadId) {
      throw new Error("No active thread.");
    }
    return withNativeOperation(async () => {
      const threadId = state.threadId;
      await sendNativeRequest("end_thread", { threadId });
      removeActiveThread(threadId);
      setState({
        threadStatus: "ended",
      });
    });
  }

  function applyThreadEvent(event) {
    if (!event) return;
    if (!state.threadId || event.threadId !== state.threadId) return;

    if (event.threadId) {
      state = { ...state, threadId: state.threadId || event.threadId };
    }

    if (event.type === "status_changed") {
      state = { ...state, threadStatus: event.status };
    } else if (event.type === "error") {
      state = {
        ...state,
        threadStatus: "error",
        error: formatError(event.error),
      };
    } else if (event.type === "ended") {
      state = { ...state, threadStatus: "ended" };
      removeActiveThread(event.threadId);
    } else if (event.type === "tool_call") {
      autoSubmitToolResult(event);
    }

    appendEvent(event);
  }

  function autoSubmitToolResult(event) {
    const result = runDemoTool(event);
    sendNativeRequest("submit_tool_result", {
      threadId: event.threadId,
      toolRequestId: event.requestId,
      result,
    }).catch((err) => {
      setState({ error: formatError(err) });
    });
  }

  function runDemoTool(event) {
    if (event.toolName === "get_app_state") {
      return {
        connected: state.connected,
        threadId: state.threadId,
        threadStatus: state.threadStatus,
        counter: state.counter,
        eventCount: state.events.length,
      };
    }

    if (event.toolName === "update_counter") {
      const delta = Number(event.args?.delta);
      const nextCounter = state.counter + delta;
      setState({ counter: nextCounter });
      return {
        counter: nextCounter,
        delta,
      };
    }

    return {
      error: {
        code: "TOOL_NOT_FOUND",
        message: `No demo handler for ${event.toolName}`,
      },
    };
  }

  function formatError(err) {
    if (!err) return "";
    if (typeof err === "string") return err;
    if (err.code && err.message) return `${err.code}: ${err.message}`;
    return err.message || JSON.stringify(err);
  }

  function postSdkResponse(port, channelId, requestId, ok, result, error) {
    const message = { channelId, type: "response", requestId, ok };
    if (ok) message.result = result ?? {};
    if (!ok) message.error = normalizeError(error, "SDK_TRANSPORT_ERROR", "SDK transport request failed");
    try {
      port.postMessage(message);
    } catch (_err) {
      disconnectSdkPort(port);
    }
  }

  function postSdkEvent(port, message) {
    try {
      port.postMessage(message);
    } catch (_err) {
      disconnectSdkPort(port);
    }
  }

  function rejectPendingApproval(error) {
    if (!pendingApproval) return;
    const current = pendingApproval;
    pendingApproval = null;
    clearTimeout(current.timeoutId);
    for (const request of current.requests) {
      postSdkResponse(request.port, request.channelId, request.requestId, false, null, error);
    }
    broadcastPopupApprovalState();
  }

  async function approvePendingApproval() {
    if (!pendingApproval) return;
    const current = pendingApproval;
    pendingApproval = null;
    clearTimeout(current.timeoutId);

    try {
      await approveOrigin(current.origin);
      broadcastPopupApprovalState();
      for (const request of current.requests) {
        handleSdkMessage(request.port, request.message, { skipApproval: true });
      }
    } catch (err) {
      const error = normalizeError(err, "STORAGE_ERROR", "Could not approve this site.");
      for (const request of current.requests) {
        postSdkResponse(request.port, request.channelId, request.requestId, false, null, error);
      }
      broadcastPopupApprovalState();
    }
  }

  async function ensureApprovedOrQueue(port, message, context) {
    const requestId = message?.requestId || "";
    const channelId = message?.channelId || "";
    const origin = context?.origin;

    if (!origin) {
      postSdkResponse(port, channelId, requestId, false, null, {
        code: "CREATE_SESSION_NOT_APPROVED",
        message: "Pedelec could not verify this site's origin.",
      });
      return false;
    }

    if (await isOriginApproved(origin)) return true;

    if (pendingApproval && pendingApproval.origin !== origin) {
      postSdkResponse(port, channelId, requestId, false, null, {
        code: "CREATE_SESSION_NOT_APPROVED",
        message: "Another site is already waiting for Pedelec approval.",
      });
      return false;
    }

    const pendingRequest = {
      port,
      message,
      requestId,
      channelId,
    };

    if (pendingApproval) {
      pendingApproval.requests.push(pendingRequest);
      broadcastPopupApprovalState();
      return false;
    }

    const timeoutId = setTimeout(() => {
      rejectPendingApproval({
        code: "APPROVAL_TIMEOUT",
        message: "Pedelec approval timed out.",
      });
    }, approvalTimeoutMs);
    timeoutId.unref?.();

    pendingApproval = {
      origin,
      requestedAt: Date.now(),
      requests: [pendingRequest],
      timeoutId,
    };
    broadcastPopupApprovalState();

    try {
      await openApprovalPopup();
    } catch (err) {
      rejectPendingApproval(err);
    }
    return false;
  }

  function notifyAllSdkPorts(message) {
    for (const port of sdkPorts) {
      postSdkEvent(port, message);
    }
  }

  function addActiveThread(threadId) {
    if (threadId) activeThreadIds.add(threadId);
  }

  function removeActiveThread(threadId) {
    if (threadId) activeThreadIds.delete(threadId);
    if (activeThreadIds.size === 0) clearReconnectTimer();
  }

  function addSdkSession(port, channelId, sessionId) {
    if (!channelId || !sessionId) return;
    if (!sdkChannelsByPort.has(port)) {
      sdkChannelsByPort.set(port, new Map());
    }
    const channels = sdkChannelsByPort.get(port);
    if (!channels.has(channelId)) {
      channels.set(channelId, new Set());
    }
    channels.get(channelId).add(sessionId);

    if (!sdkRoutesBySession.has(sessionId)) {
      sdkRoutesBySession.set(sessionId, new Map());
    }
    const routes = sdkRoutesBySession.get(sessionId);
    if (!routes.has(port)) {
      routes.set(port, new Set());
    }
    routes.get(port).add(channelId);
  }

  function removeSdkSession(port, channelId, sessionId) {
    const channels = sdkChannelsByPort.get(port);
    const sessions = channels?.get(channelId);
    sessions?.delete(sessionId);
    if (sessions?.size === 0) {
      channels.delete(channelId);
    }
    if (channels?.size === 0) {
      sdkChannelsByPort.delete(port);
    }

    const routes = sdkRoutesBySession.get(sessionId);
    const routeChannels = routes?.get(port);
    routeChannels?.delete(channelId);
    if (routeChannels?.size === 0) {
      routes.delete(port);
    }
    if (routes?.size === 0) {
      sdkRoutesBySession.delete(sessionId);
    }
  }

  function removeSdkSessionRoutes(sessionId) {
    const routes = sdkRoutesBySession.get(sessionId);
    if (!routes) return;
    for (const [port, channelIds] of Array.from(routes.entries())) {
      for (const channelId of Array.from(channelIds)) {
        removeSdkSession(port, channelId, sessionId);
      }
    }
  }

  function forgetSdkSession(sessionId) {
    removeSdkSessionRoutes(sessionId);
    sdkLifecycleBySession.delete(sessionId);
  }

  function shouldAutoEndSdkSession(sessionId) {
    const lifecycle = sdkLifecycleBySession.get(sessionId);
    return lifecycle?.autoEndOnDisconnect === true && !sdkRoutesBySession.has(sessionId);
  }

  async function autoEndSdkSession(sessionId) {
    try {
      await withNativeOperation(async () => {
        await sendNativeRequest("end_thread", { threadId: sessionId });
        removeActiveThread(sessionId);
        forgetSdkSession(sessionId);
      });
    } catch (err) {
      const normalized = normalizeError(err, "AUTO_END_SESSION_FAILED", "Could not end disconnected session.");
      const error = {
        code: "AUTO_END_SESSION_FAILED",
        message: normalized.message || "Could not end disconnected session.",
        details: normalized.details,
      };
      setState({ error: formatError(error) });
    }
  }

  function disconnectSdkPort(port) {
    sdkPorts.delete(port);
    sdkContextsByPort.delete(port);
    const channels = sdkChannelsByPort.get(port);
    const maybeAutoEndSessionIds = new Set();
    if (channels) {
      for (const [channelId, sessions] of Array.from(channels.entries())) {
        for (const sessionId of Array.from(sessions)) {
          removeSdkSession(port, channelId, sessionId);
          if (shouldAutoEndSdkSession(sessionId)) {
            maybeAutoEndSessionIds.add(sessionId);
          }
        }
      }
    }
    sdkChannelsByPort.delete(port);

    for (const sessionId of maybeAutoEndSessionIds) {
      autoEndSdkSession(sessionId);
    }

    if (pendingApproval) {
      pendingApproval.requests = pendingApproval.requests.filter((request) => request.port !== port);
      if (pendingApproval.requests.length === 0) {
        clearTimeout(pendingApproval.timeoutId);
        pendingApproval = null;
      }
      broadcastPopupApprovalState();
    }
  }

  function sdkEventFromThreadEvent(event) {
    if (!event?.threadId) return null;
    const base = { sessionId: event.threadId, seq: event.seq };
    if (event.type === "assistant_message") {
      return { ...base, type: "chat_delta", text: event.text || "" };
    }
    if (event.type === "status_changed") {
      return { ...base, type: "status_changed", status: sdkStatusFromCoreStatus(event.status) };
    }
    if (event.type === "tool_call") {
      return {
        ...base,
        type: "tool_call",
        toolRequestId: event.requestId,
        tool: event.toolName,
        args: event.args,
      };
    }
    if (event.type === "done") {
      return { ...base, type: "done" };
    }
    if (event.type === "error") {
      const source = event.source === "provider" || event.source === "core" ? event.source : undefined;
      const provider = source === "provider" && event.provider ? { provider: event.provider } : {};
      return { ...base, type: "error", ...(source ? { source } : {}), ...provider, error: normalizeError(event.error) };
    }
    if (event.type === "ended") {
      return { ...base, type: "ended" };
    }
    return null;
  }

  function sdkStatusFromCoreStatus(status) {
    if (status === "waitingToolResult") return "waiting_tool_result";
    if (status === "starting" || status === "stopping") return "running";
    return status;
  }

  function dispatchSdkThreadEvent(event) {
    const message = sdkEventFromThreadEvent(event);
    if (!message) return;
    const routes = sdkRoutesBySession.get(message.sessionId);
    if (!routes) return;
    for (const [port, channelIds] of routes) {
      for (const channelId of channelIds) {
        postSdkEvent(port, { ...message, channelId });
      }
    }
  }

  async function handleSdkMessage(port, message, options = {}) {
    const requestId = message?.requestId || "";
    const channelId = message?.channelId || "";
    const context = sdkContextsByPort.get(port) || {};
    try {
      if (!requestId) {
        throw { code: "SDK_PROTOCOL_ERROR", message: "requestId is required" };
      }
      if (!channelId) {
        throw { code: "SDK_PROTOCOL_ERROR", message: "channelId is required" };
      }

      if (message.type === "get_approval_status") {
        const origin = context.origin || null;
        const approved = origin ? await isOriginApproved(origin) : false;
        postSdkResponse(port, channelId, requestId, true, {
          installed: true,
          approved,
          origin,
        });
        return;
      }

      if (message.type === "create_session") {
        if (context.approvalRequired && !options.skipApproval) {
          const approved = await ensureApprovedOrQueue(port, message, context);
          if (!approved) return;
        }
        await withNativeOperation(async () => {
          const input = message.input || {};
          const result = await sendNativeRequest("create_thread", {
            provider: input.provider,
            model: input.model,
            skills: input.skills,
          });
          const sessionId = result?.threadId;
          if (!sessionId) {
            throw { code: "SDK_PROTOCOL_ERROR", message: "create_thread response did not include threadId." };
          }

          try {
            await sendNativeRequest("subscribe_thread", { threadId: sessionId });
          } catch (err) {
            await sendNativeRequest("end_thread", { threadId: sessionId }).catch(() => {});
            throw err;
          }

          addActiveThread(sessionId);
          addSdkSession(port, channelId, sessionId);
          sdkLifecycleBySession.set(sessionId, {
            autoEndOnDisconnect: input.autoEndOnDisconnect !== false,
          });
          postSdkResponse(port, channelId, requestId, true, { sessionId });
        });
        return;
      }

      if (message.type === "list_providers") {
        const result = await withNativeOperation(() => sendNativeRequest("list_providers"));
        postSdkResponse(port, channelId, requestId, true, result || []);
        return;
      }

      if (message.type === "get_settings") {
        const result = await withNativeOperation(() => sendNativeRequest("get_settings"));
        postSdkResponse(port, channelId, requestId, true, result || {});
        return;
      }

      if (message.type === "resume_session") {
        if (context.approvalRequired && !options.skipApproval) {
          const approved = await ensureApprovedOrQueue(port, message, context);
          if (!approved) return;
        }
        const sessionId = message.sessionId;
        if (!sessionId) {
          throw { code: "SDK_PROTOCOL_ERROR", message: "sessionId is required" };
        }
        await withNativeOperation(async () => {
          await sendNativeRequest("subscribe_thread", { threadId: sessionId });
          addActiveThread(sessionId);
          addSdkSession(port, channelId, sessionId);
          postSdkResponse(port, channelId, requestId, true, { sessionId });
        });
        return;
      }

      if (message.type === "send_text") {
        await sendNativeRequest("send_text", {
          threadId: message.sessionId,
          message: message.text || "",
        });
        postSdkResponse(port, channelId, requestId, true, {});
        return;
      }

      if (message.type === "create_asset_upload") {
        if (context.approvalRequired && !options.skipApproval) {
          const approved = await ensureApprovedOrQueue(port, message, context);
          if (!approved) return;
        }
        const result = await sendNativeRequest("create_asset_upload", {
          threadId: message.sessionId,
          filename: message.filename,
          sizeBytes: message.sizeBytes,
          mimeType: message.mimeType,
        });
        postSdkResponse(port, channelId, requestId, true, result || {});
        return;
      }

      if (message.type === "list_assets") {
        if (context.approvalRequired && !options.skipApproval) {
          const approved = await ensureApprovedOrQueue(port, message, context);
          if (!approved) return;
        }
        const result = await sendNativeRequest("list_assets", { threadId: message.sessionId });
        postSdkResponse(port, channelId, requestId, true, result || { assets: [] });
        return;
      }

      if (message.type === "prepare_session") {
        if (context.approvalRequired && !options.skipApproval) {
          const approved = await ensureApprovedOrQueue(port, message, context);
          if (!approved) return;
        }
        await sendNativeRequest("prepare_thread", {
          threadId: message.sessionId,
        });
        postSdkResponse(port, channelId, requestId, true, {});
        return;
      }

      if (message.type === "submit_tool_result") {
        await sendNativeRequest("submit_tool_result", {
          threadId: message.sessionId,
          toolRequestId: message.toolRequestId,
          result: message.result,
        });
        postSdkResponse(port, channelId, requestId, true, {});
        return;
      }

      if (message.type === "end_session") {
        await withNativeOperation(async () => {
          await sendNativeRequest("end_thread", { threadId: message.sessionId });
          forgetSdkSession(message.sessionId);
          removeActiveThread(message.sessionId);
        });
        postSdkResponse(port, channelId, requestId, true, {});
        return;
      }

      throw {
        code: "SDK_PROTOCOL_ERROR",
        message: "unknown SDK request type",
        details: { type: message.type },
      };
    } catch (err) {
      postSdkResponse(port, channelId, requestId, false, null, err);
    }
  }

  function handlePopupConnect(port) {
    if (port.name !== "popup") return;

    popupPorts.add(port);
    port.postMessage({ type: "state", state });
    postPopupApprovalState(port);
    postPopupProviderErrorState(port);

    port.onMessage.addListener(async (message) => {
      try {
        if (message?.type === "create_thread") {
          await createThread();
        } else if (message?.type === "send_text") {
          await sendText(message.message || "");
        } else if (message?.type === "end_thread") {
          await endThread();
        } else if (message?.type === "get_state") {
          port.postMessage({ type: "state", state });
        } else if (message?.type === "get_popup_approval_state") {
          await postPopupApprovalState(port);
        } else if (message?.type === "approve_origin") {
          await approvePopupOrigin(message.origin);
          await postPopupApprovalState(port);
        } else if (message?.type === "revoke_origin") {
          await revokePopupOrigin(message.origin);
          await postPopupApprovalState(port);
        } else if (message?.type === "reject_pending_approval") {
          rejectPendingApproval({
            code: "APPROVAL_REJECTED",
            message: "Pedelec approval was rejected.",
          });
        } else if (message?.type === "dismiss_provider_error") {
          await dismissProviderError();
        }
      } catch (err) {
        setState({ error: formatError(err) });
      }
    });

    port.onDisconnect.addListener(() => {
      popupPorts.delete(port);
      if (pendingApproval) {
        rejectPendingApproval({
          code: "APPROVAL_REJECTED",
          message: "Pedelec approval was not completed.",
        });
      }
    });
  }

  function handleSdkConnect(port, context = {}) {
    if (port.name !== SDK_INTERNAL_PORT_NAME) return;

    sdkPorts.add(port);
    sdkChannelsByPort.set(port, new Map());
    sdkContextsByPort.set(port, {
      origin: context.origin || null,
      approvalRequired: Boolean(context.approvalRequired),
    });

    port.onMessage.addListener((message) => {
      handleSdkMessage(port, message);
    });

    port.onDisconnect.addListener(() => {
      disconnectSdkPort(port);
    });
  }

  function handleSdkExternalConnect(port) {
    if (port.name !== SDK_EXTERNAL_PORT_NAME) return;

    const origin = originFromSender(port.sender);
    sdkPorts.add(port);
    sdkChannelsByPort.set(port, new Map());
    sdkContextsByPort.set(port, {
      origin,
      approvalRequired: true,
    });

    port.onMessage.addListener((message) => {
      handleSdkMessage(port, message);
    });

    port.onDisconnect.addListener(() => {
      disconnectSdkPort(port);
    });
  }

  function start() {
    runtimeChrome.runtime.onConnect.addListener((port) => {
      handlePopupConnect(port);
      handleSdkConnect(port);
    });
    runtimeChrome.runtime.onConnectExternal?.addListener((port) => {
      handleSdkExternalConnect(port);
    });
  }

  return {
    start,
    handleNativeMessage,
    handleNativeDisconnect,
    handlePopupConnect,
    handleSdkConnect,
    handleSdkExternalConnect,
    handleSdkMessage,
    dispatchSdkThreadEvent,
    sdkEventFromThreadEvent,
    getPendingApproval: getPendingApprovalState,
    getState: () => state,
    getSdkRouteCount: () => sdkRoutesBySession.size,
    getNativeRequestCount: () => pendingRequests.size,
    getActiveThreadCount: () => activeThreadIds.size,
    hasReconnectTimer: () => Boolean(reconnectTimer),
  };
}

if (typeof chrome !== "undefined" && chrome.runtime) {
  createBackground(chrome).start();
}

if (typeof module !== "undefined") {
  module.exports = { createBackground };
}
