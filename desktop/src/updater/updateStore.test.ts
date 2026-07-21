import { describe, expect, it, vi } from "vitest";
import { createUpdateStore, type UpdateClient, type UpdaterAdapter } from "./updateStore";

function update(version = "0.1.8", install = vi.fn().mockResolvedValue(undefined)): UpdateClient {
  return { version, downloadAndInstall: install };
}

function adapter(check: UpdaterAdapter["check"], relaunch = vi.fn().mockResolvedValue(undefined)) {
  return { check, relaunch };
}

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
});
