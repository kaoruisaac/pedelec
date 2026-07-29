import { describe, expect, it, vi } from "vitest";
import { createUpdateStore, type UpdateClient, type UpdaterAdapter } from "./updateStore";
import { foregroundCheckStorageKey, localCalendarDate } from "./foregroundUpdatePolicy";

function update(version = "0.1.8", install = vi.fn().mockResolvedValue(undefined)): UpdateClient {
  return { version, downloadAndInstall: install };
}

function adapter(check: UpdaterAdapter["check"], relaunch = vi.fn().mockResolvedValue(undefined)) {
  return { check, relaunch };
}

function memoryStorage() {
  const values = new Map<string, string>();
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => values.set(key, value),
  };
}

const today = () => new Date(2026, 6, 29, 12);

describe("update store", () => {
  it("keeps the sidebar idle when no update is returned", async () => {
    const store = createUpdateStore(adapter(vi.fn().mockResolvedValue(null)), false);
    await store.checkForUpdate();
    expect(store.state()).toMatchObject({ status: "idle", availableVersion: null });
  });

  it("exposes the discovered version", async () => {
    const store = createUpdateStore(adapter(vi.fn().mockResolvedValue(update("0.1.8"))), false);
    await store.checkForUpdate();
    expect(store.state()).toMatchObject({ status: "available", availableVersion: "0.1.8" });
  });

  it("records download bytes and percent before installing", async () => {
    const install = vi.fn().mockImplementation(async (onEvent) => {
      onEvent?.({ event: "Started", data: { contentLength: 100 } });
      onEvent?.({ event: "Progress", data: { chunkLength: 43 } });
      expect(store.state()).toMatchObject({ status: "downloading", downloadedBytes: 43, progressPercent: 43 });
      onEvent?.({ event: "Finished" });
    });
    const store = createUpdateStore(adapter(vi.fn().mockResolvedValue(update("0.1.8", install))), false);
    await store.checkForUpdate();
    await store.installUpdate();
    expect(store.state()).toMatchObject({ status: "installing", progressPercent: 100 });
  });

  it("prevents duplicate install clicks", async () => {
    let finish!: () => void;
    const install = vi.fn().mockImplementation(() => new Promise<void>((resolve) => { finish = resolve; }));
    const store = createUpdateStore(adapter(vi.fn().mockResolvedValue(update("0.1.8", install))), false);
    await store.checkForUpdate();
    const first = store.installUpdate();
    await store.installUpdate();
    expect(install).toHaveBeenCalledTimes(1);
    finish();
    await first;
  });

  it("marks install errors as failed and retries with a fresh check", async () => {
    const failed = update("0.1.8", vi.fn().mockRejectedValue(new Error("network failed")));
    const recovered = update("0.1.8");
    const check = vi.fn().mockResolvedValueOnce(failed).mockResolvedValueOnce(recovered);
    const store = createUpdateStore(adapter(check), false);
    await store.checkForUpdate();
    await store.installUpdate();
    expect(store.state().status).toBe("failed");
    await store.retryUpdate();
    expect(check).toHaveBeenCalledTimes(2);
  });

  it("silently ignores failed background checks", async () => {
    const store = createUpdateStore(adapter(vi.fn().mockRejectedValue(new Error("offline"))), false);
    await store.checkForUpdate();
    expect(store.state().status).toBe("idle");
  });

  it("always runs init checks without recording a foreground check", async () => {
    const storage = memoryStorage();
    const key = foregroundCheckStorageKey("0.1.15");
    storage.setItem(key, "2026-07-29");
    const check = vi.fn().mockResolvedValue(null);
    const store = createUpdateStore(adapter(check), false, { storage, now: today });
    await store.checkForUpdate("init");
    await store.checkForUpdate("init");
    expect(check).toHaveBeenCalledTimes(2);
    expect(storage.getItem(key)).toBe("2026-07-29");
  });

  it.each([null, update("0.1.16")])("records a successful foreground check (%s)", async (result) => {
    const storage = memoryStorage();
    const store = createUpdateStore(adapter(vi.fn().mockResolvedValue(result)), false, { storage, now: today });
    await store.checkForUpdate("foreground", "0.1.15");
    expect(storage.getItem(foregroundCheckStorageKey("0.1.15"))).toBe("2026-07-29");
  });

  it("limits foreground checks by installed version and local date", async () => {
    const storage = memoryStorage();
    const check = vi.fn().mockResolvedValue(null);
    let date = new Date(2026, 6, 29, 12);
    const store = createUpdateStore(adapter(check), false, { storage, now: () => date });
    await store.checkForUpdate("foreground", "0.1.15");
    await store.checkForUpdate("foreground", "0.1.15");
    await store.checkForUpdate("foreground", "0.1.16");
    date = new Date(2026, 6, 30, 12);
    await store.checkForUpdate("foreground", "0.1.15");
    expect(check).toHaveBeenCalledTimes(3);
  });

  it("does not consume the foreground allowance after a failed check and retries it", async () => {
    const storage = memoryStorage();
    const check = vi.fn().mockRejectedValueOnce(new Error("offline")).mockResolvedValueOnce(null);
    const store = createUpdateStore(adapter(check), false, { storage, now: today });
    await store.checkForUpdate("foreground", "0.1.15");
    expect(storage.getItem(foregroundCheckStorageKey("0.1.15"))).toBeNull();
    await store.checkForUpdate("foreground", "0.1.15");
    expect(check).toHaveBeenCalledTimes(2);
  });

  it("does not let foreground checks replace an available update", async () => {
    const storage = memoryStorage();
    const first = update("0.1.16");
    const check = vi.fn().mockResolvedValue(first);
    const store = createUpdateStore(adapter(check), false, { storage, now: today });
    await store.checkForUpdate("init");
    await store.checkForUpdate("foreground", "0.1.15");
    expect(check).toHaveBeenCalledTimes(1);
    expect(store.state()).toMatchObject({ status: "available", availableVersion: "0.1.16" });
    expect(storage.getItem(foregroundCheckStorageKey("0.1.15"))).toBeNull();
  });

  it("skips foreground checks while installation is in flight or has failed", async () => {
    let finishInstall!: () => void;
    const install = vi.fn().mockImplementation(() => new Promise<void>((resolve) => { finishInstall = resolve; }));
    const check = vi.fn().mockResolvedValue(update("0.1.16", install));
    const storage = memoryStorage();
    const store = createUpdateStore(adapter(check), false, { storage, now: today });
    await store.checkForUpdate("init");
    const installing = store.installUpdate();
    await store.checkForUpdate("foreground", "0.1.15");
    expect(check).toHaveBeenCalledTimes(1);
    finishInstall();
    await installing;

    const failedInstall = vi.fn().mockRejectedValue(new Error("install failed"));
    const failedStore = createUpdateStore(
      adapter(vi.fn().mockResolvedValue(update("0.1.16", failedInstall))),
      false,
      { storage, now: today },
    );
    await failedStore.checkForUpdate("init");
    await failedStore.installUpdate();
    await failedStore.checkForUpdate("foreground", "0.1.15");
    expect(storage.getItem(foregroundCheckStorageKey("0.1.15"))).toBeNull();
  });

  it("does not duplicate an in-flight init check or record it as foreground", async () => {
    let finish!: (result: null) => void;
    const check = vi.fn().mockImplementation(() => new Promise<null>((resolve) => { finish = resolve; }));
    const storage = memoryStorage();
    const store = createUpdateStore(adapter(check), false, { storage, now: today });
    const init = store.checkForUpdate("init");
    await store.checkForUpdate("foreground", "0.1.15");
    expect(check).toHaveBeenCalledTimes(1);
    expect(storage.getItem(foregroundCheckStorageKey("0.1.15"))).toBeNull();
    finish(null);
    await init;
  });

  it("allows manual checks after a foreground record without changing it", async () => {
    const storage = memoryStorage();
    const check = vi.fn().mockResolvedValue(null);
    const store = createUpdateStore(adapter(check), false, { storage, now: today });
    await store.checkForUpdate("foreground", "0.1.15");
    await store.checkForUpdate("manual");
    expect(check).toHaveBeenCalledTimes(2);
    expect(storage.getItem(foregroundCheckStorageKey("0.1.15"))).toBe("2026-07-29");
  });

  it("does not check or record foreground updates in development", async () => {
    const storage = memoryStorage();
    const check = vi.fn().mockResolvedValue(null);
    const store = createUpdateStore(adapter(check), true, { storage, now: today });
    await store.checkForUpdate("foreground", "0.1.15");
    expect(check).not.toHaveBeenCalled();
    expect(storage.getItem(foregroundCheckStorageKey("0.1.15"))).toBeNull();
  });

  it("formats dates with local calendar fields rather than UTC", () => {
    const localDate = {
      getFullYear: () => 2026,
      getMonth: () => 6,
      getDate: () => 29,
      toISOString: () => "2026-07-28T16:00:00.000Z",
    } as unknown as Date;
    expect(localCalendarDate(localDate)).toBe("2026-07-29");
  });
});
