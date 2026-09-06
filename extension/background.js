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
const INITIAL_THREAD_RECOVERY_DELAY_MS = 1000;
const MAX_THREAD_RECOVERY_DELAY_MS = 30000;
const MAX_EVENTS = 80;
const SDK_INTERNAL_PORT_NAME = "pedelec-sdk-internal";
const SDK_EXTERNAL_PORT_NAME = "pedelec-sdk-external";
const APPROVED_ORIGINS_STORAGE_KEY = "approvedOrigins";
const PROVIDER_ERROR_BY_TAB_STORAGE_KEY = "providerErrorByTab";
const DEFAULT_APPROVAL_TIMEOUT_MS = 60000;
const DEFAULT_SDK_HANDSHAKE_TIMEOUT_MS = 300;
const SDK_HELLO_MESSAGE_TYPE = "sdk_hello";

function parseMajorMinor(version) {
  if (typeof version !== "string") return null;
  const trimmed = version.trim();
  if (!trimmed) return null;
  const match = /^(\d+)\.(\d+)(?:\.(\d+))?(?:[-+][0-9A-Za-z.-]*)?$/.exec(trimmed);
  if (!match) return null;
  const major = Number(match[1]);
  const minor = Number(match[2]);
  if (!Number.isInteger(major) || !Number.isInteger(minor) || major < 0 || minor < 0) return null;
  return [major, minor];
}

function isPeerVersionOutdated(extensionVersion, peerVersion) {
  const baseline = parseMajorMinor(extensionVersion);
  if (!baseline) return false;
  const peer = parseMajorMinor(peerVersion);
  if (!peer) return true;
  return peer[0] < baseline[0] || (peer[0] === baseline[0] && peer[1] < baseline[1]);
}

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
  const threadSubscriptions = new Map();
  const pendingThreadSnapshots = new Map();
  const sdkPorts = new Set();
  const sdkChannelsByPort = new Map();
  const sdkContextsByPort = new Map();
  const sdkRoutesBySession = new Map();
  const sdkLifecycleBySession = new Map();
  const approvalTimeoutMs = options.approvalTimeoutMs ?? DEFAULT_APPROVAL_TIMEOUT_MS;
  let pendingApproval = null;
  let providerErrorOperation = Promise.resolve();
  let activeSdkTabId = null;
  let providerErrorByTab = Object.create(null);
  let providerErrorCacheLoaded = false;
  const sdkHandshakeTimeoutMs = options.sdkHandshakeTimeoutMs ?? DEFAULT_SDK_HANDSHAKE_TIMEOUT_MS;
  const threadRecoveryInitialDelayMs = Math.max(
    1,
    options.threadRecoveryInitialDelayMs ?? INITIAL_THREAD_RECOVERY_DELAY_MS,
  );
  const threadRecoveryMaxDelayMs = Math.max(
    threadRecoveryInitialDelayMs,
    options.threadRecoveryMaxDelayMs ?? MAX_THREAD_RECOVERY_DELAY_MS,
  );
  const sdkWarningByTab = Object.create(null);
  let versionWarningOperation = Promise.resolve();
  let popupOpenPromise = null;
  let desktopProbePromise = null;
  let desktopVersionState = {
    status: "unknown",
    version: null,
    processId: null,
    occurrenceId: 0,
    dismissedOccurrenceId: null,
    autoPopupConsumedOccurrenceId: null,
    legacyOccurrenceBoundaryPending: false,
  };

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

  function getExtensionVersion() {
    try {
      const version = runtimeChrome.runtime?.getManifest?.()?.version;
      return typeof version === "string" ? version : "";
    } catch (_err) {
      return "";
    }
  }

  function enqueueVersionWarningOperation(operation) {
    versionWarningOperation = versionWarningOperation
      .catch(() => {})
      .then(operation)
      .catch(() => {});
    return versionWarningOperation;
  }

  function tabHasOutdatedSdk(tabId) {
    const currentTabId = normalizeTabId(tabId);
    if (currentTabId == null) return false;
    for (const context of sdkContextsByPort.values()) {
      if (context?.tabId === currentTabId && context.outdated === true) return true;
    }
    return false;
  }

  function visibleSdkVersionWarningForTab(tabId) {
    const currentTabId = normalizeTabId(tabId);
    if (currentTabId == null || !hasSdkPortForTab(currentTabId) || !tabHasOutdatedSdk(currentTabId)) {
      return { outdated: false };
    }
    if (sdkWarningByTab[tabIdKey(currentTabId)]?.dismissed === true) {
      return { outdated: false };
    }
    return { outdated: true };
  }

  function visibleDesktopVersionWarning() {
    if (desktopVersionState.status !== "outdated") return { outdated: false };
    if (desktopVersionState.dismissedOccurrenceId === desktopVersionState.occurrenceId) {
      return { outdated: false };
    }
    return { outdated: true };
  }

  function broadcastSdkVersionWarningState(warning) {
    const message = { type: "sdk_version_warning_state", warning };
    for (const port of popupPorts) {
      try {
        port.postMessage(message);
      } catch (_err) {
        popupPorts.delete(port);
      }
    }
  }

  function broadcastDesktopVersionWarningState(warning) {
    const message = { type: "desktop_version_warning_state", warning };
    for (const port of popupPorts) {
      try {
        port.postMessage(message);
      } catch (_err) {
        popupPorts.delete(port);
      }
    }
  }

  async function resolvePopupSdkVersionWarning() {
    return visibleSdkVersionWarningForTab(await getCurrentBrowserTabId());
  }

  async function broadcastResolvedPopupSdkVersionWarning() {
    if (popupPorts.size === 0) return;
    broadcastSdkVersionWarningState(await resolvePopupSdkVersionWarning());
  }

  function broadcastResolvedPopupDesktopVersionWarning() {
    if (popupPorts.size === 0) return;
    broadcastDesktopVersionWarningState(visibleDesktopVersionWarning());
  }

  async function broadcastVersionWarningStates() {
    await broadcastResolvedPopupSdkVersionWarning();
    broadcastResolvedPopupDesktopVersionWarning();
  }

  function postPopupSdkVersionWarningState(port) {
    return enqueueVersionWarningOperation(async () => {
      try {
        if (!popupPorts.has(port)) return;
        port.postMessage({
          type: "sdk_version_warning_state",
          warning: await resolvePopupSdkVersionWarning(),
        });
      } catch (_err) {
        popupPorts.delete(port);
      }
    });
  }

  function postPopupDesktopVersionWarningState(port) {
    try {
      port.postMessage({
        type: "desktop_version_warning_state",
        warning: visibleDesktopVersionWarning(),
      });
    } catch (_err) {
      popupPorts.delete(port);
    }
  }

  function syncSdkWarningForTab(tabId) {
    const currentTabId = normalizeTabId(tabId);
    if (currentTabId == null) return;
    const key = tabIdKey(currentTabId);
    if (!tabHasOutdatedSdk(currentTabId)) {
      delete sdkWarningByTab[key];
      return;
    }
    if (!sdkWarningByTab[key]) {
      sdkWarningByTab[key] = {
        dismissed: false,
        autoPopupConsumed: false,
      };
    }
  }

  function clearSdkHandshakeTimeout(context) {
    if (context?.handshakeTimer == null) return;
    clearTimeout(context.handshakeTimer);
    context.handshakeTimer = null;
  }

  function scheduleSdkHandshakeTimeout(port) {
    const context = sdkContextsByPort.get(port);
    if (!context || context.tabId == null) return;
    clearSdkHandshakeTimeout(context);
    context.handshakeTimer = setTimeout(() => {
      context.handshakeTimer = null;
      void enqueueVersionWarningOperation(async () => {
        if (!sdkContextsByPort.has(port) || context.handshakeReceived) return;
        context.sdkVersion = null;
        context.outdated = isPeerVersionOutdated(getExtensionVersion(), null);
        syncSdkWarningForTab(context.tabId);
        await broadcastVersionWarningStates();
        await maybeOpenVersionWarningPopup();
        scheduleDesktopCompatibilityProbe();
      });
    }, sdkHandshakeTimeoutMs);
    if (typeof context.handshakeTimer.unref === "function") {
      context.handshakeTimer.unref();
    }
  }

  function applySdkPortHello(port, message) {
    const context = sdkContextsByPort.get(port);
    if (!context) return;
    clearSdkHandshakeTimeout(context);
    context.handshakeReceived = true;
    context.sdkVersion = typeof message?.sdkVersion === "string" ? message.sdkVersion : null;
    context.outdated = isPeerVersionOutdated(getExtensionVersion(), context.sdkVersion);
    syncSdkWarningForTab(context.tabId);
  }

  async function handleSdkVersionHandshake(port, message) {
    applySdkPortHello(port, message);
    await broadcastVersionWarningStates();
    await maybeOpenVersionWarningPopup();
    scheduleDesktopCompatibilityProbe();
  }

  function normalizeDesktopProcessId(value) {
    if (typeof value === "number" && Number.isInteger(value) && value > 0 && Number.isSafeInteger(value)) {
      return value;
    }
    return null;
  }

  function applyDesktopPingSuccess(result) {
    if (result?.connected !== true) {
      applyDesktopPingFailure();
      return;
    }
    const version = typeof result.version === "string" ? result.version : null;
    const processId = normalizeDesktopProcessId(result.processId);
    const previousProcessId = desktopVersionState.processId;
    const pendingLegacyBoundary = desktopVersionState.legacyOccurrenceBoundaryPending === true;
    const processIdentityChanged =
      processId != null &&
      previousProcessId != null &&
      processId !== previousProcessId;
    const lostProcessIdentity = processId == null && previousProcessId != null;
    const recoveredFromLegacyBoundary = pendingLegacyBoundary && previousProcessId == null;
    if (processIdentityChanged || lostProcessIdentity || recoveredFromLegacyBoundary) {
      beginDesktopOccurrence();
    }
    const outdated = isPeerVersionOutdated(getExtensionVersion(), version);
    desktopVersionState = {
      ...desktopVersionState,
      status: outdated ? "outdated" : "compatible",
      version,
      processId,
      legacyOccurrenceBoundaryPending: false,
    };
  }

  function applyDesktopPingFailure() {
    desktopVersionState = {
      ...desktopVersionState,
      status: "unknown",
      version: null,
      legacyOccurrenceBoundaryPending:
        desktopVersionState.processId == null ? true : desktopVersionState.legacyOccurrenceBoundaryPending,
    };
  }

  function beginDesktopOccurrence() {
    desktopVersionState = {
      ...desktopVersionState,
      occurrenceId: desktopVersionState.occurrenceId + 1,
      legacyOccurrenceBoundaryPending: false,
    };
  }

  async function pingDesktop({ quiet = false } = {}) {
    try {
      const ping = await sendNativeRequest("ping", {}, {}, { quiet });
      applyDesktopPingSuccess(ping);
      return ping;
    } catch (err) {
      applyDesktopPingFailure();
      throw err;
    }
  }

  function startDesktopCompatibilityProbe() {
    if (desktopProbePromise) return desktopProbePromise;
    desktopProbePromise = withNativeOperation(() => pingDesktop({ quiet: true }))
      .catch(() => {})
      .finally(() => {
        desktopProbePromise = null;
      });
    return desktopProbePromise;
  }

  function scheduleDesktopCompatibilityProbe() {
    void startDesktopCompatibilityProbe().then(() => {
      void enqueueVersionWarningOperation(async () => {
        broadcastResolvedPopupDesktopVersionWarning();
        await maybeOpenVersionWarningPopup();
      });
    });
  }

  async function requestActionPopupOpen() {
    if (popupOpenPromise) return popupOpenPromise;
    if (!runtimeChrome.action?.openPopup) return "unavailable";
    popupOpenPromise = (async () => {
      try {
        await runtimeChrome.action.openPopup();
        return "opened";
      } catch (_err) {
        return "failed";
      } finally {
        popupOpenPromise = null;
      }
    })();
    return popupOpenPromise;
  }

  async function openActionPopup() {
    if (popupPorts.size > 0) return "already_open";
    return requestActionPopupOpen();
  }

  async function maybeOpenVersionWarningPopup() {
    const currentTabId = await getCurrentBrowserTabId();
    const sdkRecord = currentTabId != null ? sdkWarningByTab[tabIdKey(currentTabId)] : null;
    const sdkEligible = Boolean(
      currentTabId != null &&
      hasSdkPortForTab(currentTabId) &&
      tabHasOutdatedSdk(currentTabId) &&
      sdkRecord &&
      sdkRecord.dismissed !== true &&
      sdkRecord.autoPopupConsumed !== true
    );
    const desktopEligible = Boolean(
      currentTabId != null &&
      hasSdkPortForTab(currentTabId) &&
      desktopVersionState.status === "outdated" &&
      desktopVersionState.dismissedOccurrenceId !== desktopVersionState.occurrenceId &&
      desktopVersionState.autoPopupConsumedOccurrenceId !== desktopVersionState.occurrenceId
    );
    if (!sdkEligible && !desktopEligible) return false;

    const result = await openActionPopup();
    if (result === "already_open") return false;
    if (result !== "opened") return false;

    if (sdkEligible && sdkRecord) sdkRecord.autoPopupConsumed = true;
    if (desktopEligible) {
      desktopVersionState.autoPopupConsumedOccurrenceId = desktopVersionState.occurrenceId;
    }
    return true;
  }

  async function dismissSdkVersionWarning() {
    const currentTabId = await getCurrentBrowserTabId();
    if (currentTabId != null && tabHasOutdatedSdk(currentTabId)) {
      const key = tabIdKey(currentTabId);
      sdkWarningByTab[key] = {
        ...(sdkWarningByTab[key] || {}),
        dismissed: true,
        autoPopupConsumed: true,
      };
    }
    broadcastSdkVersionWarningState({ outdated: false });
  }

  function dismissDesktopVersionWarning() {
    if (desktopVersionState.status === "outdated") {
      desktopVersionState.dismissedOccurrenceId = desktopVersionState.occurrenceId;
      desktopVersionState.autoPopupConsumedOccurrenceId = desktopVersionState.occurrenceId;
    }
    broadcastDesktopVersionWarningState({ outdated: false });
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

  function normalizeTabId(tabId) {
    return typeof tabId === "number" && Number.isInteger(tabId) && tabId >= 0 ? tabId : null;
  }

  function tabIdFromSender(sender) {
    return normalizeTabId(sender?.tab?.id);
  }

  function tabIdKey(tabId) {
    return String(tabId);
  }

  function hasSdkPortForTab(tabId) {
    for (const context of sdkContextsByPort.values()) {
      if (context?.tabId === tabId) return true;
    }
    return false;
  }

  function hasActiveSdkPortForTab(tabId) {
    for (const context of sdkContextsByPort.values()) {
      if (context?.tabId === tabId && context.pageActive === true) return true;
    }
    return false;
  }

  function tabIdsForSession(sessionId) {
    const routes = sdkRoutesBySession.get(sessionId);
    if (!routes) return [];
    const tabIds = new Set();
    for (const port of routes.keys()) {
      const tabId = sdkContextsByPort.get(port)?.tabId;
      if (typeof tabId === "number") tabIds.add(tabId);
    }
    return Array.from(tabIds);
  }

  async function getCurrentBrowserTabId() {
    const tabsApi = runtimeChrome.tabs;
    if (!tabsApi?.query) return null;

    return new Promise((resolve) => {
      let settled = false;
      const finish = (tabId) => {
        if (settled) return;
        settled = true;
        resolve(normalizeTabId(tabId));
      };

      const done = (tabs) => {
        if (runtimeChrome.runtime?.lastError) {
          finish(null);
          return;
        }
        finish(Array.isArray(tabs) ? tabs[0]?.id : null);
      };

      try {
        const maybePromise = tabsApi.query({ active: true, lastFocusedWindow: true }, done);
        if (maybePromise?.then) {
          maybePromise.then(done, () => finish(null));
        }
      } catch (_err) {
        finish(null);
      }
    });
  }

  function visibleProviderErrorFromRecord(record) {
    if (!record || typeof record.provider !== "string" || typeof record.message !== "string") {
      return null;
    }
    return {
      provider: record.provider,
      message: record.message,
    };
  }

  function popupProviderErrorForTab(tabId) {
    const currentTabId = normalizeTabId(tabId);
    if (currentTabId == null || !hasSdkPortForTab(currentTabId)) return null;
    return visibleProviderErrorFromRecord(providerErrorByTab[tabIdKey(currentTabId)]);
  }

  function isSameProviderErrorOccurrence(record, event) {
    return Boolean(
      record &&
      event &&
      typeof record.threadId === "string" &&
      record.threadId === event.threadId &&
      typeof record.seq === "number" &&
      record.seq === event.seq
    );
  }

  function retainedProviderErrorRecord(event, providerError) {
    return {
      provider: providerError.provider,
      message: providerError.message,
      threadId: event.threadId,
      seq: event.seq,
      autoPopupConsumed: false,
    };
  }

  async function resolvePopupProviderError() {
    return popupProviderErrorForTab(await getCurrentBrowserTabId());
  }

  async function broadcastResolvedPopupProviderError() {
    if (popupPorts.size === 0) return;
    broadcastProviderErrorState(await resolvePopupProviderError());
  }

  async function loadProviderErrorByTab() {
    if (providerErrorCacheLoaded) return providerErrorByTab;
    providerErrorCacheLoaded = true;
    if (!hasSessionStorage()) {
      providerErrorByTab = Object.create(null);
      return providerErrorByTab;
    }
    try {
      const result = await sessionStorageGet(PROVIDER_ERROR_BY_TAB_STORAGE_KEY);
      const stored = result?.[PROVIDER_ERROR_BY_TAB_STORAGE_KEY];
      providerErrorByTab = stored && typeof stored === "object" && !Array.isArray(stored)
        ? { ...stored }
        : Object.create(null);
    } catch (_err) {
      providerErrorByTab = Object.create(null);
    }
    return providerErrorByTab;
  }

  async function persistProviderErrorByTab() {
    if (!hasSessionStorage()) return;
    await sessionStorageSet({ [PROVIDER_ERROR_BY_TAB_STORAGE_KEY]: providerErrorByTab });
  }

  function setActiveSdkTabId(nextTabId) {
    if (activeSdkTabId === nextTabId) return false;
    activeSdkTabId = nextTabId;
    return true;
  }

  function refreshPopupProviderErrorState() {
    if (!hasSessionStorage() || popupPorts.size === 0) return Promise.resolve();
    return enqueueProviderErrorOperation(async () => {
      await loadProviderErrorByTab();
      await broadcastResolvedPopupProviderError();
    });
  }

  function handleSdkPageActivity(port, message) {
    const context = sdkContextsByPort.get(port);
    if (!context || context.tabId == null) return;

    context.pageActive = message?.active === true;
    if (context.pageActive) {
      setActiveSdkTabId(context.tabId);
      enqueueVersionWarningOperation(async () => {
        await maybeOpenVersionWarningPopup();
        await broadcastVersionWarningStates();
      });
      if (!hasSessionStorage()) return;
      enqueueProviderErrorOperation(async () => {
        await loadProviderErrorByTab();
        await maybeOpenProviderErrorPopupForTab(context.tabId);
        if (popupPorts.size > 0) {
          await broadcastResolvedPopupProviderError();
        }
      });
      return;
    }

    if (activeSdkTabId === context.tabId && !hasActiveSdkPortForTab(context.tabId)) {
      if (setActiveSdkTabId(null)) refreshPopupProviderErrorState();
    }
  }

  function forgetProviderErrorForTab(tabId) {
    if (!hasSessionStorage()) return Promise.resolve();
    return enqueueProviderErrorOperation(async () => {
      await loadProviderErrorByTab();
      const key = tabIdKey(tabId);
      if (Object.prototype.hasOwnProperty.call(providerErrorByTab, key)) {
        delete providerErrorByTab[key];
        await persistProviderErrorByTab();
      }
      await broadcastResolvedPopupProviderError();
    });
  }

  function handleSdkTabPortDisconnect(tabId) {
    if (hasSdkPortForTab(tabId)) {
      syncSdkWarningForTab(tabId);
      enqueueVersionWarningOperation(async () => {
        await broadcastVersionWarningStates();
      });
      if (activeSdkTabId === tabId && !hasActiveSdkPortForTab(tabId)) {
        if (setActiveSdkTabId(null)) refreshPopupProviderErrorState();
      }
      return;
    }

    delete sdkWarningByTab[tabIdKey(tabId)];
    enqueueVersionWarningOperation(async () => {
      await broadcastVersionWarningStates();
    });
    if (activeSdkTabId === tabId) setActiveSdkTabId(null);
    forgetProviderErrorForTab(tabId);
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
        await loadProviderErrorByTab();
        if (!popupPorts.has(port)) return;
        let providerError = null;
        try {
          providerError = await resolvePopupProviderError();
        } catch (_err) {
          providerError = null;
        }
        if (!popupPorts.has(port)) return;
        port.postMessage({
          type: "provider_error_state",
          providerError,
        });
      } catch (_err) {
        // Provider errors are an optional popup side effect.
      }
    });
  }

  async function openProviderErrorPopup() {
    const result = await requestActionPopupOpen();
    return result === "opened";
  }

  async function maybeOpenProviderErrorPopupForTab(tabId) {
    const currentTabId = normalizeTabId(tabId);
    if (currentTabId == null || !hasSdkPortForTab(currentTabId)) return false;

    const key = tabIdKey(currentTabId);
    const record = providerErrorByTab[key];
    if (!record || record.autoPopupConsumed === true) return false;

    const actualTabId = await getCurrentBrowserTabId();
    if (actualTabId !== currentTabId) return false;

    const opened = await openProviderErrorPopup();
    if (!opened) return false;

    record.autoPopupConsumed = true;
    await persistProviderErrorByTab();
    return true;
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
    const tabIds = tabIdsForSession(event.threadId);
    if (tabIds.length === 0) return;

    enqueueProviderErrorOperation(async () => {
      await loadProviderErrorByTab();
      for (const tabId of tabIds) {
        const key = tabIdKey(tabId);
        const existing = providerErrorByTab[key];
        if (!isSameProviderErrorOccurrence(existing, event)) {
          providerErrorByTab[key] = retainedProviderErrorRecord(event, providerError);
        }
      }
      await persistProviderErrorByTab();
      const currentTabId = await getCurrentBrowserTabId();
      if (popupPorts.size > 0) {
        broadcastProviderErrorState(popupProviderErrorForTab(currentTabId));
      }
      if (currentTabId != null && tabIds.includes(currentTabId)) {
        await maybeOpenProviderErrorPopupForTab(currentTabId);
      }
    });
  }

  function dismissProviderError() {
    if (!hasSessionStorage()) return Promise.resolve();
    return enqueueProviderErrorOperation(async () => {
      await loadProviderErrorByTab();
      const currentTabId = await getCurrentBrowserTabId();
      if (currentTabId == null || !hasSdkPortForTab(currentTabId)) {
        broadcastProviderErrorState(null);
        return;
      }
      const key = tabIdKey(currentTabId);
      if (!Object.prototype.hasOwnProperty.call(providerErrorByTab, key)) {
        broadcastProviderErrorState(null);
        return;
      }
      delete providerErrorByTab[key];
      await persistProviderErrorByTab();
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

  function connectNative({ quiet = false } = {}) {
    if (nativePort) return true;

    try {
      nativePort = runtimeChrome.runtime.connectNative(HOST_NAME);
    } catch (err) {
      nativePort = null;
      setState({
        connected: false,
        error: quiet ? null : err.message,
      });
      if (!quiet) {
        notifyAllSdkPorts({
          type: "error",
          error: normalizeError(err, "NATIVE_HOST_UNAVAILABLE", "Pedelec native host is not connected."),
        });
        scheduleReconnect();
      }
      return false;
    }

    nativePort.onMessage.addListener(handleNativeMessage);
    nativePort.onDisconnect.addListener(handleNativeDisconnect);
    reconnectDelayMs = INITIAL_RECONNECT_DELAY_MS;
    setState({
      connected: true,
      error: null,
    });
    void restoreActiveThreadSubscriptions();
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
    for (const threadId of activeThreadIds) {
      const subscription = threadSubscriptions.get(threadId);
      if (subscription) subscription.health = "restoring";
    }
    applyDesktopPingFailure();
    enqueueVersionWarningOperation(async () => {
      await broadcastVersionWarningStates();
    });
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
      if (!updateThreadSubscriptionSeq(message.event)) return;
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

    if (message?.type === "thread_subscription_closed") {
      const threadId = message.threadId;
      const subscription = threadSubscriptions.get(threadId);
      if (!threadId || !subscription || !activeThreadIds.has(threadId)) return;
      subscription.health = "closed";
      subscription.lastError = normalizeError(
        message.error,
        "THREAD_SUBSCRIPTION_CLOSED",
        "The Core thread subscription closed.",
      );
      void recoverThreadSubscription(threadId);
      return;
    }

    const text = `Unexpected native message: ${JSON.stringify(message)}`;
    setState({ error: text });
    notifyAllSdkPorts({
      type: "error",
      error: { code: "SDK_PROTOCOL_ERROR", message: text },
    });
  }

  function sendNativeRequest(type, payload = {}, metadata = {}, { quiet = false } = {}) {
    if (!connectNative({ quiet })) {
      return Promise.reject({
        code: "NATIVE_HOST_UNAVAILABLE",
        message: "Pedelec native host is not connected.",
      });
    }

    const requestId = nextRequestId();
    const message = { ...payload, type, requestId, ...metadata };

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
        const subscriptionResult = await sendNativeRequest("subscribe_thread", { threadId });
        addActiveThread(threadId);
        applyThreadSnapshot(threadId, subscriptionResult);
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
    await ensureThreadSubscriptionHealthy(state.threadId, {});
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

  function getThreadSubscription(threadId, origin) {
    if (!threadId) return null;
    let subscription = threadSubscriptions.get(threadId);
    if (!subscription) {
      subscription = {
        threadId,
        origin: typeof origin === "string" ? origin : null,
        health: "restoring",
        lastSeq: null,
        recoveryPromise: null,
        recoveryTimer: null,
        recoveryDelayMs: threadRecoveryInitialDelayMs,
        recoveryGeneration: 0,
        lastError: null,
      };
      threadSubscriptions.set(threadId, subscription);
    } else if (typeof origin === "string" && origin && !subscription.origin) {
      subscription.origin = origin;
    }
    return subscription;
  }

  function addActiveThread(threadId, origin = null) {
    if (threadId) {
      activeThreadIds.add(threadId);
      getThreadSubscription(threadId, origin);
    }
  }

  function removeActiveThread(threadId) {
    if (threadId) {
      activeThreadIds.delete(threadId);
      const subscription = threadSubscriptions.get(threadId);
      if (subscription) {
        subscription.recoveryGeneration += 1;
        clearThreadRecoveryTimer(subscription);
      }
      threadSubscriptions.delete(threadId);
      pendingThreadSnapshots.delete(threadId);
    }
    if (activeThreadIds.size === 0) clearReconnectTimer();
  }

  function updateThreadSubscriptionSeq(event) {
    if (!event?.threadId || !activeThreadIds.has(event.threadId) || typeof event.seq !== "number") return true;
    const subscription = getThreadSubscription(event.threadId);
    if (!subscription) return true;
    if (subscription.lastSeq != null && event.seq <= subscription.lastSeq) return false;
    subscription.lastSeq = event.seq;
    return true;
  }

  function dispatchSdkSnapshot(threadId, snapshot) {
    if (!threadId || !snapshot) return;
    pendingThreadSnapshots.set(threadId, snapshot);
    const routes = sdkRoutesBySession.get(threadId);
    if (!routes) return;
    for (const [port, channelIds] of routes) {
      for (const channelId of channelIds) {
        postSdkEvent(port, {
          type: "session_snapshot",
          sessionId: threadId,
          seq: snapshot.latestSeq,
          snapshot,
          channelId,
        });
      }
    }
  }

  function applyThreadSnapshot(threadId, result) {
    const snapshot = result?.snapshot;
    if (!snapshot || snapshot.threadId !== threadId || typeof snapshot.latestSeq !== "number" || !snapshot.status) {
      throw {
        code: "SDK_PROTOCOL_ERROR",
        message: "subscribe_thread response did not include a valid lifecycle snapshot.",
      };
    }
    const subscription = getThreadSubscription(threadId);
    if (subscription && typeof snapshot.latestSeq === "number") {
      subscription.lastSeq = Math.max(subscription.lastSeq ?? 0, snapshot.latestSeq);
      subscription.health = "healthy";
      subscription.lastError = null;
      clearThreadRecoveryTimer(subscription);
      subscription.recoveryDelayMs = threadRecoveryInitialDelayMs;
    }
    dispatchSdkSnapshot(threadId, snapshot);
    if (state.threadId === threadId) {
      setState({ threadStatus: snapshot.status });
      broadcastState();
    }
    return snapshot;
  }

  function subscribeThreadForContext(threadId, context = {}, { recovery = false } = {}) {
    const subscription = getThreadSubscription(threadId, context.origin);
    if (!subscription) return Promise.reject({ code: "INVALID_INPUT", message: "threadId is required" });
    if (subscription.origin && context.origin && subscription.origin !== context.origin) {
      return Promise.reject({
        code: "THREAD_ACCESS_DENIED",
        message: "The session belongs to a different SDK origin.",
      });
    }
    if (subscription.recoveryPromise && !recovery) return subscription.recoveryPromise;
    const metadata = subscription.origin ? { callerOrigin: subscription.origin } : {};
    const promise = sendNativeRequest("subscribe_thread", { threadId }, metadata)
      .then((result) => applyThreadSnapshot(threadId, result))
      .catch((err) => {
        subscription.health = "closed";
        subscription.lastError = normalizeError(err, "THREAD_SUBSCRIPTION_UNAVAILABLE", "Thread subscription could not be restored.");
        scheduleReconnect();
        throw subscription.lastError;
      })
      .finally(() => {
        if (subscription.recoveryPromise === promise) subscription.recoveryPromise = null;
        if (
          subscription.recoveryPromise === null &&
          threadSubscriptions.get(threadId) === subscription &&
          activeThreadIds.has(threadId) &&
          subscription.health !== "healthy"
        ) {
          scheduleThreadRecovery(threadId);
        }
      });
    subscription.recoveryPromise = promise;
    return promise;
  }

  function recoverThreadSubscription(threadId) {
    if (!activeThreadIds.has(threadId)) return Promise.resolve(null);
    const subscription = threadSubscriptions.get(threadId);
    if (!subscription) return Promise.resolve(null);
    subscription.health = "restoring";
    if (subscription.recoveryPromise) return subscription.recoveryPromise;
    if (subscription.recoveryTimer) return Promise.resolve(null);
    if (!nativePort && !connectNative({ quiet: true })) {
      scheduleReconnect();
      scheduleThreadRecovery(threadId);
      return Promise.resolve(null);
    }
    if (subscription.recoveryPromise) return subscription.recoveryPromise;
    return subscribeThreadForContext(threadId, { origin: subscription.origin }, { recovery: true }).catch(() => null);
  }

  async function restoreActiveThreadSubscriptions() {
    if (!nativePort) return;
    for (const threadId of activeThreadIds) {
      const subscription = getThreadSubscription(threadId);
      if (!subscription || subscription.health === "healthy" || subscription.recoveryPromise) continue;
      void recoverThreadSubscription(threadId);
    }
  }

  function clearThreadRecoveryTimer(subscription) {
    if (!subscription?.recoveryTimer) return;
    clearTimeout(subscription.recoveryTimer);
    subscription.recoveryTimer = null;
  }

  function scheduleThreadRecovery(threadId) {
    if (!activeThreadIds.has(threadId)) return;
    const subscription = threadSubscriptions.get(threadId);
    if (!subscription || subscription.health === "healthy" || subscription.recoveryPromise || subscription.recoveryTimer) {
      return;
    }

    const generation = subscription.recoveryGeneration;
    const delayMs = subscription.recoveryDelayMs;
    subscription.recoveryTimer = setTimeout(() => {
      subscription.recoveryTimer = null;
      if (
        threadSubscriptions.get(threadId) !== subscription ||
        subscription.recoveryGeneration !== generation ||
        !activeThreadIds.has(threadId) ||
        subscription.health === "healthy"
      ) {
        return;
      }
      void recoverThreadSubscription(threadId);
    }, delayMs);
    subscription.recoveryDelayMs = Math.min(delayMs * 2, threadRecoveryMaxDelayMs);
    subscription.recoveryTimer.unref?.();
  }

  async function ensureThreadSubscriptionHealthy(threadId, context) {
    const subscription = getThreadSubscription(threadId, context?.origin);
    if (!subscription) {
      throw { code: "THREAD_SUBSCRIPTION_UNAVAILABLE", message: "Thread subscription is unavailable." };
    }
    if (subscription.health !== "healthy") {
      await recoverThreadSubscription(threadId);
    }
    if (subscription.health !== "healthy") {
      throw subscription.lastError || {
        code: "THREAD_SUBSCRIPTION_UNAVAILABLE",
        message: "Thread subscription is not healthy.",
      };
    }
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
    const snapshot = pendingThreadSnapshots.get(sessionId);
    if (snapshot && sdkRoutesBySession.has(sessionId)) {
      dispatchSdkSnapshot(sessionId, snapshot);
    }
  }

  function hasSdkSessionRoute(port, channelId, sessionId) {
    return sdkChannelsByPort.get(port)?.get(channelId)?.has(sessionId) === true;
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
    const lifecycle = sdkLifecycleBySession.get(sessionId);
    try {
      await withNativeOperation(async () => {
        await sendSdkNativeRequest({ origin: lifecycle.origin }, "end_thread", { threadId: sessionId });
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
    const context = sdkContextsByPort.get(port);
    const tabId = context?.tabId;
    clearSdkHandshakeTimeout(context);
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

    if (tabId != null) handleSdkTabPortDisconnect(tabId);
  }

  function sdkEventFromThreadEvent(event) {
    if (!event?.threadId) return null;
    const base = {
      sessionId: event.threadId,
      seq: event.seq,
      ...(event.operationId ? { operationId: event.operationId } : {}),
    };
    if (event.type === "assistant_delta") {
      return { ...base, type: "chat_delta", text: event.text || "" };
    }
    if (event.type === "assistant_message") {
      return { ...base, type: "chat_message", text: event.text || "" };
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
    if (event.type === "operation_completed") {
      return {
        ...base,
        type: "operation_completed",
        operationId: event.operationId,
        operationKind: event.operationKind,
        success: event.success,
        ...(event.error ? { error: normalizeError(event.error) } : {}),
      };
    }
    if (event.type === "usage_updated") {
      return {
        ...base,
        type: "usage_updated",
        totalTokens: event.totalTokens,
      };
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

  function sendSdkNativeRequest(context, type, payload = {}, metadata = {}) {
    if (!context?.origin || typeof context.origin !== "string") {
      return Promise.reject({ code: "SDK_ORIGIN_UNAVAILABLE", message: "The SDK caller origin is unavailable." });
    }
    return sendNativeRequest(type, payload, { callerOrigin: context.origin, ...metadata });
  }

  function projectSdkSettings(value) {
    if (!value || typeof value !== "object" || Array.isArray(value)) {
      throw { code: "SDK_PROTOCOL_ERROR", message: "get_settings response had invalid shape" };
    }
    const { defaultProvider } = value;
    if (defaultProvider !== null && typeof defaultProvider !== "string") {
      throw { code: "SDK_PROTOCOL_ERROR", message: "get_settings response had invalid shape" };
    }
    return { defaultProvider };
  }

  function projectSdkProviders(value) {
    if (!Array.isArray(value)) {
      throw { code: "SDK_PROTOCOL_ERROR", message: "list_providers response was not an array" };
    }
    return value.map((provider) => {
      if (!provider || typeof provider !== "object" || Array.isArray(provider) ||
          typeof provider.name !== "string" || typeof provider.code !== "string" ||
          typeof provider.available !== "boolean" ||
          (provider.error !== null && typeof provider.error !== "string")) {
        throw { code: "SDK_PROTOCOL_ERROR", message: "list_providers response had invalid provider data" };
      }
      return {
        name: provider.name,
        code: provider.code,
        available: provider.available,
        error: provider.error,
      };
    });
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
    if (message?.type === "page_activity") {
      handleSdkPageActivity(port, message);
      return;
    }

    if (message?.type === SDK_HELLO_MESSAGE_TYPE) {
      void enqueueVersionWarningOperation(() => handleSdkVersionHandshake(port, message));
      return;
    }

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
        let appConnected = false;
        try {
          const ping = await withNativeOperation(() => pingDesktop());
          appConnected = ping?.connected === true;
        } catch (_) {
          // Connection status is deliberately non-diagnostic for external sites.
        }
        void enqueueVersionWarningOperation(async () => {
          await broadcastVersionWarningStates();
          await maybeOpenVersionWarningPopup();
        });
        postSdkResponse(port, channelId, requestId, true, {
          installed: true,
          approved,
          origin,
          appConnected,
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
          const result = await sendSdkNativeRequest(context, "create_thread", {
            provider: input.provider,
            effortLevel: input.effortLevel,
            skills: input.skills,
            workspace: input.workspace,
          }, message.callerSdkVersion === undefined
            ? {}
            : { callerSdkVersion: message.callerSdkVersion });
          const sessionId = result?.threadId;
          if (!sessionId) {
            throw { code: "SDK_PROTOCOL_ERROR", message: "create_thread response did not include threadId." };
          }

          addActiveThread(sessionId, context.origin);
          addSdkSession(port, channelId, sessionId);
          sdkLifecycleBySession.set(sessionId, {
            autoEndOnDisconnect: input.autoEndOnDisconnect !== false,
            origin: context.origin,
          });
          try {
            await subscribeThreadForContext(sessionId, context);
          } catch (err) {
            await sendSdkNativeRequest(context, "end_thread", { threadId: sessionId }).catch(() => {});
            removeSdkSession(port, channelId, sessionId);
            removeActiveThread(sessionId);
            forgetSdkSession(sessionId);
            throw err;
          }
          postSdkResponse(port, channelId, requestId, true, { sessionId });
        });
        return;
      }

      if (message.type === "list_providers") {
        if (context.approvalRequired && !options.skipApproval) {
          const approved = await ensureApprovedOrQueue(port, message, context);
          if (!approved) return;
        }
        const result = await withNativeOperation(() => sendNativeRequest("list_providers"));
        postSdkResponse(port, channelId, requestId, true, projectSdkProviders(result));
        return;
      }

      if (message.type === "get_settings") {
        if (context.approvalRequired && !options.skipApproval) {
          const approved = await ensureApprovedOrQueue(port, message, context);
          if (!approved) return;
        }
        const result = await withNativeOperation(() => sendNativeRequest("get_settings"));
        postSdkResponse(port, channelId, requestId, true, projectSdkSettings(result));
        return;
      }

      if (message.type === "pick_workspace_folder") {
        if (context.approvalRequired && !options.skipApproval) {
          const approved = await ensureApprovedOrQueue(port, message, context);
          if (!approved) return;
        }
        const result = await withNativeOperation(() =>
          sendSdkNativeRequest(context, "pick_workspace_folder")
        );
        postSdkResponse(port, channelId, requestId, true, result);
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
          addActiveThread(sessionId, context.origin);
          addSdkSession(port, channelId, sessionId);
          try {
            await subscribeThreadForContext(sessionId, context);
          } catch (err) {
            removeSdkSession(port, channelId, sessionId);
            if (!sdkRoutesBySession.has(sessionId)) removeActiveThread(sessionId);
            throw err;
          }
          const lifecycle = sdkLifecycleBySession.get(sessionId);
          postSdkResponse(port, channelId, requestId, true, {
            sessionId,
            autoEndOnDisconnect: lifecycle?.autoEndOnDisconnect === true,
          });
        });
        return;
      }

      if (message.type === "reactivate_session") {
        if (context.approvalRequired && !options.skipApproval) {
          const approved = await ensureApprovedOrQueue(port, message, context);
          if (!approved) return;
        }
        const sessionId = message.sessionId;
        if (!sessionId) {
          throw { code: "SDK_PROTOCOL_ERROR", message: "sessionId is required" };
        }

        await withNativeOperation(async () => {
          const routeWasPresent = hasSdkSessionRoute(port, channelId, sessionId);
          const activeWasPresent = activeThreadIds.has(sessionId);
          const hadLifecycle = sdkLifecycleBySession.has(sessionId);
          const previousLifecycle = sdkLifecycleBySession.get(sessionId);
          const autoEndOnDisconnect = message.autoEndOnDisconnect !== false;

          addActiveThread(sessionId, context.origin);
          addSdkSession(port, channelId, sessionId);
          sdkLifecycleBySession.set(sessionId, {
            autoEndOnDisconnect,
            origin: context.origin,
          });

          const cleanupTemporaryState = () => {
            if (!routeWasPresent) {
              removeSdkSession(port, channelId, sessionId);
            }
            if (!activeWasPresent && !sdkRoutesBySession.has(sessionId)) {
              removeActiveThread(sessionId);
            }
            if (hadLifecycle) {
              sdkLifecycleBySession.set(sessionId, previousLifecycle);
            } else {
              sdkLifecycleBySession.delete(sessionId);
            }
          };

          try {
            // Subscribe while Core is still Ended. The initial Ended snapshot
            // is expected and closes the observation gap before reactivation.
            await subscribeThreadForContext(sessionId, context);
          } catch (err) {
            cleanupTemporaryState();
            throw err;
          }

          try {
            const result = await sendSdkNativeRequest(context, "resume_thread", {
              threadId: sessionId,
            });
            const snapshot = applyThreadSnapshot(sessionId, result);
            if (snapshot.status !== "idle") {
              throw {
                code: "SDK_PROTOCOL_ERROR",
                message: "resume_thread response did not contain an idle snapshot.",
              };
            }
            postSdkResponse(port, channelId, requestId, true, {
              sessionId,
              autoEndOnDisconnect,
            });
          } catch (err) {
            // A Core lifecycle error is deterministic and the temporary
            // observation state must be removed. Transport/protocol failures
            // stay subscribed because Core may already have committed Idle.
            const normalized = normalizeError(err);
            const deterministic = new Set([
              "THREAD_NOT_FOUND",
              "THREAD_ACCESS_DENIED",
              "THREAD_BUSY",
              "THREAD_ENDED",
              "WORKSPACE_OPEN_FAILED",
              "PROVIDER_COMMAND_FAILED",
              "INVALID_INPUT",
            ]).has(normalized.code);
            if (deterministic) cleanupTemporaryState();
            throw err;
          }
        });
        return;
      }

      if (message.type === "send_text") {
        await ensureThreadSubscriptionHealthy(message.sessionId, context);
        await sendSdkNativeRequest(context, "send_text", {
          threadId: message.sessionId,
          message: message.text || "",
          operationId: message.operationId,
        });
        postSdkResponse(port, channelId, requestId, true, {});
        return;
      }

      if (message.type === "create_asset_upload") {
        if (context.approvalRequired && !options.skipApproval) {
          const approved = await ensureApprovedOrQueue(port, message, context);
          if (!approved) return;
        }
        const result = await sendSdkNativeRequest(context, "create_asset_upload", {
          threadId: message.sessionId,
          filename: message.filename,
          sizeBytes: message.sizeBytes,
          mimeType: message.mimeType,
          ...(message.targetPath === undefined ? {} : { targetPath: message.targetPath }),
        });
        postSdkResponse(port, channelId, requestId, true, result || {});
        return;
      }

      if (message.type === "list_assets") {
        if (context.approvalRequired && !options.skipApproval) {
          const approved = await ensureApprovedOrQueue(port, message, context);
          if (!approved) return;
        }
        const result = await sendSdkNativeRequest(context, "list_assets", { threadId: message.sessionId });
        postSdkResponse(port, channelId, requestId, true, result || { assets: [] });
        return;
      }

      if (message.type === "create_asset_download") {
        if (context.approvalRequired && !options.skipApproval) {
          const approved = await ensureApprovedOrQueue(port, message, context);
          if (!approved) return;
        }
        const result = await sendSdkNativeRequest(context, "create_asset_download", {
          threadId: message.sessionId,
          path: message.path,
        });
        postSdkResponse(port, channelId, requestId, true, result || {});
        return;
      }

      if (message.type === "prepare_session") {
        if (context.approvalRequired && !options.skipApproval) {
          const approved = await ensureApprovedOrQueue(port, message, context);
          if (!approved) return;
        }
        await ensureThreadSubscriptionHealthy(message.sessionId, context);
        await sendSdkNativeRequest(context, "prepare_thread", {
          threadId: message.sessionId,
          operationId: message.operationId,
        });
        postSdkResponse(port, channelId, requestId, true, {});
        return;
      }

      if (message.type === "submit_tool_result") {
        await ensureThreadSubscriptionHealthy(message.sessionId, context);
        await sendSdkNativeRequest(context, "submit_tool_result", {
          threadId: message.sessionId,
          toolRequestId: message.toolRequestId,
          result: message.result,
        });
        postSdkResponse(port, channelId, requestId, true, {});
        return;
      }

      if (message.type === "end_session") {
        await withNativeOperation(async () => {
          await sendSdkNativeRequest(context, "end_thread", { threadId: message.sessionId });
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
    postPopupSdkVersionWarningState(port);
    postPopupDesktopVersionWarningState(port);

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
        } else if (message?.type === "dismiss_sdk_version_warning") {
          await enqueueVersionWarningOperation(() => dismissSdkVersionWarning());
        } else if (message?.type === "dismiss_desktop_version_warning") {
          dismissDesktopVersionWarning();
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
      tabId: tabIdFromSender(port.sender),
      pageActive: false,
      sdkVersion: null,
      handshakeReceived: false,
      outdated: false,
      handshakeTimer: null,
    });
    scheduleSdkHandshakeTimeout(port);

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
    getActiveSdkTabId: () => activeSdkTabId,
    hasReconnectTimer: () => Boolean(reconnectTimer),
  };
}

if (typeof chrome !== "undefined" && chrome.runtime) {
  createBackground(chrome).start();
}

if (typeof module !== "undefined") {
  module.exports = { createBackground, parseMajorMinor, isPeerVersionOutdated };
}
