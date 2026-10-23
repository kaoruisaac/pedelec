# Runtime Evolution Phase 5 acceptance

## Repeatable validation

Install the lockfile dependencies in `sdk`, `dev-docs`, and `demo/solid-sdk-demo` first. Rust requires the repository's normal native build prerequisites. From the repository root:

```powershell
# Use the pinned managed engine, not either Pedelec CLI wrapper.
$env:PEDELEC_TEST_DENO = 'C:\path\to\managed\deno.exe'
node desktop/scripts/validate-runtime-evolution.mjs --real
```

On macOS/Linux, use `PEDELEC_TEST_DENO=/path/to/managed/deno node desktop/scripts/validate-runtime-evolution.mjs --real`.
The runner verifies the engine version and Sandbox smoke first, then runs each component independently and exits nonzero if any fails. It includes SDK test/type-test/build, Extension tests, eight Rust crates, release metadata/staging tests, docs build, and Demo typecheck/build. Without `--real`, real execution fixtures can skip when the managed runtime is unavailable; that run alone is insufficient for security acceptance. Two explicitly ignored runtime tests (public npm registry and authenticated Claude CLI) require a separate opt-in; review `cargo test -p pedelec-runtime -- --list --ignored` before running them with network access.

## Matrix and assertions

| ID | Automated evidence | Boundary asserted |
| --- | --- | --- |
| A1 | SDK existing default tests; Extension legacy-handshake tests; Core `legacy_calls_stay_sandbox_and_cannot_self_authorize` | Old callers need no new flags and stay Sandbox |
| A2 | `sdk/src/runtime-integration.test.ts`; SDK input validation; Extension Standard Instance tests | Instance false + Session true never reaches Native create |
| A3 | SDK/Extension integration; Extension partial approval tests | Full Instance without both grants cannot create either mode |
| A4–A5 | SDK/Extension integration; Extension grant tests; Core mixed-thread tests | Both grants permit Sandbox or Full according to Session flag |
| A6 | Extension `legacy grant does not authorize a Full Instance`; Popup partial/default-checkbox tests | Ordinary grant cannot bypass additional review |
| A7 | SDK/Extension integration with missing Native acknowledgment; SDK missing/false acknowledgment tests | Fail closed, abort setup, no Thread subscription; old Extension responses also fail in SDK tests |
| A8 | SDK/Extension integration with three actual SDK-created Sessions; Extension Port/channel/Origin tests | Same-Origin Instances keep independent requested modes |
| A9 | Core `mixed_threads_and_workspace_runs_keep_their_own_modes_on_resume` | Shared Workspace does not promote Sandbox, Workspace runs, or resumed modes |
| A10 | Extension revoke/restart/race tests; IPC `trusted_session_capability_is_required_and_revocation_cancels_only_its_origin`; Core revocation tests; runtime cancellation tests | Revocation invalidates pending/active authority only for that Origin |
| M1 | SDK `uploads private Session artifacts via runtimeModules`; Vite runtime-named fixtures; runtime logical-module execution fixtures | Prepare, upload, declarations, import maps and Thread execution |
| M2 | SDK equivalent `denoModules` cases; legacy Vite fixtures and type-test | Deprecated aliases remain operational |
| M3 | SDK `prepares and uploads Workspace modules via` both names; Core/real runtime Workspace-module tests | Explicit per-run map and separate Session snapshots |
| M4 | SDK conflicting fields tests for Session skills and Workspace options | `INVALID_INPUT`, including empty/undefined conflicting declarations |
| C1–C2 | `pedelec-deno` crate executable integration tests (both compiled binaries, file/stdin, Sandbox/Full fake IPC intents) | Both CLIs serialize the same target, mode authority, arguments and output |
| C3 | Executable integration rejection tests; Agent command allowlist tests | Unknown subcommands, flags, traversal and shell syntax rejected |

The new SDK/Extension suite runs the production SDK and Extension background together with linked Chrome Ports. Chrome storage, browser events and Native replies are simulated. It does not claim a live Chrome-to-Core result. Core/IPC/runtime suites separately exercise authoritative mode, trusted grants, actual child execution, and revocation. CLI tests run real compiled wrappers against a fixture IPC server.

## Security regression evidence

- `verify-deno-runtime.mjs` verifies the pinned engine's Sandbox read/write boundary, denied network/environment/process permissions and script-argument handling.
- Runtime `real_full_access_can_use_external_files_host_environment_subprocesses_sys_and_remote_modules` uses isolated temporary files outside its temporary Workspace, a loopback HTTP module server and a harmless child engine process. It checks host environment availability, managed `DENO_DIR`, `sys`, and FFI permission. No real private files are read.
- Runtime timeout/output-cap/revocation/cancel/shutdown tests, Core resume tests, and existing import-map validation remain in the runner. Runtime npm/authenticated-CLI ignored tests and live provider behavior are separate opt-ins.
- IPC tests reject page/native callers that self-assert Full Access. Only the trusted Extension capability can authorize it. Provider sandbox settings are independent.
- Cross-platform staging tests simulate Windows x64/arm64, macOS x64/arm64 and Linux x64, asserting both helper names. They cannot prove an installed release payload on those hosts.

## Live browser and installed release checks

Use a disposable browser profile, a temporary custom Workspace and a temporary sibling directory containing only test data. Start compatible Desktop and unpacked Extension builds. Serve the built Demo; it must connect and create ordinary Sandbox Sessions without requesting Full Access.

1. Clear grants. Create `new Pedelec()` and a normal Session. Verify the Popup shows Agent Sessions only (A1). Confirm the Demo's `workspace.run()` works and denied capabilities remain denied.
2. In a separate browser app fixture, create `new Pedelec({ runtimeFullAccess: true })`. Open the Extension Popup to review this Instance before calling `createSession`: SDK status preflight itself does not open the Popup. Verify both checkboxes appear and Full Access starts unchecked. Approve Agent Sessions only. Both Session flag values must fail; return to permission review and approve both (A3/A6).
3. On that Instance create one Session with `false` and one with `true`. In each Agent turn request `pedelec-ts-runtime --thread-id <id> run scripts/probe.ts`, then stdin `run -`. Repeat with `pedelec-deno`. Use only temporary sibling test files, a loopback HTTP server, a dedicated harmless environment marker and a harmless local tool. Sandbox denies outside access, environment, networking and subprocess; Full succeeds under OS permissions (A4/A5/C1/C2). Test FFI only with a purpose-built harmless fixture library.
4. Connect a standard Instance at the same Origin while the privileged one stays connected. It must remain Sandbox and cannot set its Session flag true (A2/A8). Use `workspace.createSession()` to place both modes in one Workspace; run `workspace.run()` and verify it stays Sandbox. End/resume each Thread and repeat (A9).
5. Revoke only Full Runtime Access during a long harmless Full loop. Verify cancellation and denial of the next high-privilege operation; ordinary Sessions and a separately approved Origin stay usable. Repeat with Native temporarily disconnected, reconnect and confirm queued revocation is applied (A10).
6. Test compatible SDK with an old Extension, then an old Desktop. Expect an authorization/protocol error and no usable high-privilege Session (A7). Do not interpret a generic connection error as a successful compatibility check.
7. On each release OS/architecture above, install the actual release, check both `pedelec-ts-runtime` and `pedelec-deno` are discoverable in Agent Host Context, execute both file/stdin probes, and confirm blocked subcommands/shell syntax (C3). Inspect the release bundle using the existing bundle verifier. Record artifact name, OS/architecture, versions and outcomes.

Live Chrome/Native, actual provider execution, installed packages on macOS/Linux and FFI library loading require these manual checks; simulated staging and permission-query tests are not substitutes.

## API examples

```ts
// Preferred authoring; Session requires Agent metadata.
const module = defineRuntimeModule({
  name: "project-tools", entry: "./agent/project-tools.ts",
  description: "Project utilities", usage: 'import { inspect } from "project-tools";',
});
await pedelec.createSession({ skills: { guidance: "Inspect project data", tools: [], runtimeModules: [module] } });
await workspace.run('import { inspect } from "project-tools"; console.log(await inspect());', { runtimeModules: [module] });

// Existing callers retain support; do not combine the two module fields.
const legacy = defineDenoModule({ name: "legacy-tools", entry: "./agent/project-tools.ts" });
await workspace.run('import "legacy-tools";', { denoModules: [legacy] });
```

Both languages' dev-docs and `sdk/README.md` have been updated with the SDK naming and authorization changes. Existing `deno/` document URLs, engine `Deno.*`, `DENO_DIR`, engine flags, error literals and internal `SerializableSkillsManifest.denoModules` remain stable. The Demo uses Runtime authoring and keeps Full Access disabled by default.

## Recorded run — 2026-10-22, Windows x64

`validate-runtime-evolution.mjs --real` used the installed pinned Deno 2.9.5 engine. The Sandbox smoke and actual Full Access external-file, environment, harmless-child, loopback-module, timeout, output-cap, cancellation and revocation fixtures passed.

| Check | Result |
| --- | --- |
| SDK | 198 tests passed (including 3 new SDK/Extension integration cases); type-test and build passed |
| Extension | 123 tests passed |
| Core / IPC | 255 / 144 tests passed |
| Runtime | 155 passed, 2 intentionally ignored (npm registry, authenticated Claude CLI) |
| Runtime CLI crate | 15 unit tests and 2 executable integration tests passed |
| Agent | 101 passed, 1 failed: `agent::server::registry::tests::inflight_lock_cleanup_does_not_split_mutual_exclusion` |
| Native Host / CLI / Shared | 21 / 16 / 13 tests passed |
| Release metadata and staging | 24 tests passed |
| dev-docs / Solid Demo | Both builds passed; Demo typecheck passed |

The Agent lock-cleanup assertion expected `(0, 0)` but observed `(1, 0)` at `desktop/crates/pedelec-agent/src/agent/server/registry.rs:386`; an isolated rerun also failed. This is outside Phase 5's docs/Demo/acceptance implementation and was not changed. The validation runner correctly reports failure and exits nonzero; integration acceptance is not fully green. An earlier combined Rust invocation also observed an IPC RPC-log assertion failure (`rpc_traffic_is_owner_scoped_and_omits_secrets`); its isolated rerun and subsequent full IPC run passed. No retry is hidden by the runner.

Live Chrome/Native, installed macOS/Linux/Windows release artifacts, actual FFI library calls and the two ignored external-service fixtures were not run. Follow the manual procedure above before claiming release acceptance. The optional source requirement file `.agent_temp/runtime-full-access-requirements.md` was absent; implementation followed the supplied Phase 5 plan and Phase 0 shared contract.
