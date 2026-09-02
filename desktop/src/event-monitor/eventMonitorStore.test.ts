import { describe, expect, it } from "vitest";
import { createEventMonitorStore } from "./eventMonitorStore";

describe("event monitor store", () => {
  it("removes ended threads, adjusts event totals, and falls back selection", () => {
    const monitor = createEventMonitorStore();

    monitor.upsertThreadEvent({ type: "created", threadId: "ended-thread" });
    monitor.upsertThreadEvent({
      type: "status_changed",
      threadId: "ended-thread",
      status: "ended",
    });
    monitor.upsertThreadEvent({ type: "raw_stdout", threadId: "ended-thread", text: "done" });
    monitor.upsertThreadEvent({ type: "created", threadId: "active-thread" });
    monitor.upsertThreadEvent({
      type: "status_changed",
      threadId: "active-thread",
      status: "running",
    });
    monitor.selectThread("ended-thread");

    expect(monitor.store.totalEventCount).toBe(5);
    monitor.clearEndedThreads();

    expect(monitor.store.threadOrder).toEqual(["active-thread"]);
    expect(monitor.store.threadsById["ended-thread"]).toBeUndefined();
    expect(monitor.store.threadsById["active-thread"]?.status).toBe("running");
    expect(monitor.store.totalEventCount).toBe(2);
    expect(monitor.store.selectedThreadId).toBe("active-thread");
  });

  it("ignores residual events for dismissed ended threads", () => {
    const monitor = createEventMonitorStore();

    monitor.upsertThreadEvent({ type: "created", threadId: "ended-thread" });
    monitor.upsertThreadEvent({
      type: "status_changed",
      threadId: "ended-thread",
      status: "ended",
    });
    monitor.clearEndedThreads();
    const totalEventCountAfterClear = monitor.store.totalEventCount;

    monitor.upsertThreadEvent({
      type: "status_changed",
      threadId: "ended-thread",
      status: "ended",
    });
    monitor.upsertThreadEvent({ type: "ended", threadId: "ended-thread" });
    monitor.upsertThreadEvent({ type: "created", threadId: "ended-thread" });

    expect(monitor.store.threadsById["ended-thread"]).toBeUndefined();
    expect(monitor.store.threadOrder).toEqual([]);
    expect(monitor.store.totalEventCount).toBe(totalEventCountAfterClear);
    expect(monitor.store.selectedThreadId).toBeNull();
  });

  it("does not change selection when clearing a non-selected ended thread", () => {
    const monitor = createEventMonitorStore();

    monitor.upsertThreadEvent({ type: "created", threadId: "active-thread" });
    monitor.upsertThreadEvent({
      type: "status_changed",
      threadId: "active-thread",
      status: "running",
    });
    monitor.upsertThreadEvent({ type: "created", threadId: "ended-thread" });
    monitor.upsertThreadEvent({
      type: "status_changed",
      threadId: "ended-thread",
      status: "ended",
    });

    expect(monitor.store.selectedThreadId).toBe("active-thread");
    monitor.clearEndedThreads();
    expect(monitor.store.selectedThreadId).toBe("active-thread");
  });

  it("keeps global runtime diagnostics out of arbitrary threads and routes attached metadata", () => {
    const monitor = createEventMonitorStore();

    monitor.upsertThreadEvent({
      type: "provider_runtime_started",
      provider: "codex",
      processId: 1234,
      runtimeGeneration: 7,
    });
    monitor.upsertThreadEvent({
      type: "provider_runtime_attached",
      provider: "codex",
      threadId: "codex-thread",
      providerThreadId: "provider-thread",
      processId: 1234,
      runtimeGeneration: 7,
      resumed: true,
    });

    expect(monitor.store.runtimeStatus).toBe("attached");
    expect(monitor.store.runtimeProcessId).toBe(1234);
    expect(monitor.store.runtimeGeneration).toBe(7);
    expect(monitor.store.totalEventCount).toBe(1);
    expect(monitor.store.threadsById["codex-thread"]?.provider).toBe("codex");
    expect(monitor.store.threadsById["codex-thread"]?.providerSessionId).toBe(
      "provider-thread",
    );
    expect(monitor.store.threadsById["codex-thread"]?.events).toHaveLength(1);
  });

  it("keeps persistent runtime summaries isolated by provider", () => {
    const monitor = createEventMonitorStore();

    monitor.upsertRuntimeDiagnostic({
      type: "provider_runtime_started",
      provider: "codex",
      processId: 100,
      runtimeGeneration: 1,
    });
    monitor.upsertRuntimeDiagnostic({
      type: "provider_runtime_started",
      provider: "cursor",
      processId: 200,
      runtimeGeneration: 2,
    });
    monitor.upsertRuntimeDiagnostic({
      type: "provider_runtime_disconnected",
      provider: "codex",
      processId: 100,
      runtimeGeneration: 1,
      reason: "crashed",
    });

    expect(monitor.store.runtimeByProvider.codex).toEqual({
      status: "disconnected",
      processId: 100,
      generation: 1,
    });
    expect(monitor.store.runtimeByProvider.cursor).toEqual({
      status: "started",
      processId: 200,
      generation: 2,
    });
    expect(monitor.store.runtimeByProvider.opencode).toBeUndefined();
  });

  it("bounds RPC traffic without changing semantic event metrics or truncating messages", () => {
    const monitor = createEventMonitorStore();
    monitor.upsertThreadEvent({ type: "created", threadId: "codex-thread" });
    const largeText = "x".repeat(10_000);

    for (let index = 0; index < 350; index += 1) {
      monitor.upsertRpcTraffic({
        type: "provider_rpc_traffic",
        provider: "codex",
        threadId: "codex-thread",
        processId: 1234,
        runtimeGeneration: 7,
        ts: `2026-09-02T00:00:${String(index).padStart(3, "0")}Z`,
        direction: "provider_to_client",
        kind: "notification",
        message: { index, text: index === 349 ? largeText : "small" },
      });
      monitor.upsertRpcTraffic({
        type: "provider_rpc_traffic",
        provider: "codex",
        processId: 1234,
        runtimeGeneration: 7,
        ts: `2026-09-02T01:00:${String(index).padStart(3, "0")}Z`,
        direction: "provider_to_client",
        kind: "notification",
        message: { globalIndex: index },
      });
    }

    const thread = monitor.store.threadsById["codex-thread"]!;
    expect(thread.rpcTraffic).toHaveLength(300);
    expect(thread.rpcTraffic[0]?.message).toEqual({ index: 349, text: largeText });
    expect(monitor.store.globalRpcTraffic).toHaveLength(300);
    expect(monitor.store.globalRpcTraffic[0]?.message).toEqual({ globalIndex: 349 });
    expect(monitor.store.totalEventCount).toBe(1);
    expect(thread.eventCount).toBe(1);
    expect(thread.lastEventType).toBe("created");
  });

  it("stores persistent runtime stderr separately and routes runtime errors into Errors", () => {
    const monitor = createEventMonitorStore();
    monitor.upsertThreadEvent({
      type: "status_changed",
      threadId: "codex-thread",
      status: "running",
    });
    monitor.upsertRuntimeDiagnostic({
      type: "provider_runtime_stderr",
      provider: "codex",
      processId: 1234,
      runtimeGeneration: 7,
      text: "provider stderr\n",
    });
    monitor.upsertRuntimeDiagnostic({
      type: "provider_runtime_error",
      provider: "codex",
      threadId: "codex-thread",
      processId: 1234,
      runtimeGeneration: 7,
      code: "PROVIDER_PROTOCOL_ERROR",
      message: "bad frame",
    });

    expect(monitor.store.runtimeStderr).toHaveLength(1);
    expect(monitor.store.runtimeStderr[0]?.text).toBe("provider stderr\n");
    expect(monitor.store.threadsById["codex-thread"]?.errors).toHaveLength(1);
    expect(monitor.store.threadsById["codex-thread"]?.status).toBe("running");
  });
});
