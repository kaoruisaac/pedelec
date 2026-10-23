# Runtime process-tree lifecycle validation

Validated on 2026-10-23 for `.agent_temp/runtime-evolution-followup-2-process-tree-lifecycle-implementation.md`.

## Implementation

- `pedelec-runtime/src/deno_process_tree.rs` owns one container per invocation. Sandbox uses the same lifecycle without gaining permissions.
- Windows creates an unnamed, non-inherited Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Deno starts with `CREATE_NO_WINDOW | CREATE_SUSPENDED`, is assigned before script code can execute, and resumes through the documented ToolHelp/ResumeThread APIs. Setup failures kill/reap the suspended parent and close the job; there is no fallback to uncontained execution. Cleanup explicitly terminates the job and checks its active process count for at most one second. All handles use RAII.
- Linux/macOS establish an independent process group before exec. Polling uses `waitid(WNOWAIT)` so the leader remains waitable, retaining its PID/PGID until group termination. Cleanup sends SIGKILL to that group and reaps the immediate child; orphan descendants are reaped by the OS's orphan reaper. Desktop's group and other runs are never targeted.
- Thread cancellation, Core authorization revocation, IPC revocation, timeout, shutdown, and normal parent exit share container cleanup. Normal exit cleanup precedes output collection, including when descendants inherit stdout/stderr. Stdin writer completion also has a bounded grace.
- Cleanup failures survive repeated cancellation attempts in `processTreeCleanupError`. Cancellation/timeout retain their existing codes and include `captureError` if output collection fails. Normal-exit cleanup failure returns `DENO_PROCESS_IO_FAILED`. Shutdown returns its error list.

## Regression coverage

The shared fixture starts the actual managed Deno, which uses ordinary `Deno.Command()` calls to start a harmless child and grandchild. It records all three PIDs, inherits output pipes, and checks process liveness with OS APIs. Assertions have bounded waits and fixture cleanup guards.

- Runtime: explicit cancel, Core grant revocation, timeout, shutdown, and normal parent exit terminate the full fixture tree and release the registry entry.
- Runtime: cancelling one tree leaves another Full Access Thread and an unrelated Sandbox Workspace run active.
- IPC: `revoke_runtime_full_access` uses the real runtime dispatcher, terminates only its Origin's tree, leaves another Origin's tree running, rejects subsequent run/resume on the old Thread, then verifies shutdown of the remaining tree.
- Diagnostics: output capture and process-tree cleanup failures preserve cancellation/timeout codes and are explicitly surfaced.
- Existing single-process, stdin, permissions, cache/import, output cap, and timeout tests remain enabled. The production defaults remain 60 seconds and 1 MiB per output stream.

Run from `desktop/` on **each target OS** with its pinned managed Deno provisioned:

```text
cargo test -p pedelec-runtime -p pedelec-ipc -p pedelec-core --lib --no-fail-fast
```

Alternatively set `PEDELEC_TEST_DENO` to an actual Deno executable. Without an engine the real-engine fixtures print a skip reason; a green Rust test summary alone does not establish real-engine coverage. The timeout fixture uses a short injected execution timeout rather than waiting 60 seconds.

## Execution results

| Platform | Result |
| --- | --- |
| Windows x86_64 MSVC | Actual pinned managed Deno 2.9.5: Core 255 passed; final IPC 146 passed; final Runtime 158 passed, 2 existing ignored. Real process-tree and IPC fixtures executed. |
| Linux x86_64 | Process-container implementation and shared PID fixture passed isolated cross-target `cargo check`. Native managed-runtime execution remains pending. The full-crate cross-check was blocked by the missing `x86_64-linux-gnu-gcc` toolchain for aws-lc-sys; the available WSL distribution has no Cargo/native build toolchain. |
| macOS x86_64 | Process-container implementation and shared PID fixture passed isolated cross-target `cargo check`. Native managed-runtime execution remains pending; no macOS host is available in this session. |

The isolated checks compile the actual source files against `libc`/`serde_json` without replacing OS calls. They verify target-specific Rust types only, not linking, full-crate integration, or OS behavior. Actual native lifecycle regressions on Linux and macOS remain release gates.

Two pre-existing Runtime tests remain ignored: public npm access and the opt-in real 60-second timeout test. An existing Core `private_interfaces` warning about `ProviderCli` is unrelated to this change.

## Boundary and remaining validation

Full Access is **not an OS sandbox**. This cleanup covers normally inherited subprocesses; deliberate group/session escape, privileged processes, or daemons started through external services may escape it. Revocation cannot undo completed host operations. Unix does not claim to synchronously reap non-child processes, and a successful group signal is not proof that a malicious script left no other processes elsewhere.

The SDK API and Extension/Native proof protocol were not changed. English and Traditional Chinese runtime-security dev-docs were updated because the observable lifecycle and its limits changed.

Formal Chrome + Tauri installation GUI E2E remains separate and pending. Continue to use `runtime-native-grant-followup-validation.md` for that workflow.
