import { forwardPopUp } from "../services/PopUpProvider";

interface MissingProviderPopupProps {
  onGoToSettings: () => void;
}

const MissingProviderPopup = forwardPopUp((popup, props: MissingProviderPopupProps) => (
  <section class="settings-modal" role="dialog" aria-modal="true" aria-labelledby="missing-provider-title">
    <header class="settings-modal-header">
      <h2 id="missing-provider-title">Pedelec cannot be used yet</h2>
    </header>
    <p>No available Agent Provider was found on this computer. Set up an Agent Provider on the Settings page to use Pedelec.</p>
    <footer class="settings-modal-actions">
      <button
        type="button"
        class="settings-primary-button"
        onClick={() => {
          popup.close();
          props.onGoToSettings();
        }}
      >
        Go to Settings
      </button>
    </footer>
  </section>
));

export default MissingProviderPopup;
