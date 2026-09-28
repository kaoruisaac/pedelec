export const APP_PREPARATION_EVENT = "app-preparation-state";

export type AppPreparationSnapshot =
  | { attempt: number; status: "checking" }
  | {
      attempt: number;
      status: "downloading";
      downloadedBytes: number;
      totalBytes: number;
      progressPercent: number;
    }
  | { attempt: number; status: "finalizing"; progressPercent: number }
  | { attempt: number; status: "ready" }
  | { attempt: number; status: "failed" };

export type PreparationStatus = AppPreparationSnapshot["status"] | "unknown";

export interface PreparationUiState {
  blocking: boolean;
  status: PreparationStatus;
  progressPercent: number | null;
  retryDisabled: boolean;
}

const INITIAL_UI_STATE: PreparationUiState = {
  blocking: true,
  status: "unknown",
  progressPercent: null,
  retryDisabled: true,
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function readAttempt(value: unknown): number | null {
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0) return null;
  return value;
}

function readByteCount(value: unknown): number | null {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) return null;
  return value;
}

function readPercent(value: unknown): number | null {
  if (typeof value !== "number" || !Number.isFinite(value)) return null;
  return Math.min(100, Math.max(0, Math.round(value)));
}

export function parsePreparationSnapshot(value: unknown): AppPreparationSnapshot | null {
  if (!isRecord(value)) return null;
  const attempt = readAttempt(value.attempt);
  if (attempt === null || typeof value.status !== "string") return null;

  switch (value.status) {
    case "checking":
    case "ready":
    case "failed":
      return { attempt, status: value.status };
    case "downloading": {
      const downloadedBytes = readByteCount(value.downloadedBytes);
      const totalBytes = readByteCount(value.totalBytes);
      const progressPercent = readPercent(value.progressPercent);
      if (downloadedBytes === null || totalBytes === null || progressPercent === null) return null;
      return { attempt, status: "downloading", downloadedBytes, totalBytes, progressPercent };
    }
    case "finalizing": {
      const progressPercent = readPercent(value.progressPercent);
      if (progressPercent === null) return null;
      return { attempt, status: "finalizing", progressPercent };
    }
    default:
      return null;
  }
}

/**
 * Backend attempt ids and monotonic progress are the only ordering signal.
 * A newer snapshot must not be replaced by an older event from a previous attempt.
 */
export function shouldApplyPreparationSnapshot(
  current: AppPreparationSnapshot | null,
  next: AppPreparationSnapshot,
): boolean {
  if (!current) return true;
  if (next.attempt > current.attempt) return true;
  if (next.attempt < current.attempt) return false;
  if (current.status === "ready" || next.status === "checking") return false;
  if (next.status === "ready") return true;
  if (current.status === "failed") return false;
  if (next.status === "failed") return true;
  if (next.status === "finalizing") {
    return current.status !== "finalizing" || next.progressPercent >= current.progressPercent;
  }
  if (current.status === "finalizing" || next.status !== "downloading") return false;
  if (current.status === "checking") return true;
  return next.progressPercent >= current.progressPercent;
}

export function reducePreparation(
  current: AppPreparationSnapshot | null,
  next: AppPreparationSnapshot,
): AppPreparationSnapshot | null {
  if (!shouldApplyPreparationSnapshot(current, next)) return null;
  return next;
}

export function projectPreparation(input: {
  snapshot: AppPreparationSnapshot | null;
  bridgeFailed: boolean;
  retryInFlight: boolean;
}): PreparationUiState {
  const { snapshot, bridgeFailed, retryInFlight } = input;
  if (!snapshot) {
    if (bridgeFailed) {
      return {
        blocking: true,
        status: "failed",
        progressPercent: null,
        retryDisabled: retryInFlight,
      };
    }
    return INITIAL_UI_STATE;
  }

  const blocking = snapshot.status !== "ready";
  const progressPercent = snapshot.status === "downloading" || snapshot.status === "finalizing"
    ? snapshot.progressPercent
    : null;

  return {
    blocking,
    status: snapshot.status,
    progressPercent,
    retryDisabled: retryInFlight || snapshot.status !== "failed",
  };
}

/** Ready, in-progress download/finalize, or failure. A bare check is not enough to reveal the window. */
export function isPreparationFrameReady(state: PreparationUiState): boolean {
  return state.status !== "unknown" && state.status !== "checking";
}
