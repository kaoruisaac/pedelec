import { AiOutlineChrome } from "solid-icons/ai";
import { openExternalUrl } from "../utils/openExternalUrl";
import appIcon from "../../app-icon.png?url";
import { availableWizardProviders } from "../effort-wizard/effortWizardSelectors";
import type { EffortWizardStore } from "../effort-wizard/effortWizardStore";
import "./HomePage.css";

const CHROME_WEB_STORE_URL =
  "https://chromewebstore.google.com/detail/pedelec/ogccgaminlphbkeghldidiiimajfdpag";
const LIVE_DEMO_URL = "https://pedelec.cc/demos";

interface HomePageProps {
  wizard: EffortWizardStore;
  onOpenWizard: (origin: "home-initial" | "home-update") => void;
}

function HomePage(props: HomePageProps) {
  const reminder = () => props.wizard.store.bootstrap?.homeReminder;
  const availableCount = () => availableWizardProviders(props.wizard.store.bootstrap).length;
  const updateProviders = () => {
    const currentReminder = reminder();
    return currentReminder?.type === "preset_update" ? currentReminder.providers : [];
  };

  const handleOpenChromeWebStore = () => {
    void openExternalUrl(CHROME_WEB_STORE_URL);
  };

  const handleOpenLiveDemo = () => {
    void openExternalUrl(LIVE_DEMO_URL);
  };

  return (
    <main class="home-page">
      <div class="home-content">
        <section class="home-card" aria-labelledby="home-title">
          <img class="home-icon" src={appIcon} alt="Pedelec" />
          <h1 id="home-title">Welcome to Pedelec</h1>
          <p>Pedelec is the bridge that connects AI Agents on your computer to Chrome.</p>
          <p>
            Pedelec App cannot work alone - you need to install the Pedelec Chrome
            Extension.
          </p>
          <div class="home-divider" aria-hidden="true" />
          <button
            type="button"
            class="home-primary-link"
            onClick={handleOpenChromeWebStore}
          >
            <AiOutlineChrome size={24} />
            <span>Get Pedelec Extension on Chrome Web Store</span>
          </button>
          <button type="button" class="home-demo-link" onClick={handleOpenLiveDemo}>
            Visit live demo →
          </button>
        </section>

        <div class="home-reminder-slot">
          {reminder()?.type === "initial_setup" ? (
            <section class="home-effort-reminder" aria-labelledby="home-effort-initial-title">
              <div class="home-effort-reminder-header">
                <div>
                  <span class="home-effort-eyebrow">Recommended</span>
                  <h2 id="home-effort-initial-title">Set up effort profiles</h2>
                </div>
                <span class="home-effort-count">{availableCount()} supported {availableCount() === 1 ? "provider" : "providers"} available</span>
              </div>
              <p>Check Pedelec-maintained recommendations for your available supported providers, then review every tier before applying.</p>
              <span class="home-effort-hint">You can always run the wizard again from Settings.</span>
              <button type="button" class="home-effort-primary-link" onClick={() => props.onOpenWizard("home-initial")}>Check recommendations</button>
            </section>
          ) : reminder()?.type === "preset_update" ? (
            <section class="home-effort-reminder is-update" aria-labelledby="home-effort-update-title">
              <div class="home-effort-reminder-header">
                <div>
                  <span class="home-effort-eyebrow">Please review</span>
                  <h2 id="home-effort-update-title">Updated effort recommendations available</h2>
                </div>
                <span class="home-effort-count">{updateProviders().length} {updateProviders().length === 1 ? "provider has" : "providers have"} newer recommendations</span>
              </div>
              <p>Pedelec has updated its maintained recommendations. This does not mean your current settings are wrong.</p>
              <span class="home-effort-hint">Current settings will not change until you review and apply.</span>
              <button type="button" class="home-effort-primary-link" onClick={() => props.onOpenWizard("home-update")}>Check recommendations</button>
            </section>
          ) : null}
        </div>
      </div>
    </main>
  );
}

export default HomePage;
