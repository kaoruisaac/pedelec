/** @vitest-environment jsdom */

import { render } from "solid-js/web";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AppShell } from "./main";
import type { EffortWizardBootstrap, EffortWizardRunState } from "./effort-wizard/types";

const mocks = vi.hoisted(() => ({
  listener: undefined as ((state: EffortWizardRunState | null) => void) | undefined,
  unlisten: vi.fn(),
  reset: vi.fn(),
  bootstrap: undefined as EffortWizardBootstrap | undefined,
  initialState: null as EffortWizardRunState | null,
}));

vi.mock("@tauri-apps/api/app", () => ({
  getVersion: vi.fn(async () => "0.2.7"),
}));

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: vi.fn(() => ({
    onFocusChanged: vi.fn(async () => () => undefined),
  })),
}));

vi.mock("./updater/updateStore", () => ({
  updateStore: {
    state: () => ({ status: "idle", availableVersion: null, progressPercent: null }),
    checkForUpdate: vi.fn(async () => undefined),
    installUpdate: vi.fn(async () => undefined),
    retryUpdate: vi.fn(async () => undefined),
  },
}));

vi.mock("./services/PopUpProvider", () => ({
  default: (props: { children: unknown }) => props.children,
}));

vi.mock("./event-monitor/EventMonitorApp", () => ({ EventMonitorApp: () => null }));
vi.mock("./home/HomePage", () => ({
  default: (props: { onOpenWizard: (origin: "home-initial" | "home-update") => void }) => (
    <button type="button" class="mock-home-open" onClick={() => props.onOpenWizard("home-initial")}>
      Open Wizard
    </button>
  ),
}));
vi.mock("./settings/SettingsPage", () => ({
  default: (props: { onOpenEffortWizard: (origin: "settings") => void }) => (
    <div class="mock-settings">
      Settings
      <button type="button" class="mock-settings-open" onClick={() => props.onOpenEffortWizard("settings")}>
        Open Wizard from Settings
      </button>
    </div>
  ),
}));
vi.mock("./effort-wizard/EffortWizardPage", () => ({
  default: (props: { wizard: { store: { runState?: { status?: string } } } }) => (
    <div class="mock-wizard">
      {props.wizard.store.runState?.status === "checking"
        ? "Running"
        : props.wizard.store.runState?.status === "review_ready"
          ? "Review"
          : "Wizard"}
    </div>
  ),
}));

vi.mock("./effort-wizard/effortWizardApi", () => ({
  getEffortWizardBootstrap: vi.fn(async () => mocks.bootstrap!),
  getEffortWizardState: vi.fn(async () => mocks.initialState),
  listenToEffortWizardState: vi.fn(async (callback: (state: EffortWizardRunState | null) => void) => {
    mocks.listener = callback;
    return mocks.unlisten;
  }),
  resetEffortWizard: mocks.reset,
  applyEffortWizardSettings: vi.fn(async () => {
    throw new Error("not used in AppShell navigation tests");
  }),
  refreshProviders: vi.fn(async () => undefined),
  startEffortWizard: vi.fn(async () => {
    throw new Error("not used in AppShell navigation tests");
  }),
}));

function bootstrapFixture(): EffortWizardBootstrap {
  return {
    providers: [
      {
        provider: "codex",
        available: true,
        version: "test",
        currentPresetRevision: 2,
        appliedPresetRevision: 1,
        hasAnyEffortSetting: true,
        presetUpdateAvailable: true,
      },
    ],
    homeReminder: { type: "preset_update", providers: ["codex"] },
  };
}

function runState(status: "checking" | "review_ready"): EffortWizardRunState {
  return {
    runId: "listener-run",
    status,
    selectedProviders: ["codex"],
    providers: [],
    review: null,
    completion: null,
    error: null,
    bootstrap: null,
  };
}

async function tick(): Promise<void> {
  await Promise.resolve();
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  await Promise.resolve();
}

describe("AppShell Wizard navigation", () => {
  let dispose: (() => void) | undefined;

  beforeEach(() => {
    document.body.innerHTML = "<div id=\"root\"></div>";
    mocks.listener = undefined;
    mocks.unlisten.mockClear();
    mocks.reset.mockClear();
    mocks.bootstrap = bootstrapFixture();
    mocks.initialState = null;
  });

  afterEach(() => {
    dispose?.();
    dispose = undefined;
    document.body.innerHTML = "";
  });

  it("uses the real listener/store path without focus-stealing", async () => {
    const container = document.getElementById("root")!;
    dispose = render(() => <AppShell />, container);
    await tick();
    expect(mocks.listener).toBeTypeOf("function");

    mocks.listener!(runState("checking"));
    await tick();
    expect(container.querySelector<HTMLButtonElement>('button[title="Home"]')?.classList.contains("is-active")).toBe(true);
    expect(container.querySelector<HTMLButtonElement>('button[title="Settings"]')?.classList.contains("is-active")).toBe(false);

    mocks.listener!(runState("review_ready"));
    await tick();
    expect(container.querySelector<HTMLButtonElement>('button[title="Home"]')?.classList.contains("is-active")).toBe(true);

    container.querySelector<HTMLButtonElement>('button[title="Settings"]')!.click();
    await tick();
    mocks.listener!(runState("checking"));
    await tick();
    expect(container.querySelector<HTMLButtonElement>('button[title="Settings"]')?.classList.contains("is-active")).toBe(true);
    expect(container.querySelector<HTMLButtonElement>('button[title="Home"]')?.classList.contains("is-active")).toBe(false);

    container.querySelector<HTMLButtonElement>(".mock-settings-open")!.click();
    await tick();
    expect(container.textContent).toContain("Running");

    container.querySelector<HTMLButtonElement>('button[title="Settings"]')!.click();
    await tick();
    mocks.listener!(runState("review_ready"));
    await tick();
    expect(container.querySelector<HTMLButtonElement>('button[title="Settings"]')?.classList.contains("is-active")).toBe(true);
    expect(mocks.unlisten).not.toHaveBeenCalled();
    expect(mocks.reset).not.toHaveBeenCalled();

    container.querySelector<HTMLButtonElement>(".mock-settings-open")!.click();
    await tick();
    expect(container.textContent).toContain("Review");
    expect(mocks.unlisten).not.toHaveBeenCalled();
    expect(mocks.reset).not.toHaveBeenCalled();
  });
});
