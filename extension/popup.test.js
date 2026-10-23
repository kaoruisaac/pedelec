const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function popupFixture() {
  const html = fs.readFileSync(path.join(__dirname, "popup.html"), "utf8");
  const elements = new Map([...html.matchAll(/<[^>]*\bid="([^"]+)"[^>]*>/g)].map(([tag, id]) => [
    id, { hidden: /\bhidden\b/.test(tag), checked: /\bchecked\b/.test(tag),
      textContent: "", dataset: {}, listeners: {}, addEventListener(type, listener) { this.listeners[type] = listener; } },
  ]));
  const sent = [];
  let receive;
  let closed = false;
  const approvedDescription = { hidden: false };
  const context = {
    document: {
      getElementById: (id) => { assert.ok(elements.has(id), id); return elements.get(id); },
      querySelector: () => approvedDescription,
    },
    chrome: { runtime: { connect: () => ({
      postMessage: (message) => sent.push(message), onMessage: { addListener: (listener) => { receive = listener; } },
    }) } },
    window: { close: () => { closed = true; } },
    PedelecI18n: { applyDocumentTranslations() {}, getMessageOrFallback: (_, fallback) => fallback },
  };
  vm.runInNewContext(fs.readFileSync(path.join(__dirname, "popup.js"), "utf8"), context);
  return {
    elements, sent, approvedDescription, get closed() { return closed; },
    render: (approvalState) => receive({ type: "approval_state", approvalState }),
    receive, click: (id) => elements.get(id).listeners.click(),
    change: (id, checked) => { elements.get(id).checked = checked; elements.get(id).listeners.change(); },
  };
}

const target = { origin: "https://example.test", approvalId: "instance:1", pending: false,
  approved: false, runtimeFullAccess: false, agentSessionsApproved: false, runtimeFullAccessApproved: false };

test("Standard Instances never display Full Access controls, including approved Origins", () => {
  const popup = popupFixture();
  for (const approved of [false, true]) {
    popup.render({ ...target, approved, agentSessionsApproved: approved, runtimeFullAccessApproved: true });
    assert.equal(popup.elements.get("full-permissions").hidden, true);
    assert.equal(popup.elements.get("revoke-full").hidden, true);
  }
  popup.click("revoke");
  assert.equal(popup.sent.at(-1).type, "revoke_origin");
});

test("Full Instance defaults unchecked and sends only the selected grants with the displayed identity", () => {
  const popup = popupFixture();
  popup.render({ ...target, runtimeFullAccess: true, pending: true });
  assert.equal(popup.elements.get("full-permissions").hidden, false);
  assert.equal(popup.elements.get("allow-full").checked, false);
  popup.click("approve");
  assert.equal(popup.sent.at(-1).runtimeFullAccess, false);
  assert.equal(popup.sent.at(-1).agentSessions, true);
  assert.equal(popup.sent.at(-1).approvalId, target.approvalId);
  popup.change("allow-full", true);
  popup.click("approve");
  assert.equal(popup.sent.at(-1).runtimeFullAccess, true);
  popup.change("allow-agent", false);
  assert.equal(popup.elements.get("allow-full").checked, false);
  assert.equal(popup.elements.get("approve").disabled, true);
  popup.change("allow-full", true);
  assert.equal(popup.elements.get("allow-agent").checked, true);
  assert.equal(popup.elements.get("approve").disabled, false);
});

test("partial approval displays Authorization Incomplete and offers permission review", () => {
  const popup = popupFixture();
  popup.render({ ...target, runtimeFullAccess: true, agentSessionsApproved: true });
  assert.equal(popup.elements.get("revocation").hidden, false);
  assert.equal(popup.elements.get("authorization-title").textContent, "Authorization Incomplete");
  assert.equal(popup.elements.get("authorization-incomplete").hidden, false);
  assert.equal(popup.approvedDescription.hidden, true);
  popup.click("review-permissions");
  assert.equal(popup.elements.get("approval").hidden, false);
  assert.equal(popup.elements.get("origin").textContent, target.origin);
  assert.equal(popup.elements.get("allow-full").checked, false);
});

test("Full Access and Agent Sessions revocations send separate messages", () => {
  const popup = popupFixture();
  popup.render({ ...target, runtimeFullAccess: true, approved: true,
    agentSessionsApproved: true, runtimeFullAccessApproved: true });
  assert.equal(popup.elements.get("revoke-full").hidden, false);
  popup.click("revoke-full");
  assert.equal(popup.sent.at(-1).type, "revoke_runtime_full_access");
  popup.click("revoke");
  assert.equal(popup.sent.at(-1).type, "revoke_origin");
  assert.equal(popup.sent.at(-1).approvalId, target.approvalId);
});

test("popup retains Reject, connection pill, provider error and version warning behavior", () => {
  const popup = popupFixture();
  popup.render({ ...target, runtimeFullAccess: true, pending: true });
  popup.click("reject");
  assert.equal(popup.closed, true);
  assert.equal(popup.sent.at(-1).type, "reject_pending_approval");
  assert.equal(popup.sent.at(-1).approvalId, target.approvalId);
  popup.receive({ type: "state", state: { connected: true } });
  assert.equal(popup.elements.get("connection").dataset.connected, "true");
  popup.receive({ type: "provider_error_state", providerError: { provider: "codex", message: "failure" } });
  assert.equal(popup.elements.get("provider-error-message").textContent, "failure");
  for (const kind of ["sdk", "desktop"]) {
    popup.receive({ type: kind + "_version_warning_state", warning: { outdated: true } });
    assert.equal(popup.elements.get(kind + "-version-warning").hidden, false);
    popup.click("dismiss-" + kind + "-version-warning");
    assert.equal(popup.sent.at(-1).type, "dismiss_" + kind + "_version_warning");
  }
});
