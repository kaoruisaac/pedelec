import type { PopUp } from "../services/PopUpProvider";

interface ExistingSettingsWarningPopupProps {
  onContinue: () => void;
}

export default function ExistingSettingsWarningPopup(props: ExistingSettingsWarningPopupProps & { popup?: PopUp<ExistingSettingsWarningPopupProps> }) {
  return (
    <section class="settings-modal effort-wizard-popup" role="dialog" aria-modal="true" aria-labelledby="effort-wizard-existing-title">
    <header class="settings-modal-header">
      <h2 id="effort-wizard-existing-title">Review existing effort settings</h2>
      <p>Some supported providers already have effort profiles configured.</p>
    </header>
    <p class="effort-wizard-popup-copy">
      The wizard will only change confirmed tiers that you explicitly choose to update. Unchecked tiers, failed checks, and unavailable providers keep their current settings. Nothing changes before Apply.
    </p>
    <div class="settings-modal-actions">
      <button type="button" class="effort-wizard-secondary-button" onClick={() => props.popup?.close()}>Cancel</button>
      <button type="button" class="effort-wizard-primary-button" onClick={() => { props.popup?.close(); props.onContinue(); }}>Continue</button>
    </div>
  </section>
  );
}
