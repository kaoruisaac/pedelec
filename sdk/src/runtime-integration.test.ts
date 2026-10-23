import { createRequire } from "node:module";
import { afterEach, expect, it, vi } from "vitest";
import { Pedelec } from "./index";

// Exercise the real SDK and Extension together. Only Chrome and Native IPC
// are simulated; Rust acceptance suites cover the actual Core/runtime boundary.
const { createBackground } = createRequire(import.meta.url)("../../extension/background.js");

function portPair() {
  const listeners: Array<Array<(message: any) => void>> = [[], []];
  const disconnected: Array<Array<() => void>> = [[], []];
  const ports = [0, 1].map((side) => ({
    name: "pedelec-sdk-external",
    sender: { url: "https://app.example.test/page", tab: { id: 1 } },
    onMessage: { addListener: (fn: (message: any) => void) => listeners[side].push(fn) },
    onDisconnect: { addListener: (fn: () => void) => disconnected[side].push(fn) },
    postMessage: (message: any) => queueMicrotask(() => listeners[1 - side].forEach((fn) => fn(structuredClone(message)))),
    disconnect: () => disconnected.flat().forEach((fn) => fn()),
  }));
  return ports;
}

function storage(data: Record<string, any> = {}) {
  return {
    get: (key: string | null, cb: (result: any) => void) => cb(key === null ? { ...data } : { [key]: data[key] }),
    set: (value: any, cb?: () => void) => { Object.assign(data, value); cb?.(); },
    remove: (key: string, cb?: () => void) => { delete data[key]; cb?.(); },
  };
}

function fixture(fullGrant: boolean, confirmMode = true, approved = true) {
  const requests: any[] = [];
  const external: Array<(port: any) => void> = [];
  let counter = 0;
  const native = () => {
    const [extension, host] = portPair();
    host.onMessage.addListener((request) => {
      requests.push(request);
      let result: any = {};
      if (request.type === "ping") result = { connected: true, version: "0.4.10", processId: 1 };
      if (request.type === "create_thread") result = {
        threadId: `thread-${++counter}`, workspaceId: "workspace",
        explicitModelConfigApplied: true,
        ...(confirmMode ? { runtimeFullAccessApplied: request.runtimeFullAccess === true } : {}),
      };
      if (request.type === "subscribe_thread") result = {
        snapshot: { threadId: request.threadId, status: "idle", latestSeq: 0 },
        workspace: { workspaceId: "workspace", path: null },
      };
      host.postMessage({ type: "response", requestId: request.requestId, ok: true, result });
    });
    return extension;
  };
  const chrome = {
    storage: { local: storage({ approvedOrigins: approved ? [{ origin: "https://app.example.test", approvedAt: 1, runtimeFullAccess: fullGrant }] : [] }), session: storage() },
    action: { openPopup: async () => {} },
    i18n: { getMessage: () => "" },
    tabs: { query: (_: any, cb?: (tabs: any[]) => void) => { cb?.([]); return Promise.resolve([]); } },
    runtime: {
      lastError: null,
      getManifest: () => ({ version: "0.4.10" }),
      onConnect: { addListener: () => {} },
      onConnectExternal: { addListener: (fn: (port: any) => void) => external.push(fn) },
      connectNative: native,
      connect: () => {
        const [page, extension] = portPair();
        external.forEach((fn) => fn(extension));
        return page;
      },
    },
  };
  const background = createBackground(chrome, { disableReconnect: true });
  background.start();
  vi.stubGlobal("chrome", chrome);
  vi.stubGlobal("window", {
    location: { origin: "https://app.example.test" }, addEventListener() {},
    document: { visibilityState: "visible", hidden: false, hasFocus: () => true, addEventListener() {} },
  });
  const popup = portPair();
  background.handlePopupConnect(Object.assign(popup[1], { name: "popup", sender: undefined }));
  return { requests, background, popup: popup[0], chrome };
}

afterEach(() => { vi.unstubAllGlobals(); });

it("A4/A5/A8: same-Origin SDK Instances retain separate modes through Extension and Native", async () => {
  const { requests } = fixture(true);
  const standard = new Pedelec();
  const privileged = new Pedelec({ runtimeFullAccess: true });
  const ordinary = await standard.createSession({ provider: "codex" });
  const sandbox = await privileged.createSession({ provider: "codex", runtimeFullAccess: false });
  const full = await privileged.createSession({ provider: "codex", runtimeFullAccess: true });
  expect(new Set([ordinary.sessionId, sandbox.sessionId, full.sessionId]).size).toBe(3);
  const creates = requests.filter((request) => request.type === "create_thread");
  expect(creates.map((request) => request.runtimeFullAccess)).toEqual([false, false, true]);
  expect(creates.map((request) => request.nativeRuntimeGrant?.runtimeFullAccessApproved)).toEqual([undefined, undefined, true]);
  expect(creates.every((request) => request.callerOrigin === "https://app.example.test")).toBe(true);
  await Promise.all([ordinary.end(), sandbox.end(), full.end()]);
});

it.each([false, true])("first Full Instance createSession opens approval; Agent-only rejects Session %s", async (runtimeFullAccess) => {
  const { requests, background, popup, chrome } = fixture(false, true, false);
  chrome.action.openPopup = vi.fn(async () => {});
  const standard = new Pedelec();
  await expect(standard.createSession({ provider: "codex", runtimeFullAccess: true })).rejects.toMatchObject({ code: "INVALID_INPUT" });
  const privileged = new Pedelec({ runtimeFullAccess: true });
  const create = privileged.createSession({ provider: "codex", runtimeFullAccess });
  const rejected = expect(create).rejects.toMatchObject({ code: "CREATE_SESSION_NOT_APPROVED" });
  await vi.waitFor(() => expect(background.getPendingApproval()).toBeTruthy());
  expect(chrome.action.openPopup).toHaveBeenCalled();
  const target = background.getPendingApproval();
  popup.postMessage({ type: "approve_origin", origin: target.origin, approvalId: target.approvalId,
    agentSessions: true, runtimeFullAccess: false });
  await rejected;
  expect(requests.some((request) => request.type === "create_thread")).toBe(false);
});

it.each([false, true])("first Full Instance createSession replays after both grants: %s", async (runtimeFullAccess) => {
  const { requests, background, popup } = fixture(false, true, false);
  const privileged = new Pedelec({ runtimeFullAccess: true });
  const create = privileged.createSession({ provider: "codex", runtimeFullAccess });
  await vi.waitFor(() => expect(background.getPendingApproval()).toBeTruthy());
  const target = background.getPendingApproval();
  popup.postMessage({ type: "approve_origin", origin: target.origin, approvalId: target.approvalId,
    agentSessions: true, runtimeFullAccess: true });
  const session = await create;
  expect(requests.find((request) => request.type === "create_thread").runtimeFullAccess).toBe(runtimeFullAccess);
  await session.end();
});

it("A7: old Native mode response fails closed across Extension and SDK", async () => {
  const { requests } = fixture(true, false);
  const privileged = new Pedelec({ runtimeFullAccess: true });
  await expect(privileged.createSession({ provider: "codex", runtimeFullAccess: true })).rejects.toMatchObject({ code: "CREATE_SESSION_NOT_APPROVED" });
  expect(requests.some((request) => request.type === "subscribe_thread")).toBe(false);
  expect(requests.some((request) => request.type === "abort_session_setup")).toBe(true);
});
