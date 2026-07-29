export interface ForegroundCheckStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export const foregroundCheckStorageKey = (installedVersion: string) =>
  `pedelec.updater.foreground-check.${installedVersion}`;

/** Formats the user's local calendar date, deliberately avoiding UTC conversion. */
export function localCalendarDate(date: Date): string {
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return `${date.getFullYear()}-${month}-${day}`;
}

export function hasForegroundCheckToday(
  storage: ForegroundCheckStorage,
  installedVersion: string,
  now: Date,
) {
  return storage.getItem(foregroundCheckStorageKey(installedVersion)) === localCalendarDate(now);
}

export function recordForegroundCheck(
  storage: ForegroundCheckStorage,
  installedVersion: string,
  now: Date,
) {
  storage.setItem(foregroundCheckStorageKey(installedVersion), localCalendarDate(now));
}
