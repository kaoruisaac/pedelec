import { render } from "solid-js/web";
import { createSignal, For, Show } from "solid-js";
import { EventMonitorApp } from "./event-monitor/EventMonitorApp";
import HomePage from "./home/HomePage";
import SettingsPage from "./settings/SettingsPage";
import "./style.css";
import "./event-monitor/event-monitor.css";
import PopUpProvider from "./services/PopUpProvider";

const IS_DEV = import.meta.env.DEV;

const PAGES: Record<string, string> = {
  home: "Home",
  settings: "Settings",
  ...(IS_DEV ? { monitor: "Event Monitor" } : {}),
};

function AppShell() {
  const [page, setPage] = createSignal("home");
  const [sidebarCollapsed, setSidebarCollapsed] = createSignal(false);

  return (
    <PopUpProvider>
      <div
        class="app-shell"
        classList={{
          "is-sidebar-collapsed": sidebarCollapsed(),
        }}
      >
        <aside class="app-sidebar" aria-label="Main menu">
          <div class="app-sidebar-header">
            <Show when={!sidebarCollapsed()}>
              <strong>Pedelec</strong>
            </Show>
            <button
              type="button"
              class="app-sidebar-toggle"
              aria-label={sidebarCollapsed() ? "Expand menu" : "Collapse menu"}
              onClick={() => setSidebarCollapsed((value) => !value)}
            >
              {sidebarCollapsed() ? ">" : "<"}
            </button>
          </div>
          <nav class="app-nav">
            <For each={Object.entries(PAGES)}>
              {([key, label]) => (
                <button
                  type="button"
                  class="app-nav-item"
                  classList={{ "is-active": page() === key }}
                  title={label}
                  onClick={() => setPage(key)}
                >
                  <span class="app-nav-icon" aria-hidden="true">
                    {key === "home" ? "H" : key === "settings" ? "S" : "E"}
                  </span>
                  <Show when={!sidebarCollapsed()}>
                    <span>{label}</span>
                  </Show>
                </button>
              )}
            </For>
          </nav>
        </aside>

        <section class="app-page">
          <div hidden={page() !== "home"}>
            <HomePage />
          </div>
          <div hidden={page() !== "settings"}>
            <SettingsPage />
          </div>
          <Show when={IS_DEV}>
            <div hidden={page() !== "monitor"}>
              <EventMonitorApp />
            </div>
          </Show>
        </section>
      </div>
    </PopUpProvider>
  );
}

const dispose = render(() => <AppShell />, document.getElementById("root")!);

if (import.meta.hot) {
  import.meta.hot.dispose(dispose);
}
