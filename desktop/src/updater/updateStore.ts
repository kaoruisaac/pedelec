import { check, type DownloadEvent } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { createSignal } from "solid-js";

export type UpdateStatus =
  | "idle"
  | "checking"
  | "available"
  | "downloading"
  | "installing"
  | "failed";

export interface UpdateState {
  status: UpdateStatus;
  availableVersion: string | null;
  downloadedBytes: number;
  totalBytes: number | null;
  progressPercent: number | null;
}

export interface UpdateClient {
  version: string;
  downloadAndInstall: (onEvent?: (event: DownloadEvent) => void) => Promise<void>;
}

export interface UpdaterAdapter {
  check: () => Promise<UpdateClient | null>;
  relaunch: () => Promise<void>;
}

const tauriUpdater: UpdaterAdapter = { check, relaunch };

const initialState: UpdateState = {
  status: "idle",
  availableVersion: null,
  downloadedBytes: 0,
  totalBytes: null,
  progressPercent: null,
};

/**
 * Keeps the Tauri update resource alive between the background check and the
 * user's click, so a click does not need to make a second network request.
 */
export function createUpdateStore(
  adapter: UpdaterAdapter = tauriUpdater,
  isDevelopment = import.meta.env.DEV,
) {
  const [state, setState] = createSignal<UpdateState>(initialState);
  let update: UpdateClient | null = null;
  let operationInFlight = false;

  async function checkForUpdate() {
    if (isDevelopment || operationInFlight || state().status === "checking") return;

    setState({ ...initialState, status: "checking" });
    try {
      update = await adapter.check();
      setState(
        update
          ? { ...initialState, status: "available", availableVersion: update.version }
          : initialState,
      );
    } catch (error) {
      // A check failure is intentionally silent in the UI. It must never make
      // the desktop app unusable when offline or while a release is unavailable.
      console.debug("Update check failed", error);
      update = null;
      setState(initialState);
    }
  }

  async function installUpdate() {
    if (operationInFlight || !update) return;

    operationInFlight = true;
    setState({
      ...initialState,
      status: "downloading",
      availableVersion: update.version,
    });

    try {
      await update.downloadAndInstall((event: DownloadEvent) => {
        if (event.event === "Started") {
          const totalBytes = event.data.contentLength ?? null;
          setState((current) => ({ ...current, totalBytes }));
        } else if (event.event === "Progress") {
          setState((current) => {
            const downloadedBytes = current.downloadedBytes + event.data.chunkLength;
            const progressPercent = current.totalBytes
              ? Math.min(100, Math.round((downloadedBytes / current.totalBytes) * 100))
              : null;
            return { ...current, downloadedBytes, progressPercent };
          });
        } else {
          setState((current) => ({ ...current, status: "installing", progressPercent: 100 }));
        }
      });

      // The updater installs the bundle, then Tauri's process plugin performs
      // the single relaunch recommended by the updater API.
      await adapter.relaunch();
    } catch (error) {
      console.error("Update installation failed", error);
      setState((current) => ({ ...current, status: "failed" }));
    } finally {
      operationInFlight = false;
    }
  }

  async function retryUpdate() {
    await checkForUpdate();
    if (update) await installUpdate();
  }

  return { state, checkForUpdate, installUpdate, retryUpdate };
}

export const updateStore = createUpdateStore();
