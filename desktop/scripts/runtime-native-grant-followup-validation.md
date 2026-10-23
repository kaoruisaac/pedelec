# Native Runtime grant / Approval UX follow-up

Date: 2026-10-23. Implementation record, **not independent acceptance**.

## Production path

- The SDK sends `create_session` through the existing approval queue, including Full Instances. Constructor false + Session true still rejects before transport. Status APIs remain read-only.
- External Port sender supplies Origin; its first Hello fixes Instance scope. Extension background rereads stored Agent/Full grants and creates the Native envelope itself. Page-supplied approval/envelope fields are not forwarded.
- The Chrome nativeMessaging ingress (`admit_native_message`) validates required scope and grants and attests the complete request with HMAC-SHA256, a random nonce and a 30-second lifetime.
- The common Desktop IPC initializer used by Tauri installs `NativeRuntimePlatformServices` around the toolkit directory-picker adapter. This production service verifies the proof and calls the actual Core grant/trusted Thread admission APIs. Normal IPC requests, old peers, altered Origin/Instance/payload, expired proofs and replays fail closed.
- A fresh local signing key is installed on each Desktop start. Unix mode is 0600; Windows inherits the user-profile directory ACL. The key is never exposed through IPC or the SDK. Same-OS-user native programs are inside the trust boundary; this protects against browser callers, not a compromised local account.
- Durable revocations are retried for every queued Origin before SDK Native actions. Failure for the caller blocks its action; failure for another Origin keeps that entry durable without denying the caller. Core revokes the Origin capability and Desktop cancels its active processes. Reapproval permits new Threads and never revives revoked Threads.

## Automated verification on Windows

- `node --test extension/background.test.js extension/localization.test.js extension/popup.test.js`: 123 passed.
- SDK `npm test -- --reporter=dot`: 199 passed; after adding three read-only availability cases, `npx vitest run src/index.test.ts --reporter=dot`: 158 passed. The real SDK/background integration cases cover first-use Popup pending, Agent-only refusal for both Session modes, both-grant replay and old Native acknowledgment refusal.
- SDK `npm run type-test` and `npm run build`: passed.
- `cargo test -p pedelec-core -p pedelec-ipc -p pedelec-native-host -p pedelec-runtime --lib --quiet`: Core 255, IPC 145, Native 22, Runtime 155 passed; two existing Runtime tests ignored.
- Native production integration uses the real server composition and Native admission function, with no test SessionPlatform or direct Core grant calls. Managed Deno 2.9.5 was present: outside-Workspace file I/O, inherited host environment, harmless subprocess execution, active-process cancellation, rejected resume and other-Origin isolation were exercised. The test explicitly reports an execution skip on machines without a managed Deno executable; admission checks still run.
- `cargo check -p pedelec-app --lib --quiet`: passed. Existing `CoreRuntime::provider_scan` private-interface warning remains.
- Dev-docs `npm run build`: passed, 63 pages. English/Traditional Chinese approval, Runtime security and error-code pages updated for the SDK behavior.
- `git diff --check` and `node --check extension/background.js`: passed. Modified Rust files were formatted. Whole-workspace `cargo fmt --all -- --check` still reports pre-existing formatting differences in Core, its shared Tauri tests and `pedelec-deno`; those unrelated edits were preserved.
- Initial parallel Rust run encountered a Provider version-fixture failure; its isolated rerun and subsequent full suite passed. The local remote-module HTTP fixture also initially failed after consuming a speculative empty connection; it now waits for real request headers, and the full suite passed.

## Naming assessment

SDK engine-neutral private module types and normalized data now use Runtime names. `denoModules` remains the legacy Extension wire field and public deprecated alias. Core `DenoExecutionIntent`, `DenoExecutionOwner` and `DenoRunInput` currently carry Deno-specific execution targets/arguments and serve the Deno dispatcher/CLI protocol; their Rust API names remain to avoid an unrelated cross-crate migration. Actual `Deno.*`, CLI flags and `DENO_*` errors are unchanged.

## Installed GUI acceptance still required

Not run: installed Chrome Extension + Tauri GUI end-to-end on Windows, macOS or Linux. Run these steps separately on each supported OS:

1. Install matching Desktop/Native Host and Extension builds, use a clean Chrome profile, and open the demo on a permitted HTTP localhost or HTTPS Origin. Remove prior grants.
2. Create a Full Instance and call `createSession({ provider: "codex", runtimeFullAccess: false })`. Confirm Popup opens automatically with Full unchecked. Approve Agent Sessions only: expect Authorization Incomplete and `CREATE_SESSION_NOT_APPROVED`, with no Thread created. Repeat with Session true.
3. Repeat on a clean Origin and approve both grants. Session false must report Sandbox; Session true must have a real `runtimeFullAccessApplied: true`. Run a temporary script via the Agent Runtime CLI to write/read a disposable outside-Workspace file, read a harmless host variable and launch a harmless child process. Confirm `workspace.run()` still cannot do these operations.
4. Start a long-running Full Runtime process, revoke Full Access in Popup, and confirm cancellation and rejected subsequent execution/resume. A different approved Origin's Thread must continue. Reapprove and create a new Thread; the revoked Thread must remain unusable.
5. Disconnect Native, revoke, restart the Extension worker/Desktop, then issue an SDK operation. Confirm the durable revocation is acknowledged before the action and cleared only on `{ revoked: true }`. With an old/unresponsive Native, preserve the queue and fail closed for that Origin; an unrelated Origin's ordinary action must remain usable after the bounded retry.
6. Repeat first-use creation with an ordinary legacy SDK/no Full flags; confirm its normal Popup and Sandbox behavior. Pair a new Full SDK with old Native/Desktop builds and confirm refusal rather than silent downgrade.
7. Run the production adapter tests on macOS/Linux as well, including the Unix key-file permission assertion. Preserve the GUI and platform results for the separate implementation review.
