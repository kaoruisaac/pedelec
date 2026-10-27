# Full Runtime Access: Extension / Native contract

The background owns Origin grants. Legacy `approvedOrigins` records continue to
grant Agent Sessions; only a record with `runtimeFullAccess: true` also grants Full
Runtime Access. Removing Agent Sessions removes both grants.

The first SDK Hello fixes `runtimeFullAccess` for that external Port. A request
before Hello fixes the legacy false default. Pending approval is bound to Origin,
Port, channel and scope. Popup `approvalId` identifies the displayed request or
Instance; stale Full Access selections cannot approve a replacement request.

After checking both grants and the Port's immutable Instance flag, background sends
`create_thread` or per-invocation `workspace_run` with `runtimeFullAccess: true` and a `nativeRuntimeGrant` envelope:

```json
{ "version": 1, "origin": "https://example.com", "instanceId": "instance:1",
  "instanceRequested": true, "agentSessionsApproved": true, "runtimeFullAccessApproved": true }
```

Only background builds this envelope from the external Port's sender and stored
grants; SDK fields are never spread into it. Chrome's nativeMessaging pipe is the
trusted ingress. Native Host validates the envelope and attests the entire Core
request with HMAC-SHA256, a random nonce and a 30-second expiry. The production
`NativeRuntimePlatformServices` adapter is composed by the common Desktop IPC
initializer used by Tauri. It verifies the attestation, consumes the nonce and
calls `CoreRuntime.grant_extension_runtime_full_access()` before trusted Thread or Workspace run admission. Workspace authorization is checked first; no Session is required. Toolkit directory selection is delegated to the existing Tauri service.
The adapter never falls back to payload booleans or the platform's default denial.

The signing key is generated on each Desktop start beside runtime.json, never
returned over IPC, and rotated on restart. Unix files are created with mode 0600;
on Windows the file inherits the user's profile directory ACL. Browser callers
cannot read it. Same-OS-user native programs are trusted, consistent with the
existing local Desktop control plane; this is not protection from compromised
local user accounts. Ordinary IPC cannot issue attestations. Native Host strips
arbitrary page proof fields from create_thread before admission.

Native must return boolean `runtimeFullAccessApplied`. A Full Access request
requires exactly true; missing/false acknowledgments fail closed and trigger
`abort_session_setup` for any returned thread. A Sandbox request rejects an
unexpected true acknowledgment. Legacy Sandbox responses may omit the field;
the Extension never invents a successful Full Access acknowledgment.

Revoking either grant persists a pending Origin revocation and sends:

```json
{ "type": "revoke_runtime_full_access", "requestId": "...", "callerOrigin": "https://example.com",
  "nativeRuntimeGrant": { "version": 1, "origin": "https://example.com", "revoke": true } }
```

Native must invalidate existing Full Access threads for this Origin, refuse their
subsequent high privilege execution/resume, cancel active Full Access Thread and Workspace Runtime processes and
reply with `{ "revoked": true }`. This operation must be
idempotent. A later explicit grant may authorize a new thread, but cannot revive
revoked threads. Agent Sessions and Sandbox threads stay usable after successful
Full Access revocation.

Unacknowledged revocations remain in local `runtimeFullAccessRevocations`
storage, survive service worker restarts and are retried before SDK Native
operations for every queued Origin, including after reconnect. Failures for the
current caller block its action; other Origins remain queued without denying the action. Revocation requests
time out after five seconds and remain queued on timeout/error; unrelated Origins
can continue their operations. Legacy Agent-only revocation needs no new Native
capability. In-flight creates are invalidated locally
by a grant epoch and aborted if revocation races with their Native response.
Native errors remain visible in the existing connection error state. An older
Native implementation cannot silently acknowledge either creation or revocation.

Validation: `node --test extension/background.test.js extension/localization.test.js extension/popup.test.js`
covers grant isolation, partial approval, Port/channel/Origin binding, mode
acknowledgment, stale/forged Popup messages, revocation/create races, restart
retries, approval timeout/close and the existing warning/provider/connection UI.
These are mocked transport and DOM tests; live Chrome/Native integration remains
an integration acceptance check.

The SDK sends create_session without a Full Instance status preflight, so first
use enters the existing pending approval queue/timeout and automatically opens
Popup. Agent-only approval rejects both Session modes for Full Instances.
getApprovalStatus/checkAvailability remain read-only with respect to approval.

Production integration: `cargo test -p pedelec-native-host --lib runtime_admission`
uses the actual Native admission function and Desktop server adapter, checks Core
mode acknowledgment and performs outside-Workspace I/O, host environment access,
subprocess execution, active cancellation and resume denial with managed Deno.
If Deno is unavailable, the test explicitly reports the execution portion skipped.
The installed Chrome/Tauri GUI workflow still requires manual acceptance.

Workspace runs default to Sandbox independently of Session mode. Full Access requires the Full Instance, both grants, and an explicit per-run request. Legacy Sandbox responses may omit the mode acknowledgment; a Full Access response must confirm it. Revocation invalidates in-flight and running Full Access work for the affected Origin.
