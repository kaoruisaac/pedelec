const connectionEl = document.getElementById("connection");
const approvalEl = document.getElementById("approval");
const revocationEl = document.getElementById("revocation");
const originEl = document.getElementById("origin");
const approvedOriginEl = document.getElementById("approved-origin");
const approveButton = document.getElementById("approve");
const rejectButton = document.getElementById("reject");
const revokeButton = document.getElementById("revoke");
const fullPermissionsEl = document.getElementById("full-permissions");
const allowAgentEl = document.getElementById("allow-agent");
const allowFullEl = document.getElementById("allow-full");
const fullWarningEl = document.getElementById("full-warning");
const authorizationTitleEl = document.getElementById("authorization-title");
const incompleteEl = document.getElementById("authorization-incomplete");
const reviewButton = document.getElementById("review-permissions");
const revokeFullButton = document.getElementById("revoke-full");
const providerErrorEl = document.getElementById("provider-error");
const providerErrorProviderEl = document.getElementById("provider-error-provider");
const providerErrorMessageEl = document.getElementById("provider-error-message");
const dismissProviderErrorButton = document.getElementById("dismiss-provider-error");
const sdkVersionWarningEl = document.getElementById("sdk-version-warning");
const dismissSdkVersionWarningButton = document.getElementById("dismiss-sdk-version-warning");
const desktopVersionWarningEl = document.getElementById("desktop-version-warning");
const dismissDesktopVersionWarningButton = document.getElementById("dismiss-desktop-version-warning");
const port = chrome.runtime.connect({ name: "popup" });
let currentApprovalState = null;

PedelecI18n.applyDocumentTranslations(document);

function render(state) {
  connectionEl.textContent = state.connected
    ? PedelecI18n.getMessageOrFallback("connectionConnected", "connected")
    : PedelecI18n.getMessageOrFallback("connectionDisconnected", "disconnected");
  connectionEl.dataset.connected = String(Boolean(state.connected));
}

function renderApprovalState(approvalState) {
  currentApprovalState = approvalState || null;
  approvalEl.hidden = true;
  revocationEl.hidden = true;
  originEl.textContent = "";
  approvedOriginEl.textContent = "";
  rejectButton.hidden = true;
  fullPermissionsEl.hidden = !approvalState?.runtimeFullAccess;
  allowAgentEl.checked = true;
  allowFullEl.checked = approvalState?.runtimeFullAccessApproved === true;
  approveButton.disabled = false;
  fullWarningEl.hidden = allowFullEl.checked;
  incompleteEl.hidden = true;
  reviewButton.hidden = true;
  revokeFullButton.hidden = true;

  if (!approvalState?.origin) {
    return;
  }

  if (approvalState.approved || (approvalState.agentSessionsApproved && !approvalState.pending)) {
    revocationEl.hidden = false;
    approvedOriginEl.textContent = approvalState.origin;
    const incomplete = !approvalState.approved;
    document.querySelector('[data-i18n="approvedSiteDescription"]').hidden = incomplete;
    authorizationTitleEl.textContent = incomplete
      ? PedelecI18n.getMessageOrFallback("authorizationIncomplete", "Authorization Incomplete")
      : PedelecI18n.getMessageOrFallback("approvedSiteTitle", "Approved Site");
    incompleteEl.hidden = !incomplete;
    reviewButton.hidden = !incomplete;
    revokeFullButton.hidden = !approvalState.runtimeFullAccess || !approvalState.runtimeFullAccessApproved;
    revokeButton.textContent = approvalState.runtimeFullAccess
      ? PedelecI18n.getMessageOrFallback("revokeAgentSessions", "Revoke Agent Sessions")
      : PedelecI18n.getMessageOrFallback("revokeApproval", "Revoke Approval");
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

function renderVersionWarning(element, warning) {
  element.hidden = warning?.outdated !== true;
}

function closePopup() {
  window.close();
}

approveButton.addEventListener("click", () => {
  if (!currentApprovalState?.origin) return;
  port.postMessage({ type: "approve_origin", origin: currentApprovalState.origin,
    approvalId: currentApprovalState.approvalId,
    agentSessions: !currentApprovalState.runtimeFullAccess || allowAgentEl.checked,
    runtimeFullAccess: currentApprovalState.runtimeFullAccess === true && allowFullEl.checked });
});

allowAgentEl.addEventListener("change", () => {
  if (!allowAgentEl.checked) allowFullEl.checked = false;
  approveButton.disabled = !allowAgentEl.checked;
  fullWarningEl.hidden = allowAgentEl.checked && allowFullEl.checked;
});
allowFullEl.addEventListener("change", () => {
  if (allowFullEl.checked) allowAgentEl.checked = true;
  approveButton.disabled = !allowAgentEl.checked;
  fullWarningEl.hidden = allowAgentEl.checked && allowFullEl.checked;
});
reviewButton.addEventListener("click", () => {
  revocationEl.hidden = true;
  approvalEl.hidden = false;
  originEl.textContent = currentApprovalState.origin;
});
revokeFullButton.addEventListener("click", () => {
  if (!currentApprovalState?.origin) return;
  port.postMessage({ type: "revoke_runtime_full_access", origin: currentApprovalState.origin,
    approvalId: currentApprovalState.approvalId });
});

rejectButton.addEventListener("click", () => {
  port.postMessage({ type: "reject_pending_approval", approvalId: currentApprovalState?.approvalId });
  closePopup();
});

revokeButton.addEventListener("click", () => {
  if (!currentApprovalState?.origin) return;
  port.postMessage({ type: "revoke_origin", origin: currentApprovalState.origin,
    approvalId: currentApprovalState.approvalId });
});

dismissProviderErrorButton.addEventListener("click", () => {
  port.postMessage({ type: "dismiss_provider_error" });
});

dismissSdkVersionWarningButton.addEventListener("click", () => {
  port.postMessage({ type: "dismiss_sdk_version_warning" });
});

dismissDesktopVersionWarningButton.addEventListener("click", () => {
  port.postMessage({ type: "dismiss_desktop_version_warning" });
});

port.onMessage.addListener((message) => {
  if (message?.type === "state") render(message.state);
  if (message?.type === "approval_state") renderApprovalState(message.approvalState);
  if (message?.type === "provider_error_state") renderProviderError(message.providerError);
  if (message?.type === "sdk_version_warning_state") renderVersionWarning(sdkVersionWarningEl, message.warning);
  if (message?.type === "desktop_version_warning_state") renderVersionWarning(desktopVersionWarningEl, message.warning);
});

port.postMessage({ type: "get_state" });
port.postMessage({ type: "get_popup_approval_state" });
