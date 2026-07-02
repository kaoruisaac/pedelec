import { AiOutlineChrome } from "solid-icons/ai";
import { openExternalUrl } from "../utils/openExternalUrl";
import "./HomePage.css";

const CHROME_WEB_STORE_URL =
  "https://chromewebstore.google.com/detail/pedelec/ogccgaminlphbkeghldidiiimajfdpag";

function HomePage() {
  const handleOpenChromeWebStore = () => {
    void openExternalUrl(CHROME_WEB_STORE_URL);
  };

  return (
    <main class="home-page">
      <section class="home-card" aria-labelledby="home-title">
        <img class="home-icon" src="../../app-icon.png" alt="Pedelec" />
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
      </section>
    </main>
  );
}

export default HomePage;
