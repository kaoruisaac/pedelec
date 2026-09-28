import { createEffect, Show } from "solid-js";
import type { PreparationUiState } from "./preparationState";

const PREPARING_COPY = "Preparing Pedelec…";
const FAILED_COPY = "Pedelec couldn't finish preparing.";
const FINALIZING_COPY = "Finishing up…";

export function PreparationMask(props: {
  state: () => PreparationUiState;
  onRetry: () => void;
}) {
  let mask: HTMLDivElement | undefined;
  let retryButton: HTMLButtonElement | undefined;

  let focusedStatus: PreparationUiState["status"] | undefined;

  createEffect(() => {
    const status = props.state().status;
    if (status === focusedStatus) return;
    focusedStatus = status;
    if (status === "failed") {
      retryButton?.focus();
      return;
    }
    mask?.focus();
  });

  return (
    <div
      ref={mask}
      class="app-preparation-mask"
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="app-preparation-status"
      aria-busy={props.state().status === "failed" ? "false" : "true"}
      tabindex="-1"
    >
      <strong class="app-preparation-brand">Pedelec</strong>
      <Show when={props.state().status !== "failed"}>
        <div class="app-preparation-spinner" aria-hidden="true" />
      </Show>
      <p id="app-preparation-status" class="app-preparation-status" aria-live="polite">
        {props.state().status === "failed" ? FAILED_COPY : PREPARING_COPY}
      </p>
      <Show when={props.state().status === "finalizing"}>
        <p class="app-preparation-detail">{FINALIZING_COPY}</p>
      </Show>
      <Show when={props.state().progressPercent !== null}>
        <p class="app-preparation-percent">{props.state().progressPercent}%</p>
      </Show>
      <Show when={props.state().status === "failed"}>
        <button
          ref={retryButton}
          type="button"
          class="settings-primary-button app-preparation-retry"
          disabled={props.state().retryDisabled}
          onClick={() => props.onRetry()}
        >
          Retry
        </button>
      </Show>
    </div>
  );
}
