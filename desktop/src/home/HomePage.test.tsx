/** @vitest-environment jsdom */

import { render } from "solid-js/web";
import { createStore } from "solid-js/store";
import { afterEach, describe, expect, it, vi } from "vitest";
import HomePage from "./HomePage";
import type { EffortWizardStore } from "../effort-wizard/effortWizardStore";
import type { EffortWizardBootstrap } from "../effort-wizard/types";

function bootstrap(homeReminder: EffortWizardBootstrap["homeReminder"]): EffortWizardBootstrap {
  return {
    providers: [
      { provider: "codex", available: true, version: null, currentPresetRevision: 2, appliedPresetRevision: 2, hasAnyEffortSetting: false, presetUpdateAvailable: false },
      { provider: "claude", available: false, version: null, currentPresetRevision: 2, appliedPresetRevision: null, hasAnyEffortSetting: false, presetUpdateAvailable: true },
      { provider: "cursor", available: true, version: null, currentPresetRevision: 2, appliedPresetRevision: 1, hasAnyEffortSetting: false, presetUpdateAvailable: true },
      { provider: "antigravity", available: false, version: null, currentPresetRevision: 2, appliedPresetRevision: null, hasAnyEffortSetting: false, presetUpdateAvailable: false },
    ],
    homeReminder,
  };
}

function home(
  reminder: EffortWizardBootstrap["homeReminder"],
  value: EffortWizardBootstrap | null = bootstrap(reminder),
): EffortWizardStore {
  return { store: { bootstrap: value } } as EffortWizardStore;
}

describe("HomePage effort reminder", () => {
  let dispose: (() => void) | undefined;

  afterEach(() => {
    dispose?.();
    dispose = undefined;
    document.body.innerHTML = "";
  });

  it("renders the Welcome card before the conditional reminder", () => {
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <HomePage wizard={home({ type: "initial_setup" })} onOpenWizard={() => undefined} />, container);

    const content = container.querySelector(".home-content")!;
    expect(content.children[0].classList.contains("home-card")).toBe(true);
    expect(content.children[1].classList.contains("home-reminder-slot")).toBe(true);
    expect(container.textContent).toContain("Set up effort profiles");
  });

  it("renders the Initial Setup card with the supported-provider count and origin", () => {
    const onOpenWizard = vi.fn();
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <HomePage wizard={home({ type: "initial_setup" })} onOpenWizard={onOpenWizard} />, container);

    expect(container.textContent).toContain("Set up effort profiles");
    expect(container.textContent).toContain("Recommended");
    expect(container.textContent).toContain("2 supported providers available");
    expect(container.textContent).toContain("Check recommendations");
    expect(container.textContent).not.toContain("helper");
    container.querySelector<HTMLButtonElement>(".home-effort-primary-link")!.click();
    expect(onOpenWizard).toHaveBeenCalledWith("home-initial");
  });

  it("shows no reminder when bootstrap is loading or has no reminder", () => {
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <HomePage wizard={home(null, null)} onOpenWizard={() => undefined} />, container);

    expect(container.querySelector(".home-effort-reminder")).toBeNull();
    expect(container.textContent).not.toContain("Check recommendations");
  });

  it("renders the update reminder count from the backend reminder", () => {
    const onOpenWizard = vi.fn();
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <HomePage wizard={home({ type: "preset_update", providers: ["cursor"] })} onOpenWizard={onOpenWizard} />, container);

    expect(container.textContent).toContain("1 provider has newer recommendations");
    expect(container.textContent).toContain("This does not mean your current settings are wrong");
    expect(container.textContent).not.toContain("helper");
    container.querySelector<HTMLButtonElement>(".home-effort-primary-link")!.click();
    expect(onOpenWizard).toHaveBeenCalledWith("home-update");
  });

  it("reacts to bootstrap reminder removal and provider-count changes without remounting", async () => {
    const [wizardState, setWizardState] = createStore({ bootstrap: bootstrap({ type: "preset_update", providers: ["codex", "cursor"] }) });
    const wizard = { store: wizardState } as unknown as EffortWizardStore;
    const container = document.createElement("div");
    document.body.append(container);
    dispose = render(() => <HomePage wizard={wizard} onOpenWizard={() => undefined} />, container);

    expect(container.textContent).toContain("2 providers have newer recommendations");
    setWizardState("bootstrap", "homeReminder", { type: "preset_update", providers: ["cursor"] });
    await Promise.resolve();
    expect(container.textContent).toContain("1 provider has newer recommendations");

    setWizardState("bootstrap", "homeReminder", null);
    await Promise.resolve();
    expect(container.querySelector(".home-effort-reminder")).toBeNull();
  });
});
