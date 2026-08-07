import { createSignal, For, Show } from "solid-js";
import { OcTerminal2 } from "solid-icons/oc";
import { forwardPopUp } from "../services/PopUpProvider";
import {
  createInitialMissingProviderOnboardingState,
  createMissingProviderOnboardingController,
  RECOMMENDED_PROVIDERS,
} from "./missingProviderOnboarding";
import {
  isOnboardingInstallerSupported,
  type OnboardingInstallerCode,
} from "./providerInstaller";

interface MissingProviderPopupProps {
  onGoToSettings: () => void;
  onOpenProviderInstaller: (provider: OnboardingInstallerCode) => Promise<void>;
  onRestart: () => Promise<void>;
}

const MissingProviderPopup = forwardPopUp((popup, props: MissingProviderPopupProps) => {
  const [onboardingState, setOnboardingState] = createSignal(createInitialMissingProviderOnboardingState());
  const onboarding = createMissingProviderOnboardingController({
    openProviderInstaller: props.onOpenProviderInstaller,
    restartPedelec: props.onRestart,
    onStateChange: setOnboardingState,
  });

  function goToSettings(): void {
    popup.close();
    props.onGoToSettings();
  }

  return (
    <section
      class="settings-modal missing-provider-modal"
      classList={{ "is-installation-progress": onboardingState().phase === "installation-progress" }}
      role="dialog"
      aria-modal="true"
      aria-labelledby="missing-provider-title"
    >
      <Show when={onboardingState().phase === "selection"}>
        <header class="settings-modal-header">
          <h2 id="missing-provider-title">Choose an AI Agent provider</h2>
          <p>Pedelec couldn't find an AI Agent provider on this computer. Install one provider to get started — you can change it later in Settings.</p>
        </header>

        <div class="missing-provider-copy">
          <strong>Pick a provider to install</strong>
        </div>
        <div class="missing-provider-list">
          <For each={RECOMMENDED_PROVIDERS}>
            {(provider) => {
              const installable = isOnboardingInstallerSupported(provider.code);
              const launching = () => onboardingState().launchingProvider === provider.code;

              return (
                <button
                  type="button"
                  class="missing-provider-card"
                  classList={{ "is-coming-soon": !installable }}
                  disabled={!installable || onboardingState().launchingProvider !== null}
                  aria-label={installable ? `Install ${provider.name}` : `${provider.name} installation coming soon`}
                  onClick={() => void onboarding.installProvider(provider.code)}
                >
                  <span class="missing-provider-card-copy">
                    <strong>{provider.name}</strong>
                    <span>{provider.description}</span>
                  </span>
                  <span class="missing-provider-card-action" aria-hidden="true">
                    <Show when={installable} fallback="Coming soon">
                      {launching() ? "Opening Terminal…" : "Install ›"}
                    </Show>
                  </span>
                </button>
              );
            }}
          </For>
        </div>

        <Show when={onboardingState().error}>
          <div class="missing-provider-error" role="alert" aria-live="assertive">{onboardingState().error}</div>
        </Show>

        <footer class="missing-provider-footer">
          <button type="button" class="settings-inline-link-button" onClick={goToSettings}>
            View all providers in Settings →
          </button>
          <span>At least one provider is required.</span>
        </footer>
      </Show>

      <Show when={onboardingState().phase === "installation-progress"}>
        <header class="settings-modal-header">
          <h2 id="missing-provider-title">Terminal opened — complete installation and setup</h2>
          <p>After installation and sign-in are complete, close the Terminal and return to Pedelec to restart.</p>
        </header>

        <div class="missing-provider-progress" aria-live="polite">
          <OcTerminal2 class="missing-provider-progress-icon" aria-hidden="true" />
          <span>
            <strong>Terminal is ready for installation</strong>
            <span>Complete the installation and sign-in flow in your Terminal, then close it when finished.</span>
          </span>
        </div>

        <Show when={onboardingState().error}>
          <div class="missing-provider-error" role="alert" aria-live="assertive">{onboardingState().error}</div>
        </Show>

        <footer class="missing-provider-footer missing-provider-footer-progress">
          <button
            type="button"
            class="settings-primary-button"
            disabled={onboardingState().restarting}
            onClick={() => void onboarding.restart()}
          >
            {onboardingState().restarting ? "Restarting Pedelec…" : "I've finished installing — Restart Pedelec"}
          </button>
        </footer>
      </Show>
    </section>
  );
});

export default MissingProviderPopup;
