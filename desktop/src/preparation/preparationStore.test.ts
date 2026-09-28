import { createRoot } from "solid-js";
import { describe, expect, it, vi } from "vitest";
import type { PreparationClient } from "./preparationClient";
import { createPreparationStore, type PreparationStore } from "./preparationStore";
import { projectPreparation, type AppPreparationSnapshot } from "./preparationState";

const ready: AppPreparationSnapshot = { attempt: 1, status: "ready" };

function downloading(progressPercent: number, attempt = 1): AppPreparationSnapshot {
  return {
    attempt,
    status: "downloading",
    downloadedBytes: progressPercent,
    totalBytes: 100,
    progressPercent,
  };
}

async function withStore<T>(
  client: PreparationClient,
  run: (store: PreparationStore) => Promise<T>,
): Promise<T> {
  let disposeRoot: () => void = () => undefined;
  const store = createRoot((dispose) => {
    disposeRoot = dispose;
    return createPreparationStore(client);
  });
  try {
    return await run(store);
  } finally {
    store.dispose();
    disposeRoot();
  }
}

describe("preparation projection", () => {
  it("does not derive readiness or percentage from byte counts", () => {
    const downloadingUi = projectPreparation({
      snapshot: {
        attempt: 1,
        status: "downloading",
        downloadedBytes: 90,
        totalBytes: 100,
        progressPercent: 10,
      },
      bridgeFailed: false,
      retryInFlight: false,
    });
    expect(downloadingUi).toMatchObject({ blocking: true, status: "downloading", progressPercent: 10 });

    const completeDownload = projectPreparation({
      snapshot: downloading(100),
      bridgeFailed: false,
      retryInFlight: false,
    });
    expect(completeDownload).toMatchObject({ blocking: true, status: "downloading", progressPercent: 100 });
  });

  it("blocks until a snapshot proves the app is ready", () => {
    expect(projectPreparation({
      snapshot: null,
      bridgeFailed: false,
      retryInFlight: false,
    })).toMatchObject({ blocking: true, status: "unknown", retryDisabled: true });
    expect(projectPreparation({
      snapshot: { attempt: 1, status: "checking" },
      bridgeFailed: false,
      retryInFlight: false,
    })).toMatchObject({ blocking: true, status: "checking", progressPercent: null });
    expect(projectPreparation({
      snapshot: ready,
      bridgeFailed: false,
      retryInFlight: false,
    })).toMatchObject({ blocking: false, status: "ready" });
  });

  it("projects a bridge failure as the generic blocking failure", () => {
    expect(projectPreparation({
      snapshot: null,
      bridgeFailed: true,
      retryInFlight: false,
    })).toMatchObject({
      blocking: true,
      status: "failed",
      progressPercent: null,
      retryDisabled: false,
    });
  });
});

describe("preparation store", () => {
  it("subscribes before reading the current snapshot", async () => {
    const order: string[] = [];
    await withStore({
      listen: async () => {
        order.push("listen");
        return () => undefined;
      },
      getState: async () => {
        order.push("snapshot");
        return ready;
      },
      retry: vi.fn(),
      acknowledgeFrame: vi.fn(async () => undefined),
    }, async (store) => {
      await store.initialize();
      expect(order).toEqual(["listen", "snapshot"]);
      expect(store.state()).toMatchObject({ status: "ready", blocking: false });
    });
  });

  it("lets a newer in-flight event win over an older snapshot", async () => {
    let emit: (snapshot: unknown) => void = () => undefined;
    let resolveState: (snapshot: unknown) => void = () => undefined;
    await withStore({
      listen: async (handler) => {
        emit = handler;
        return () => undefined;
      },
      getState: () => new Promise((resolve) => {
        resolveState = resolve;
      }),
      retry: vi.fn(),
      acknowledgeFrame: vi.fn(async () => undefined),
    }, async (store) => {
      const pending = store.initialize();
      await Promise.resolve();
      emit(downloading(80));
      resolveState(downloading(30));
      await pending;
      expect(store.state()).toMatchObject({ status: "downloading", progressPercent: 80, blocking: true });
    });
  });

  it("does not stay on an obsolete download after the snapshot is ready", async () => {
    let emit: (snapshot: unknown) => void = () => undefined;
    let resolveState: (snapshot: unknown) => void = () => undefined;
    await withStore({
      listen: async (handler) => {
        emit = handler;
        return () => undefined;
      },
      getState: () => new Promise((resolve) => {
        resolveState = resolve;
      }),
      retry: vi.fn(),
      acknowledgeFrame: vi.fn(async () => undefined),
    }, async (store) => {
      const pending = store.initialize();
      await Promise.resolve();
      emit(downloading(20));
      resolveState(ready);
      await pending;
      expect(store.state()).toMatchObject({ status: "ready", blocking: false, progressPercent: null });
    });
  });

  it("ignores stale progress, older attempts, and malformed payloads", async () => {
    let emit: (snapshot: unknown) => void = () => undefined;
    await withStore({
      listen: async (handler) => {
        emit = handler;
        return () => undefined;
      },
      getState: async () => downloading(40),
      retry: vi.fn(),
      acknowledgeFrame: vi.fn(async () => undefined),
    }, async (store) => {
      await store.initialize();
      emit(downloading(10));
      emit({ attempt: 1, status: "downloading", progressPercent: 90, archiveUrl: "https://example.invalid/deno.zip" });
      emit({ attempt: 0, status: "failed" });
      expect(store.state()).toMatchObject({ status: "downloading", progressPercent: 40 });

      emit(downloading(70));
      expect(store.state().progressPercent).toBe(70);
      emit({ attempt: 1, status: "finalizing", progressPercent: 100 });
      expect(store.state()).toMatchObject({ status: "finalizing", progressPercent: 100, blocking: true });
      emit(downloading(100));
      expect(store.state().status).toBe("finalizing");
      emit(ready);
      expect(store.state()).toMatchObject({ status: "ready", blocking: false });
      emit(downloading(5));
      expect(store.state().status).toBe("ready");
    });
  });

  it("keeps the mask through retry and resets when the new attempt downloads", async () => {
    const retry = vi.fn(async (): Promise<unknown> => ({ attempt: 2, status: "checking" }));
    let emit: (snapshot: unknown) => void = () => undefined;
    await withStore({
      listen: async (handler) => {
        emit = handler;
        return () => undefined;
      },
      getState: async () => ({ attempt: 1, status: "failed" }),
      retry,
      acknowledgeFrame: vi.fn(async () => undefined),
    }, async (store) => {
      await store.initialize();
      expect(store.state()).toMatchObject({ status: "failed", blocking: true, retryDisabled: false });

      let resolveRetry: (snapshot: unknown) => void = () => undefined;
      retry.mockReturnValueOnce(new Promise<unknown>((resolve) => {
        resolveRetry = resolve;
      }));
      const first = store.retry();
      const second = store.retry();
      expect(store.state().retryDisabled).toBe(true);
      resolveRetry({ attempt: 2, status: "checking" });
      await first;
      await second;
      expect(retry).toHaveBeenCalledTimes(1);
      expect(store.state()).toMatchObject({
        status: "checking",
        blocking: true,
        progressPercent: null,
        retryDisabled: true,
      });

      emit(downloading(0, 2));
      expect(store.state()).toMatchObject({ status: "downloading", progressPercent: 0, blocking: true });
      emit({ attempt: 1, status: "failed" });
      expect(store.state().status).toBe("downloading");
    });
  });

  it("removes the listener on dispose, including when initialization is still in flight", async () => {
    const unlisten = vi.fn();
    let resolveListen: (cleanup: () => void) => void = () => undefined;
    const getState = vi.fn(async () => ready);
    await withStore({
      listen: () => new Promise((resolve) => {
        resolveListen = resolve;
      }),
      getState,
      retry: vi.fn(),
      acknowledgeFrame: vi.fn(async () => undefined),
    }, async (store) => {
      const pending = store.initialize();
      store.dispose();
      resolveListen(unlisten);
      await pending;
      expect(unlisten).toHaveBeenCalledTimes(1);
      expect(getState).not.toHaveBeenCalled();
    });
  });

  it("does not reveal the window for a stalled check, then reveals a 0% download", async () => {
    const acknowledgeFrame = vi.fn(async () => undefined);
    let emit: (snapshot: unknown) => void = () => undefined;
    await withStore({
      listen: async (handler) => {
        emit = handler;
        return () => undefined;
      },
      getState: () => new Promise(() => undefined),
      retry: vi.fn(),
      acknowledgeFrame,
    }, async (store) => {
      void store.initialize();
      await Promise.resolve();
      store.presentFrame();
      expect(store.state()).toMatchObject({ status: "unknown", blocking: true });
      expect(acknowledgeFrame).not.toHaveBeenCalled();
    });

    await withStore({
      listen: async (handler) => {
        emit = handler;
        return () => undefined;
      },
      getState: async () => ({ attempt: 1, status: "checking" }),
      retry: vi.fn(),
      acknowledgeFrame,
    }, async (store) => {
      await store.initialize();
      store.presentFrame();
      expect(acknowledgeFrame).not.toHaveBeenCalled();
      expect(store.state()).toMatchObject({ status: "checking", blocking: true });

      emit(downloading(0));
      store.presentFrame();
      await Promise.resolve();
      expect(acknowledgeFrame).toHaveBeenCalledTimes(1);
      expect(store.state()).toMatchObject({ status: "downloading", progressPercent: 0, blocking: true });
    });
  });

  it("fails closed when the preparation bridge cannot initialize and still retries", async () => {
    const retry = vi.fn(async () => downloading(0, 2));
    const acknowledgeFrame = vi.fn(async () => undefined);
    await withStore({
      listen: async () => {
        throw new Error("preparation events unavailable");
      },
      getState: async () => {
        throw new Error("preparation snapshot unavailable");
      },
      retry,
      acknowledgeFrame,
    }, async (store) => {
      await store.initialize();
      expect(store.state()).toMatchObject({
        status: "failed",
        blocking: true,
        retryDisabled: false,
        progressPercent: null,
      });
      store.presentFrame();
      expect(acknowledgeFrame).toHaveBeenCalledTimes(1);

      const first = store.retry();
      const second = store.retry();
      await first;
      await second;
      expect(retry).toHaveBeenCalledTimes(1);
      expect(store.state()).toMatchObject({ status: "downloading", progressPercent: 0, blocking: true });
    });
  });

  it("reveals an installed runtime only after the ready snapshot", async () => {
    const acknowledgeFrame = vi.fn(async () => undefined);
    await withStore({
      listen: async () => () => undefined,
      getState: async () => ready,
      retry: vi.fn(),
      acknowledgeFrame,
    }, async (store) => {
      store.presentFrame();
      expect(acknowledgeFrame).not.toHaveBeenCalled();
      await store.initialize();
      store.presentFrame();
      store.presentFrame();
      await Promise.resolve();
      expect(acknowledgeFrame).toHaveBeenCalledTimes(1);
      expect(store.state()).toMatchObject({ status: "ready", blocking: false });
    });
  });
});
