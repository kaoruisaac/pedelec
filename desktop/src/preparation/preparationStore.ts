import { createSignal } from "solid-js";
import { preparationClient, type PreparationClient } from "./preparationClient";
import {
  isPreparationFrameReady,
  parsePreparationSnapshot,
  projectPreparation,
  reducePreparation,
  type AppPreparationSnapshot,
  type PreparationUiState,
} from "./preparationState";

export interface PreparationStore {
  state: () => PreparationUiState;
  initialize: () => Promise<void>;
  dispose: () => void;
  retry: () => Promise<void>;
  /** Call after the projected frame has been committed, so the native window reveals that frame. */
  presentFrame: () => void;
}

export function createPreparationStore(client: PreparationClient = preparationClient): PreparationStore {
  const [state, setState] = createSignal<PreparationUiState>(projectPreparation({
    snapshot: null,
    bridgeFailed: false,
    retryInFlight: false,
  }));
  let snapshot: AppPreparationSnapshot | null = null;
  let bridgeFailed = false;
  let retryInFlight = false;
  let unlisten: (() => void) | undefined;
  let disposed = false;
  let initialization: Promise<void> | null = null;
  let presented = false;
  let presenting = false;

  function publish(): void {
    setState(projectPreparation({ snapshot, bridgeFailed, retryInFlight }));
  }

  function apply(value: unknown): boolean {
    if (disposed) return false;
    const next = parsePreparationSnapshot(value);
    if (!next) return false;
    const reduced = reducePreparation(snapshot, next);
    if (!reduced) return false;
    snapshot = reduced;
    bridgeFailed = false;
    publish();
    return true;
  }

  async function initialize(): Promise<void> {
    if (initialization) return initialization;
    initialization = (async () => {
      let sawSnapshot = false;
      try {
        // Subscribe before reading the snapshot so a fast transition is not lost.
        const cleanup = await client.listen((payload) => {
          if (apply(payload)) sawSnapshot = true;
        });
        if (disposed) {
          cleanup();
          return;
        }
        unlisten = cleanup;
      } catch (error) {
        console.error("Could not subscribe to app preparation", error);
      }
      if (disposed) return;
      try {
        if (apply(await client.getState())) sawSnapshot = true;
      } catch (error) {
        console.error("Could not read app preparation state", error);
      }
      if (!disposed && !sawSnapshot && snapshot === null) {
        bridgeFailed = true;
        publish();
      }
    })();
    return initialization;
  }

  function dispose(): void {
    disposed = true;
    unlisten?.();
    unlisten = undefined;
  }

  function presentFrame(): void {
    if (presented || presenting || disposed) return;
    if (!isPreparationFrameReady(state())) return;
    presenting = true;
    void client.acknowledgeFrame()
      .then(() => {
        presented = true;
      })
      .catch((error) => {
        console.error("Could not present the app window", error);
      })
      .finally(() => {
        presenting = false;
      });
  }

  async function retry(): Promise<void> {
    if (disposed || retryInFlight) return;
    if (!bridgeFailed && snapshot?.status !== "failed") return;
    retryInFlight = true;
    publish();
    try {
      apply(await client.retry());
    } catch (error) {
      console.error("Could not retry app preparation", error);
    } finally {
      retryInFlight = false;
      if (!disposed) publish();
    }
  }

  return { state, initialize, dispose, retry, presentFrame };
}
