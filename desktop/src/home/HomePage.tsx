import { AiOutlineChrome } from "solid-icons/ai";
import "./HomePage.css";

const CHROME_WEB_STORE_URL =
  "https://chromewebstore.google.com/detail/pedelec/ogccgaminlphbkeghldidiiimajfdpag";

function HomePage() {
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
        <a
          class="home-primary-link"
          href={CHROME_WEB_STORE_URL}
          target="_blank"
          rel="noreferrer"
        >
          <AiOutlineChrome size={24} />
          <span>Get Pedelec Extension on Chrome Web Store</span>
        </a>
      </section>
    </main>
  );
}

export default HomePage;
