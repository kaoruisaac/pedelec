/** @vitest-environment jsdom */

import { render } from "solid-js/web";
import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import SettingsPage from "./SettingsPage";
import { Provider, ProviderCode, Settings } from "./types";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

vi.mock("../services/PopUpProvider", () => ({
  usePopUp: () => ({ pop: () => undefined }),
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
});
