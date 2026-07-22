import { render } from "solid-js/web";
import { createSignal, For, onMount, Show } from "solid-js";
import { getVersion } from "@tauri-apps/api/app";
import { updateStore } from "./updater/updateStore";
import { EventMonitorApp } from "./event-monitor/EventMonitorApp";
import HomePage from "./home/HomePage";
import SettingsPage from "./settings/SettingsPage";
import "./style.css";
import "./event-monitor/event-monitor.css";
import PopUpProvider from "./services/PopUpProvider";
import { FaSolidChevronLeft, FaSolidChevronRight } from "solid-icons/fa";
import { FiDownload } from "solid-icons/fi";

const IS_DEV = import.meta.env.DEV;

const PAGES: Record<string, string> = {
  home: "Home",
  settings: "Settings",
  ...(IS_DEV ? { monitor: "Event Monitor" } : {}),
};

function AppShell() {
  const [page, setPage] = createSignal("home");
  const [sidebarCollapsed, setSidebarCollapsed] = createSignal(false);
  const [appVersion, setAppVersion] = createSignal<string | null>(null);

  onMount(() => {
    void getVersion().then(setAppVersion).catch(() => {});
    void updateStore.checkForUpdate();
  });

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
              <div class="app-sidebar-brand">
                <strong class="app-sidebar-name">Pedelec</strong>
                <Show when={appVersion()} keyed>
                  {(version) => <span class="app-sidebar-version">v{version}</span>}
                </Show>
              </div>
            </Show>
            <Show when={updateStore.state().status !== "idle" && updateStore.state().status !== "checking"}>
              <div class="app-sidebar-update">
                <Show when={updateStore.state().status === "available"}>
                  <button
                    type="button"
                    class="app-update-button"
                    aria-label={`Update to v${updateStore.state().availableVersion}`}
                    title={`Update to v${updateStore.state().availableVersion}`}
                    onClick={() => void updateStore.installUpdate()}
                  >
                    <Show when={!sidebarCollapsed()} fallback="↑">
                      <FiDownload style={{ "margin-right": "5px" }} />
                      <span>Pelect needs update</span>
                    </Show>
                  </button>
                </Show>
                <Show when={updateStore.state().status === "downloading"}>
                  <span class="app-update-status" aria-live="polite">
                    <Show
                      when={updateStore.state().progressPercent !== null}
                      fallback="Downloading…"
                    >
                      Downloading {updateStore.state().progressPercent}%
                    </Show>
                  </span>
                </Show>
                <Show when={updateStore.state().status === "installing"}>
                  <span class="app-update-status" aria-live="polite">Installing…</span>
                </Show>
                <Show when={updateStore.state().status === "failed"}>
                  <button
                    type="button"
                    class="app-update-button app-update-retry"
                    aria-label="Retry update"
                    title="Update failed. Retry update"
                    onClick={() => void updateStore.retryUpdate()}
                  >
                    <Show when={!sidebarCollapsed()} fallback="!"><span>Update failed · Retry</span></Show>
                  </button>
                </Show>
              </div>
            </Show>
            <button
              type="button"
              class="app-sidebar-toggle"
              aria-label={sidebarCollapsed() ? "Expand menu" : "Collapse menu"}
              onClick={() => setSidebarCollapsed((value) => !value)}
            >
              {sidebarCollapsed() ? <FaSolidChevronRight size={14} /> : <FaSolidChevronLeft size={14} />}
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
