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
});
