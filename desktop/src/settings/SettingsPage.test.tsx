/** @vitest-environment jsdom */

import { render } from "solid-js/web";
import { createStore } from "solid-js/store";
import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import SettingsPage from "./SettingsPage";
import { Provider, ProviderCode, Settings } from "./types";
import type { EffortWizardStore } from "../effort-wizard/effortWizardStore";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

const popMock = vi.hoisted(() => vi.fn());

vi.mock("../services/PopUpProvider", () => ({
  usePopUp: () => ({ pop: popMock }),
}));

vi.mock("./EditingProviderPopup", () => ({ default: () => null }));
vi.mock("./MissingProviderPopup", () => ({ default: () => null }));
vi.mock("./providerInstaller", () => ({
  isProviderInstallerSupported: () => false,
  openProviderInstaller: async () => undefined,
  restartPedelec: async () => undefined,
}));

const invokeMock = vi.mocked(invoke);

function settings(defaultProvider: ProviderCode | null): Settings {
  const effortsArgs = () => ({ default: [], low: [], high: [] });
  return {
    defaultProvider,
    providerSettings: {
      codex: { effortsArgs: effortsArgs() },
      antigravity: { effortsArgs: effortsArgs() },
      opencode: { effortsArgs: effortsArgs() },
      cursor: { effortsArgs: effortsArgs() },
      claude: { effortsArgs: effortsArgs() },
      ollama: {
        baseUrl: "http://127.0.0.1:11434",
        timeoutMs: 120_000,
        apiKey: "",
        tavilyApiKey: "",
        effortsArgs: effortsArgs(),
      },
    },
  };
}

function provider(overrides: Partial<Provider> = {}): Provider {
  return {
    code: "codex",
    name: "Codex",
    scanned: true,
    available: true,
    error: null,
    ...overrides,
  };
}

function installInvokeMock(
  initialSettings: Settings,
  initialProviders: Provider[],
  refreshedProviders = initialProviders,
): void {
  invokeMock.mockImplementation(async (command: string, args?: unknown) => {
    switch (command) {
      case "get_settings":
        return initialSettings;
      case "list_providers":
        return initialProviders;
      case "refresh_providers":
        return refreshedProviders;
      case "check_ollama_connection":
        return { connected: false };
      case "update_settings":
        return settings((args as { input?: Settings } | undefined)?.input?.defaultProvider ?? "codex");
      default:
        throw new Error(`unexpected invoke: ${command}`);
    }
  });
}

async function tick(): Promise<void> {
  await Promise.resolve();
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
  await Promise.resolve();
}

async function waitFor(condition: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 50; attempt += 1) {
    if (condition()) return;
    await tick();
  }
  throw new Error("condition was not met before the test timeout");
}

describe("SettingsPage provider readiness integration", () => {
  let dispose: (() => void) | undefined;

  beforeEach(() => {
    invokeMock.mockReset();
    popMock.mockReset();
    document.body.innerHTML = "";
  });

  afterEach(() => {
    dispose?.();
    dispose = undefined;
    document.body.innerHTML = "";
  });

  it("consumes the ready provider snapshot on mount without a second refresh", async () => {
    installInvokeMock(settings("codex"), [provider()]);
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <SettingsPage onNavigateToSettings={() => undefined} />, container);

    await waitFor(() => container.querySelectorAll(".provider-option").length === 1);

    const commands = invokeMock.mock.calls.map(([command]) => command);
    expect(commands).toContain("get_settings");
    expect(commands).toContain("list_providers");
    expect(commands.filter((command) => command === "refresh_providers")).toHaveLength(0);
  });

  it("initializes a missing default provider from the ready list snapshot", async () => {
    installInvokeMock(settings(null), [provider()]);
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <SettingsPage onNavigateToSettings={() => undefined} />, container);

    await waitFor(() => invokeMock.mock.calls.some(([command]) => command === "update_settings"));

    const updateCall = invokeMock.mock.calls.find(([command]) => command === "update_settings");
    expect((updateCall?.[1] as { input: Settings }).input.defaultProvider).toBe("codex");
    expect(invokeMock.mock.calls.filter(([command]) => command === "refresh_providers")).toHaveLength(0);
  });

  it("only rescans providers when the user clicks Refresh", async () => {
    installInvokeMock(
      settings("codex"),
      [provider()],
      [provider({ version: "2.0.0" })],
    );
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <SettingsPage onNavigateToSettings={() => undefined} />, container);

    await waitFor(() => {
      const button = container.querySelector<HTMLButtonElement>(".settings-secondary-button");
      return button !== null && !button.disabled;
    });
    expect(invokeMock.mock.calls.filter(([command]) => command === "refresh_providers")).toHaveLength(0);

    container.querySelector<HTMLButtonElement>(".settings-secondary-button")!.click();
    await waitFor(() => invokeMock.mock.calls.filter(([command]) => command === "refresh_providers").length === 1);
    await waitFor(() => container.textContent?.includes("2.0.0") ?? false);
  });

  it("refreshes the Wizard bootstrap once after a successful provider Refresh", async () => {
    installInvokeMock(settings("codex"), [provider()], [provider({ version: "2.0.0" })]);
    const refreshBootstrap = vi.fn(async () => true);
    const effortWizard = {
      store: { settingsRefreshRevision: 0 },
      refreshBootstrap,
    } as unknown as EffortWizardStore;
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <SettingsPage onNavigateToSettings={() => undefined} effortWizard={effortWizard} onProvidersRefreshed={refreshBootstrap} />, container);

    await waitFor(() => container.querySelector<HTMLButtonElement>(".settings-secondary-button")?.disabled === false);
    container.querySelector<HTMLButtonElement>(".settings-secondary-button")!.click();
    await waitFor(() => invokeMock.mock.calls.filter(([command]) => command === "refresh_providers").length === 1);
    await waitFor(() => refreshBootstrap.mock.calls.length === 1);
    expect(refreshBootstrap).toHaveBeenCalledTimes(1);
  });

  it("reloads mounted Settings when the Wizard success refresh signal changes", async () => {
    let returnedSettings = settings("codex");
    const [wizardState, setWizardState] = createStore({ settingsRefreshRevision: 0 });
    invokeMock.mockImplementation(async (command: string) => {
      if (command === "get_settings") return returnedSettings;
      if (command === "list_providers") return [provider(), provider({ code: "claude", name: "Claude Code" })];
      if (command === "check_ollama_connection") return { connected: false };
      throw new Error(`unexpected invoke: ${command}`);
    });
    const effortWizard = { store: wizardState } as unknown as EffortWizardStore;
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <SettingsPage onNavigateToSettings={() => undefined} effortWizard={effortWizard} />, container);

    await waitFor(() => container.querySelectorAll<HTMLInputElement>('input[name="defaultProvider"]').length === 2);
    expect(container.querySelector<HTMLInputElement>('input[value="codex"]')?.checked).toBe(true);
    returnedSettings = settings("claude");
    setWizardState("settingsRefreshRevision", 1);

    await waitFor(() => container.querySelector<HTMLInputElement>('input[value="claude"]')?.checked === true);
    expect(invokeMock.mock.calls.filter(([command]) => command === "get_settings")).toHaveLength(2);
    expect(container.textContent).not.toContain("unsaved changes");
  });

  it("keeps rendering while list_providers is deferred, then renders its result", async () => {
    let resolveProviders!: (providers: Provider[]) => void;
    const deferredProviders = new Promise<Provider[]>((resolve) => {
      resolveProviders = resolve;
    });
    invokeMock.mockImplementation(async (command: string) => {
      if (command === "get_settings") return settings("codex");
      if (command === "list_providers") return deferredProviders;
      if (command === "check_ollama_connection") return { connected: false };
      throw new Error(`unexpected invoke: ${command}`);
    });

    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <SettingsPage onNavigateToSettings={() => undefined} />, container);

    await waitFor(() => container.querySelector("main.settings-page") !== null);
    expect(container.textContent).toContain("Loading...");
    expect(invokeMock.mock.calls.filter(([command]) => command === "refresh_providers")).toHaveLength(0);

    resolveProviders([provider({ name: "Codex ready" })]);
    await waitFor(() => container.textContent?.includes("Codex ready") ?? false);
  });

  it("renders the header and body Wizard entries when the callback is provided", async () => {
    installInvokeMock(settings("codex"), [provider()]);
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <SettingsPage onNavigateToSettings={() => undefined} onOpenEffortWizard={() => undefined} />, container);

    await waitFor(() => container.querySelector(".settings-effort-wizard-button") !== null);
    expect(container.querySelector(".settings-effort-wizard-button")).not.toBeNull();
    expect(container.querySelector(".settings-effort-wizard-card")).not.toBeNull();
  });

  it("lets unsaved Settings draft safety take precedence over resumable Wizard runs", async () => {
    installInvokeMock(settings("codex"), [provider(), provider({ code: "claude", name: "Claude Code" })]);
    const onOpenEffortWizard = vi.fn();
    const effortWizard = {
      store: { settingsRefreshRevision: 0 },
      hasResumableRun: vi.fn(() => true),
    } as unknown as EffortWizardStore;
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <SettingsPage onNavigateToSettings={() => undefined} effortWizard={effortWizard} onOpenEffortWizard={onOpenEffortWizard} />, container);

    await waitFor(() => container.querySelectorAll<HTMLInputElement>('input[name="defaultProvider"]').length === 2);
    container.querySelectorAll<HTMLInputElement>('input[name="defaultProvider"]')[1].click();
    await waitFor(() => container.textContent?.includes("unsaved changes") ?? false);
    container.querySelector<HTMLButtonElement>(".settings-effort-wizard-button")!.click();

    expect(popMock).toHaveBeenCalledTimes(1);
    expect(onOpenEffortWizard).not.toHaveBeenCalled();
    const popupProps = popMock.mock.calls[0][1] as { onDiscardAndContinue: () => void };
    popupProps.onDiscardAndContinue();

    expect(effortWizard.hasResumableRun).toHaveBeenCalled();
    expect(onOpenEffortWizard).toHaveBeenCalledWith("settings");
    expect(container.querySelectorAll<HTMLInputElement>('input[name="defaultProvider"]')[0].checked).toBe(true);
    expect(container.querySelectorAll<HTMLInputElement>('input[name="defaultProvider"]')[1].checked).toBe(false);
  });

  it("resumes an active run directly when there is no unsaved draft", async () => {
    installInvokeMock(settings("codex"), [provider()]);
    const onOpenEffortWizard = vi.fn();
    const effortWizard = {
      store: { settingsRefreshRevision: 0 },
      hasResumableRun: vi.fn(() => true),
    } as unknown as EffortWizardStore;
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <SettingsPage onNavigateToSettings={() => undefined} effortWizard={effortWizard} onOpenEffortWizard={onOpenEffortWizard} />, container);

    await waitFor(() => container.querySelector(".settings-effort-wizard-button") !== null && !container.querySelector<HTMLButtonElement>(".settings-effort-wizard-button")!.disabled);
    container.querySelector<HTMLButtonElement>(".settings-effort-wizard-button")!.click();

    expect(popMock).not.toHaveBeenCalled();
    expect(onOpenEffortWizard).toHaveBeenCalledWith("settings");
  });

  it("does not warn for OpenCode/Ollama-only effort settings on a new run", async () => {
    const initial = settings("codex");
    initial.providerSettings.opencode.effortsArgs.low = ["--model", "opencode-model"];
    initial.providerSettings.ollama.effortsArgs.high = ["--model", "ollama-model"];
    installInvokeMock(initial, [provider()]);
    const onOpenEffortWizard = vi.fn();
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <SettingsPage onNavigateToSettings={() => undefined} onOpenEffortWizard={onOpenEffortWizard} />, container);

    await waitFor(() => container.querySelector(".settings-effort-wizard-button") !== null && !container.querySelector<HTMLButtonElement>(".settings-effort-wizard-button")!.disabled);
    container.querySelector<HTMLButtonElement>(".settings-effort-wizard-button")!.click();

    expect(popMock).not.toHaveBeenCalled();
    expect(onOpenEffortWizard).toHaveBeenCalledWith("settings");
  });

  it("warns before starting a new run when a Wizard provider has saved effort settings", async () => {
    const initial = settings("codex");
    initial.providerSettings.codex.effortsArgs.low = ["--model", "codex-model"];
    installInvokeMock(initial, [provider()]);
    const onOpenEffortWizard = vi.fn();
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <SettingsPage onNavigateToSettings={() => undefined} onOpenEffortWizard={onOpenEffortWizard} />, container);

    await waitFor(() => container.querySelector(".settings-effort-wizard-button") !== null && !container.querySelector<HTMLButtonElement>(".settings-effort-wizard-button")!.disabled);
    container.querySelector<HTMLButtonElement>(".settings-effort-wizard-button")!.click();

    expect(popMock).toHaveBeenCalledTimes(1);
    const popupProps = popMock.mock.calls[0][1] as { onContinue: () => void };
    popupProps.onContinue();
    expect(onOpenEffortWizard).toHaveBeenCalledWith("settings");
  });

  it.each(["codex", "claude", "cursor", "antigravity"] as const)(
    "warns for saved %s effort settings",
    async (wizardProvider) => {
      const initial = settings("codex");
      initial.providerSettings[wizardProvider].effortsArgs.low = ["--model", `${wizardProvider}-model`];
      initial.providerSettings.opencode.effortsArgs.default = [];
      initial.providerSettings.ollama.effortsArgs.high = [];
      installInvokeMock(initial, [provider()]);
      const onOpenEffortWizard = vi.fn();
      const container = document.createElement("div");
      document.body.append(container);
      dispose = render(() => <SettingsPage onNavigateToSettings={() => undefined} onOpenEffortWizard={onOpenEffortWizard} />, container);

      await waitFor(() => container.querySelector<HTMLButtonElement>(".settings-effort-wizard-button")?.disabled === false);
      container.querySelector<HTMLButtonElement>(".settings-effort-wizard-button")!.click();

      expect(popMock).toHaveBeenCalledTimes(1);
      dispose?.();
      dispose = undefined;
      document.body.innerHTML = "";
      popMock.mockReset();
    },
  );
});
