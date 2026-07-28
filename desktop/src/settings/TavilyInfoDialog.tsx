import { openExternalUrl } from "../utils/openExternalUrl";

interface TavilyInfoDialogProps {
  onClose: () => void;
}

const TAVILY_HOMEPAGE_URL = "https://www.tavily.com/";

export default function TavilyInfoDialog(props: TavilyInfoDialogProps) {
  return (
    <section
      class="settings-modal settings-info-dialog"
      role="dialog"
      aria-modal="true"
      aria-labelledby="tavily-info-title"
    >
      <header class="settings-modal-header">
        <h2 id="tavily-info-title">About Tavily</h2>
      </header>

      <p class="settings-info-dialog-copy">
        Tavily is a search engine built specifically for AI agents. You can create a free account and get an API key in just a few minutes. Add the key here to give your Ollama model access to web search.
        <br />
        <a
          href={TAVILY_HOMEPAGE_URL}
          onClick={(event) => {
            event.preventDefault();
            void openExternalUrl(TAVILY_HOMEPAGE_URL);
          }}
        >
          Go To Tavily
        </a>
      </p>

      <footer class="settings-modal-actions">
        <button type="button" class="settings-primary-button" autofocus onClick={props.onClose}>
          Close
        </button>
      </footer>
    </section>
  );
}
