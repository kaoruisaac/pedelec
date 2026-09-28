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
  preparationHandlers: [] as Array<(snapshot: unknown) => void>,
  preparationUnlisten: vi.fn(),
  getPreparationState: vi.fn(async (): Promise<unknown> => ({ attempt: 1, status: "ready" })),
  retryPreparation: vi.fn(async (): Promise<unknown> => ({ attempt: 2, status: "checking" })),
  acknowledgePreparation: vi.fn(async () => undefined),
  listenPreparation: vi.fn(async (handler: (snapshot: unknown) => void) => {
    mocks.preparationHandlers.push(handler);
    return () => {
      mocks.preparationUnlisten();
      const index = mocks.preparationHandlers.indexOf(handler);
      if (index >= 0) mocks.preparationHandlers.splice(index, 1);
    };
  }),
  updateState: {
    status: "idle" as string,
    availableVersion: null as string | null,
    progressPercent: null as number | null,
    downloadedBytes: 0,
    totalBytes: null as number | null,
  },
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
    state: () => mocks.updateState,
    checkForUpdate: vi.fn(async () => undefined),
    installUpdate: vi.fn(async () => undefined),
    retryUpdate: vi.fn(async () => undefined),
  },
}));

vi.mock("./preparation/preparationClient", () => ({
  preparationClient: {
    getState: () => mocks.getPreparationState(),
    retry: () => mocks.retryPreparation(),
    listen: (handler: (snapshot: unknown) => void) => mocks.listenPreparation(handler),
    acknowledgeFrame: () => mocks.acknowledgePreparation(),
  },
}));

vi.mock("./services/PopUpProvider", () => ({
  default: (props: { children: unknown }) => props.children,
}));

vi.mock("./event-monitor/EventMonitorApp", () => ({
  EventMonitorApp: () => <div class="mock-event-monitor">Event Monitor</div>,
}));
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

function resetPreparationMocks(): void {
  mocks.preparationHandlers.length = 0;
  mocks.preparationUnlisten.mockClear();
  mocks.getPreparationState.mockReset();
  mocks.getPreparationState.mockResolvedValue({ attempt: 1, status: "ready" });
  mocks.retryPreparation.mockReset();
  mocks.retryPreparation.mockResolvedValue({ attempt: 2, status: "checking" });
  mocks.acknowledgePreparation.mockReset();
  mocks.acknowledgePreparation.mockResolvedValue(undefined);
  mocks.listenPreparation.mockReset();
  mocks.listenPreparation.mockImplementation(async (handler: (snapshot: unknown) => void) => {
    mocks.preparationHandlers.push(handler);
    return () => {
      mocks.preparationUnlisten();
      const index = mocks.preparationHandlers.indexOf(handler);
      if (index >= 0) mocks.preparationHandlers.splice(index, 1);
    };
  });
  mocks.updateState.status = "idle";
  mocks.updateState.availableVersion = null;
  mocks.updateState.progressPercent = null;
  mocks.updateState.downloadedBytes = 0;
  mocks.updateState.totalBytes = null;
}

function emitPreparation(snapshot: unknown): void {
  for (const handler of [...mocks.preparationHandlers]) handler(snapshot);
}

const TECHNICAL_PREPARATION_TEXT = /\bdeno\b|github|runtime|sha-?256|\bhash\b|https?:|deno\.exe|\.zip/i;

describe("AppShell Wizard navigation", () => {
  let dispose: (() => void) | undefined;

  beforeEach(() => {
    document.body.innerHTML = "<div id=\"root\"></div>";
    mocks.listener = undefined;
    mocks.unlisten.mockClear();
    mocks.reset.mockClear();
    mocks.bootstrap = bootstrapFixture();
    mocks.initialState = null;
    resetPreparationMocks();
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

describe("AppShell Event Monitor shortcut", () => {
  let dispose: (() => void) | undefined;

  beforeEach(() => {
    document.body.innerHTML = "<div id=\"root\"></div>";
    resetPreparationMocks();
  });

  afterEach(() => {
    dispose?.();
    dispose = undefined;
    document.body.innerHTML = "";
  });

  function monitorPage(container: HTMLElement): HTMLElement {
    return container.querySelector<HTMLElement>(".mock-event-monitor")!.parentElement!;
  }

  it.each([
    ["Ctrl+M", { ctrlKey: true }],
    ["Meta+M", { metaKey: true }],
  ])("opens Event Monitor with %s", async (_label, modifiers) => {
    const container = document.getElementById("root")!;
    dispose = render(() => <AppShell />, container);
    await tick();

    expect(monitorPage(container).hidden).toBe(true);
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "m", ...modifiers, bubbles: true }));
    await tick();

    expect(monitorPage(container).hidden).toBe(false);
  });

  it("does not open Event Monitor for M without Ctrl or Meta", async () => {
    const container = document.getElementById("root")!;
    dispose = render(() => <AppShell />, container);

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "m", bubbles: true }));
    await tick();

    expect(monitorPage(container).hidden).toBe(true);
  });

  it("removes the shortcut listener when AppShell is disposed", async () => {
    const container = document.getElementById("root")!;
    dispose = render(() => <AppShell />, container);
    dispose();
    dispose = undefined;

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "m", ctrlKey: true, bubbles: true }));
    await tick();

    expect(container.querySelector(".mock-event-monitor")).toBeNull();
  });
});

describe("AppShell preparation mask", () => {
  let dispose: (() => void) | undefined;

  beforeEach(() => {
    document.body.innerHTML = "<div id=\"root\"></div>";
    mocks.bootstrap = bootstrapFixture();
    mocks.initialState = null;
    resetPreparationMocks();
  });

  afterEach(() => {
    dispose?.();
    dispose = undefined;
    document.body.innerHTML = "";
  });

  function mask(container: HTMLElement): HTMLElement | null {
    return container.querySelector<HTMLElement>(".app-preparation-mask");
  }

  it("presents the normal app without a preparation mask when the runtime is already ready", async () => {
    const container = document.getElementById("root")!;
    let maskVisibleWhenPresented: boolean | null = null;
    mocks.acknowledgePreparation.mockImplementation(async () => {
      maskVisibleWhenPresented = mask(container) !== null;
    });
    dispose = render(() => <AppShell />, container);
    expect(mocks.acknowledgePreparation).not.toHaveBeenCalled();
    expect(container.querySelector(".app-shell")?.hasAttribute("inert")).toBe(true);

    await tick();

    expect(mocks.acknowledgePreparation).toHaveBeenCalledTimes(1);
    expect(maskVisibleWhenPresented).toBe(false);
    expect(mask(container)).toBeNull();
    expect(container.querySelector('button[title="Home"]')).not.toBeNull();
    expect(container.querySelector(".app-shell")?.hasAttribute("inert")).toBe(false);
  });

  it("keeps the normal app hidden until a stalled first run can show 0%", async () => {
    let resolveState!: (snapshot: unknown) => void;
    mocks.getPreparationState.mockReturnValue(new Promise<unknown>((resolve) => {
      resolveState = resolve;
    }));
    const container = document.getElementById("root")!;
    let presentedText: string | null = null;
    mocks.acknowledgePreparation.mockImplementation(async () => {
      presentedText = mask(container)?.textContent ?? null;
    });
    dispose = render(() => <AppShell />, container);
    await tick();

    expect(mocks.acknowledgePreparation).not.toHaveBeenCalled();
    expect(mask(container)?.textContent).toContain("Preparing Pedelec…");
    expect(container.querySelector(".app-shell")?.hasAttribute("inert")).toBe(true);
    container.querySelector<HTMLButtonElement>('button[title="Settings"]')!.click();
    expect(container.querySelector<HTMLButtonElement>('button[title="Home"]')?.classList.contains("is-active")).toBe(true);
    expect(TECHNICAL_PREPARATION_TEXT.test(container.textContent ?? "")).toBe(false);

    resolveState({ attempt: 1, status: "checking" });
    await tick();
    expect(mocks.acknowledgePreparation).not.toHaveBeenCalled();
    expect(container.querySelector(".app-shell")?.hasAttribute("inert")).toBe(true);
    expect(mask(container)).not.toBeNull();

    emitPreparation({
      attempt: 1,
      status: "downloading",
      downloadedBytes: 0,
      totalBytes: 100,
      progressPercent: 0,
    });
    await tick();

    expect(mocks.acknowledgePreparation).toHaveBeenCalledTimes(1);
    expect(presentedText).toContain("Preparing Pedelec…");
    expect(presentedText).toContain("0%");
    expect(mask(container)?.textContent).toContain("0%");
    expect(container.querySelector(".app-shell")?.hasAttribute("inert")).toBe(true);
    expect(container.querySelector<HTMLButtonElement>('button[title="Home"]')?.classList.contains("is-active")).toBe(true);
    expect(TECHNICAL_PREPARATION_TEXT.test(mask(container)?.textContent ?? "")).toBe(false);
  });

  it("fails closed when preparation snapshot and events are unavailable", async () => {
    mocks.listenPreparation.mockRejectedValue(new Error("preparation events unavailable"));
    mocks.getPreparationState.mockRejectedValue(new Error("preparation snapshot unavailable"));
    const container = document.getElementById("root")!;
    let presentedText: string | null = null;
    mocks.acknowledgePreparation.mockImplementation(async () => {
      presentedText = mask(container)?.textContent ?? null;
    });
    dispose = render(() => <AppShell />, container);
    await tick();

    const overlay = mask(container);
    expect(mocks.acknowledgePreparation).toHaveBeenCalledTimes(1);
    expect(presentedText).toContain("Pedelec couldn't finish preparing.");
    expect(overlay?.textContent).toContain("Pedelec couldn't finish preparing.");
    expect(overlay?.querySelector(".app-preparation-retry")).not.toBeNull();
    expect(container.querySelector(".app-shell")?.hasAttribute("inert")).toBe(true);
    expect(TECHNICAL_PREPARATION_TEXT.test(overlay?.textContent ?? "")).toBe(false);
    expect(TECHNICAL_PREPARATION_TEXT.test(presentedText ?? "")).toBe(false);

    mocks.retryPreparation.mockResolvedValue({
      attempt: 2,
      status: "downloading",
      downloadedBytes: 0,
      totalBytes: 100,
      progressPercent: 0,
    });
    overlay?.querySelector<HTMLButtonElement>(".app-preparation-retry")!.click();
    await tick();
    expect(mocks.retryPreparation).toHaveBeenCalledTimes(1);
    expect(mask(container)?.textContent).toContain("Preparing Pedelec…");
    expect(mask(container)?.textContent).toContain("0%");
    expect(container.querySelector(".app-shell")?.hasAttribute("inert")).toBe(true);
  });

  it("blocks the app with preparation copy and backend percentage while downloading", async () => {
    mocks.getPreparationState.mockResolvedValue({
      attempt: 1,
      status: "downloading",
      downloadedBytes: 42,
      totalBytes: 100,
      progressPercent: 42,
    });
    const container = document.getElementById("root")!;
    dispose = render(() => <AppShell />, container);
    await tick();

    const overlay = mask(container);
    expect(overlay?.textContent).toContain("Preparing Pedelec…");
    expect(overlay?.textContent).toContain("42%");
    expect(overlay?.getAttribute("role")).toBe("alertdialog");
    expect(overlay?.querySelector(".app-preparation-spinner")).not.toBeNull();
    expect(overlay?.querySelector(".app-preparation-percent")?.getAttribute("aria-live")).toBeNull();
    expect(overlay?.querySelector("#app-preparation-status")?.getAttribute("aria-live")).toBe("polite");
    expect(container.querySelector(".app-shell")?.hasAttribute("inert")).toBe(true);
    expect(TECHNICAL_PREPARATION_TEXT.test(container.textContent ?? "")).toBe(false);

    container.querySelector<HTMLButtonElement>('button[title="Settings"]')!.click();
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "m", ctrlKey: true, bubbles: true }));
    await tick();
    expect(container.querySelector<HTMLButtonElement>('button[title="Home"]')?.classList.contains("is-active")).toBe(true);
    expect(container.querySelector<HTMLElement>(".mock-event-monitor")!.parentElement!.hidden).toBe(true);
  });

  it("updates the visible percentage from later backend progress", async () => {
    mocks.getPreparationState.mockResolvedValue({
      attempt: 1,
      status: "downloading",
      downloadedBytes: 10,
      totalBytes: 100,
      progressPercent: 10,
    });
    const container = document.getElementById("root")!;
    dispose = render(() => <AppShell />, container);
    await tick();
    expect(mask(container)?.textContent).toContain("10%");

    emitPreparation({
      attempt: 1,
      status: "downloading",
      downloadedBytes: 64,
      totalBytes: 100,
      progressPercent: 64,
    });
    await tick();
    expect(mask(container)?.textContent).toContain("64%");
    expect(mask(container)?.textContent).not.toContain("10%");
  });

  it("stays blocked at 100% while finalizing and until the backend is ready", async () => {
    mocks.getPreparationState.mockResolvedValue({
      attempt: 1,
      status: "downloading",
      downloadedBytes: 100,
      totalBytes: 100,
      progressPercent: 100,
    });
    const container = document.getElementById("root")!;
    dispose = render(() => <AppShell />, container);
    await tick();
    expect(mask(container)?.textContent).toContain("100%");
    expect(container.querySelector(".app-shell")?.hasAttribute("inert")).toBe(true);

    emitPreparation({ attempt: 1, status: "finalizing", progressPercent: 100 });
    await tick();
    expect(mask(container)?.textContent).toContain("Preparing Pedelec…");
    expect(mask(container)?.textContent).toContain("Finishing up…");
    expect(mask(container)?.textContent).toContain("100%");
    expect(mask(container)?.querySelector(".app-preparation-spinner")).not.toBeNull();
    expect(container.querySelector(".app-shell")?.hasAttribute("inert")).toBe(true);
    expect(TECHNICAL_PREPARATION_TEXT.test(mask(container)?.textContent ?? "")).toBe(false);

    emitPreparation({ attempt: 1, status: "ready" });
    await tick();
    expect(mask(container)).toBeNull();
    expect(container.querySelector(".app-shell")?.hasAttribute("inert")).toBe(false);
    container.querySelector<HTMLButtonElement>('button[title="Settings"]')!.click();
    await tick();
    expect(container.querySelector<HTMLButtonElement>('button[title="Settings"]')?.classList.contains("is-active")).toBe(true);
  });

  it("keeps a failed preparation blocked and retries once per action", async () => {
    mocks.getPreparationState.mockResolvedValue({ attempt: 1, status: "failed" });
    let resolveRetry!: (snapshot: unknown) => void;
    mocks.retryPreparation.mockReturnValue(new Promise<unknown>((resolve) => {
      resolveRetry = resolve;
    }));
    const container = document.getElementById("root")!;
    dispose = render(() => <AppShell />, container);
    await tick();

    const overlay = mask(container);
    expect(overlay?.textContent).toContain("Pedelec couldn't finish preparing.");
    expect(overlay?.textContent).not.toContain("Preparing Pedelec…");
    expect(TECHNICAL_PREPARATION_TEXT.test(overlay?.textContent ?? "")).toBe(false);
    const retry = overlay?.querySelector<HTMLButtonElement>(".app-preparation-retry");
    expect(retry).not.toBeNull();
    retry!.click();
    retry!.click();
    expect(mocks.retryPreparation).toHaveBeenCalledTimes(1);
    expect(retry!.disabled).toBe(true);

    resolveRetry({ attempt: 2, status: "checking" });
    await tick();
    expect(mask(container)?.textContent).toContain("Preparing Pedelec…");
    expect(mask(container)?.textContent).not.toContain("Pedelec couldn't finish preparing.");
    expect(mask(container)?.querySelector(".app-preparation-percent")).toBeNull();
    expect(container.querySelector(".app-shell")?.hasAttribute("inert")).toBe(true);

    emitPreparation({
      attempt: 2,
      status: "downloading",
      downloadedBytes: 0,
      totalBytes: 100,
      progressPercent: 0,
    });
    await tick();
    expect(mask(container)?.textContent).toContain("0%");
    emitPreparation({ attempt: 1, status: "failed" });
    await tick();
    expect(mask(container)?.textContent).toContain("0%");
    expect(mask(container)?.textContent).not.toContain("Pedelec couldn't finish preparing.");
  });

  it("ignores an obsolete downloading event once the snapshot is ready", async () => {
    let resolveState!: (snapshot: unknown) => void;
    mocks.getPreparationState.mockReturnValue(new Promise<unknown>((resolve) => {
      resolveState = resolve;
    }));
    const container = document.getElementById("root")!;
    dispose = render(() => <AppShell />, container);
    await tick();
    expect(mocks.preparationHandlers).toHaveLength(1);
    emitPreparation({
      attempt: 1,
      status: "downloading",
      downloadedBytes: 20,
      totalBytes: 100,
      progressPercent: 20,
    });
    expect(mask(container)?.textContent).toContain("20%");
    resolveState({ attempt: 1, status: "ready" });
    await tick();

    expect(mask(container)).toBeNull();
    expect(container.querySelector('button[title="Home"]')).not.toBeNull();
  });

  it("removes the preparation listener when AppShell is disposed", async () => {
    const container = document.getElementById("root")!;
    dispose = render(() => <AppShell />, container);
    await tick();
    expect(mocks.preparationHandlers).toHaveLength(1);

    dispose();
    dispose = undefined;

    expect(mocks.preparationUnlisten).toHaveBeenCalledTimes(1);
    expect(mocks.preparationHandlers).toHaveLength(0);
  });

  it("still shows the app updater when preparation is ready", async () => {
    mocks.updateState.status = "available";
    mocks.updateState.availableVersion = "0.9.1";
    const container = document.getElementById("root")!;
    dispose = render(() => <AppShell />, container);
    await tick();

    expect(mask(container)).toBeNull();
    expect(container.querySelector(".app-update-button")?.textContent).toContain("Pelect needs update");
    expect(container.querySelector(".app-update-button")?.getAttribute("aria-label")).toBe("Update to v0.9.1");
  });
});
