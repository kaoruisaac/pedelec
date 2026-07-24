const connectionEl = document.getElementById("connection");
const approvalEl = document.getElementById("approval");
const revocationEl = document.getElementById("revocation");
const originEl = document.getElementById("origin");
const approvedOriginEl = document.getElementById("approved-origin");
const approveButton = document.getElementById("approve");
const rejectButton = document.getElementById("reject");
const revokeButton = document.getElementById("revoke");
const providerErrorEl = document.getElementById("provider-error");
const providerErrorProviderEl = document.getElementById("provider-error-provider");
const providerErrorMessageEl = document.getElementById("provider-error-message");
const dismissProviderErrorButton = document.getElementById("dismiss-provider-error");
const port = chrome.runtime.connect({ name: "popup" });
let currentApprovalState = null;

function render(state) {
  connectionEl.textContent = state.connected ? "connected" : "disconnected";
  connectionEl.dataset.connected = String(Boolean(state.connected));
}

function renderApprovalState(approvalState) {
  currentApprovalState = approvalState || null;
  approvalEl.hidden = true;
  revocationEl.hidden = true;
  originEl.textContent = "";
  approvedOriginEl.textContent = "";
  rejectButton.hidden = true;

  if (!approvalState?.origin) {
    return;
  }

  if (approvalState.approved) {
    revocationEl.hidden = false;
    approvedOriginEl.textContent = approvalState.origin;
    return;
  }

  approvalEl.hidden = false;
  originEl.textContent = approvalState.origin;
  rejectButton.hidden = !approvalState.pending;
}

function renderProviderError(providerError) {
  if (providerError == null) {
    providerErrorEl.hidden = true;
    providerErrorProviderEl.textContent = "";
    providerErrorMessageEl.textContent = "";
    return;
  }

  providerErrorProviderEl.textContent = providerError.provider;
  providerErrorMessageEl.textContent = providerError.message;
  providerErrorEl.hidden = false;
}

function closePopup() {
  window.close();
}

approveButton.addEventListener("click", () => {
  if (!currentApprovalState?.origin) return;
  port.postMessage({ type: "approve_origin", origin: currentApprovalState.origin });
});

rejectButton.addEventListener("click", () => {
  port.postMessage({ type: "reject_pending_approval" });
  closePopup();
});

revokeButton.addEventListener("click", () => {
  if (!currentApprovalState?.origin) return;
  port.postMessage({ type: "revoke_origin", origin: currentApprovalState.origin });
});

dismissProviderErrorButton.addEventListener("click", () => {
  port.postMessage({ type: "dismiss_provider_error" });
});

port.onMessage.addListener((message) => {
  if (message?.type === "state") render(message.state);
  if (message?.type === "approval_state") renderApprovalState(message.approvalState);
  if (message?.type === "provider_error_state") renderProviderError(message.providerError);
});

port.postMessage({ type: "get_state" });
port.postMessage({ type: "get_popup_approval_state" });
