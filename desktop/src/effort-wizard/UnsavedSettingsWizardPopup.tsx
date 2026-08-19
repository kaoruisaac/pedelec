import type { PopUp } from "../services/PopUpProvider";

interface UnsavedSettingsWizardPopupProps {
  onDiscardAndContinue: () => void;
}

export default function UnsavedSettingsWizardPopup(props: UnsavedSettingsWizardPopupProps & { popup?: PopUp<UnsavedSettingsWizardPopupProps> }) {
  return (
    <section class="settings-modal effort-wizard-popup" role="dialog" aria-modal="true" aria-labelledby="effort-wizard-unsaved-title">
    <header class="settings-modal-header">
      <h2 id="effort-wizard-unsaved-title">Unsaved settings</h2>
      <p>Save or discard your current Settings changes before running Effort Wizard.</p>
    </header>
    <div class="settings-modal-actions">
      <button type="button" class="effort-wizard-secondary-button" onClick={() => props.popup?.close()}>Cancel</button>
      <button type="button" class="effort-wizard-primary-button" onClick={() => { props.popup?.close(); props.onDiscardAndContinue(); }}>Discard and continue</button>
    </div>
  </section>
  );
}
