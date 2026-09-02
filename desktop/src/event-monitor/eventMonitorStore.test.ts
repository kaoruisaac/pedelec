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

  it("keeps global runtime diagnostics out of arbitrary threads and routes attached diagnostics", () => {
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
    expect(monitor.store.threadsById["codex-thread"]?.providerSessionId).toBe(
      "provider-thread",
    );
    expect(monitor.store.threadsById["codex-thread"]?.runtimeDiagnostics).toHaveLength(1);
    expect(monitor.store.runtimeDiagnostics).toHaveLength(2);
    expect(monitor.store.runtimeDiagnostics[0]?.threadId).toBe("codex-thread");
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

  it("bounds persistent runtime diagnostics", () => {
    const monitor = createEventMonitorStore();

    for (let index = 0; index < 350; index += 1) {
      monitor.upsertThreadEvent({
        type: "provider_runtime_raw_protocol",
        provider: "codex",
        operation: "notification",
        summary: `frame-${index}`,
        processId: 1234,
        runtimeGeneration: 7,
      });
    }

    expect(monitor.store.runtimeDiagnostics).toHaveLength(300);
    expect(monitor.store.totalRuntimeDiagnosticCount).toBe(350);
    expect(monitor.store.runtimeDiagnostics[0]?.summary).toBe("frame-349");
  });
});
