//! Execution-level coverage for logical Deno Module imports.
//!
//! These tests dispatch through [`super::DenoRuntimeOwner`] and the production
//! `build_deno_command_args()` path. They require the staged raw Deno resource
//! (`desktop/tauri/binaries/{deno,deno.exe}`) or `PEDELEC_TEST_DENO`.

use super::{build_deno_command_args, DenoRuntimeOwner, DenoRuntimePolicy, PreparedDenoExecution};
use pedelec_core::{
    workspace_deno_import_map_path, workspace_deno_modules_root, workspace_tmp_root, CoreRuntime,
    CreateDenoModuleUploadInput, CreateThreadDenoModuleInput, CreateThreadInput,
    CreateThreadSkillsInput, CreateThreadWorkspaceInput, DenoModuleUploadState, DenoRunInput,
    EffortLevel, ProviderCode, ThreadStatus, WorkspaceManager,
};
use pedelec_shared::paths::bundled_deno_binary_name;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SPRITE_TOOLS_RUNTIME: &str = r#"
export const value = "sprite-tools-ok";

export async function loadWorkspaceModule(path) {
  const relative = String(path).replaceAll("\\", "/").replace(/^\/+/, "");
  const root = Deno.cwd().replaceAll("\\", "/");
  const combined = root.endsWith("/") ? `${root}${relative}` : `${root}/${relative}`;
  const href = combined.startsWith("/") ? `file://${combined}` : `file:///${combined}`;
  return await import(href);
}
"#;

fn resolve_test_deno_executable() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PEDELEC_TEST_DENO") {
        let path = PathBuf::from(explicit);
        assert!(
            path.is_file(),
            "PEDELEC_TEST_DENO is set but is not a file: {}",
            path.display()
        );
        assert_ne!(
            path.file_name().and_then(|name| name.to_str()),
            Some("pedelec-deno"),
            "PEDELEC_TEST_DENO must point at the raw Deno resource, not pedelec-deno"
        );
        return Some(path);
    }

    let binary_name = bundled_deno_binary_name();
    let mut current = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let candidate = current.join("tauri").join("binaries").join(binary_name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn test_deno_executable() -> Option<PathBuf> {
    match resolve_test_deno_executable() {
        Some(path) => Some(path),
        None => {
            eprintln!(
                "skipping real Deno execution fixture: stage desktop/tauri/binaries/{} or set PEDELEC_TEST_DENO",
                bundled_deno_binary_name()
            );
            None
        }
    }
}

fn owner(executable: &Path) -> DenoRuntimeOwner {
    DenoRuntimeOwner::with_policy(
        executable,
        DenoRuntimePolicy {
            execution_timeout: Duration::from_secs(30),
            stdout_cap_bytes: 64 * 1024,
            stderr_cap_bytes: 64 * 1024,
        },
    )
}

fn runtime_for(temp: &tempfile::TempDir) -> CoreRuntime {
    CoreRuntime {
        workspace_manager: WorkspaceManager::with_workspace_root(temp.path().join("managed")),
        settings_file_path: Some(temp.path().join("settings.json")),
        asset_upload_port: Some(43124),
        ..CoreRuntime::default()
    }
}

fn materialize_sprite_tools(runtime: &mut CoreRuntime, workspace: &Path, thread_id: &str) {
    let envelope = serde_json::json!({
        "version": 1,
        "format": "esm",
        "runtimeSource": SPRITE_TOOLS_RUNTIME,
        "typesSource": "export declare const value: string;\nexport declare function loadWorkspaceModule(path: string): Promise<unknown>;\n",
    });
    let bytes = serde_json::to_vec(&envelope).unwrap();
    let ticket = runtime
        .create_deno_module_upload(CreateDenoModuleUploadInput {
            thread_id: thread_id.to_string(),
            module_name: "sprite-tools".into(),
            expected_size_bytes: bytes.len() as u64,
        })
        .unwrap();
    runtime
        .deno_module_upload_tickets
        .get_mut(&ticket.upload_id)
        .unwrap()
        .state = DenoModuleUploadState::Uploading;
    let temporary_path = workspace_tmp_root(workspace).join(format!("{thread_id}-envelope.json"));
    fs::create_dir_all(temporary_path.parent().unwrap()).unwrap();
    fs::write(&temporary_path, bytes).unwrap();
    runtime
        .complete_deno_module_upload(&ticket.upload_id, &temporary_path)
        .unwrap();
}

fn ready_module_thread(
    runtime: &mut CoreRuntime,
    workspace: &Path,
) -> (String, pedelec_core::DenoExecutionIntent) {
    let thread_id = runtime
        .create_sdk_thread(
            CreateThreadInput {
                provider: ProviderCode::Codex,
                effort_level: Some(EffortLevel::Default),
                skills: Some(CreateThreadSkillsInput {
                    guidance: String::new(),
                    tools: Vec::new(),
                    deno_modules: vec![CreateThreadDenoModuleInput {
                        name: "sprite-tools".into(),
                        description: "Sprite helpers".into(),
                        usage: "import { value } from \"sprite-tools\";".into(),
                    }],
                }),
                workspace: Some(CreateThreadWorkspaceInput {
                    path: workspace.to_path_buf(),
                }),
            },
            "https://app.example.test",
            Some("0.3.3"),
        )
        .unwrap()
        .thread_id;
    materialize_sprite_tools(runtime, workspace, &thread_id);
    runtime
        .thread_manager
        .thread_mut(&thread_id)
        .unwrap()
        .status = ThreadStatus::Running;
    runtime
        .thread_manager
        .provider_state_mut(&thread_id)
        .unwrap()
        .active_provider_turn_id = Some("turn-deno-fixture".into());
    let intent = runtime
        .prepare_deno_run_intent(DenoRunInput {
            thread_id: thread_id.clone(),
            entrypoint: "scripts/work.ts".into(),
            args: Vec::new(),
        })
        .unwrap();
    (thread_id, intent)
}

fn write_agent_script(workspace: &Path, body: &str) {
    let script = workspace.join("scripts").join("work.ts");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    fs::write(script, body).unwrap();
}

#[test]
fn real_deno_imports_logical_modules_relative_files_and_workspace_dynamic_imports() {
    let Some(executable) = test_deno_executable() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(workspace.join("scripts")).unwrap();
    fs::write(
        workspace.join("scripts").join("helper.ts"),
        "export const localValue = \"relative-ok\";\n",
    )
    .unwrap();
    fs::write(
        workspace.join("agent-data.ts"),
        "export const loaded = \"workspace-dynamic-ok\";\n",
    )
    .unwrap();
    write_agent_script(
        &workspace,
        r#"
import { value, loadWorkspaceModule } from "sprite-tools";
import { localValue } from "./helper.ts";

const { loaded } = await loadWorkspaceModule("agent-data.ts");
console.log(JSON.stringify({ value, localValue, loaded }));
"#,
    );

    let mut runtime = runtime_for(&temp);
    let (thread_id, intent) = ready_module_thread(&mut runtime, &workspace);
    let expected_import_map = PathBuf::from(pedelec_shared::paths::path_for_external_use(
        &workspace_deno_import_map_path(&workspace, &thread_id)
            .canonicalize()
            .unwrap(),
    ));
    assert_eq!(
        intent.import_map_path.as_deref(),
        Some(expected_import_map.as_path())
    );
    assert!(workspace_deno_modules_root(&workspace, &thread_id)
        .join("sprite-tools")
        .join("index.mjs")
        .is_file());

    let prepared = PreparedDenoExecution::prepare(&intent).unwrap();
    let args = build_deno_command_args(&prepared);
    assert!(args
        .iter()
        .any(|arg| arg.to_string_lossy().starts_with("--import-map=")));
    assert!(args.iter().any(|arg| arg == "--no-remote"));
    assert!(args.iter().any(|arg| arg == "--no-npm"));
    assert!(args.iter().any(|arg| arg == "--cached-only"));

    let output = owner(&executable).dispatch(intent).unwrap();
    assert_eq!(
        output.exit_code, 0,
        "stdout={} stderr={}",
        output.stdout, output.stderr
    );
    assert!(
        output.stdout.contains("sprite-tools-ok"),
        "{}",
        output.stdout
    );
    assert!(output.stdout.contains("relative-ok"), "{}", output.stdout);
    assert!(
        output.stdout.contains("workspace-dynamic-ok"),
        "{}",
        output.stdout
    );
}

#[test]
fn real_deno_rejects_remote_https_module_resolution() {
    let Some(executable) = test_deno_executable() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write_agent_script(
        &workspace,
        r#"
import "https://example.com/mod.ts";
console.log("unreachable");
"#,
    );

    let mut runtime = runtime_for(&temp);
    let (_thread_id, intent) = ready_module_thread(&mut runtime, &workspace);
    let output = owner(&executable).dispatch(intent).unwrap();
    assert_ne!(output.exit_code, 0);
    let combined = format!("{}\n{}", output.stdout, output.stderr);
    assert!(
        combined.contains("no-remote")
            || combined.contains("https://example.com")
            || combined.contains("remote"),
        "expected a remote-import policy failure, got {combined}"
    );
    assert!(!combined.contains("unreachable"));
}

#[test]
fn real_deno_rejects_npm_package_resolution() {
    let Some(executable) = test_deno_executable() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    write_agent_script(
        &workspace,
        r#"
import "npm:left-pad";
console.log("unreachable");
"#,
    );

    let mut runtime = runtime_for(&temp);
    let (_thread_id, intent) = ready_module_thread(&mut runtime, &workspace);
    let output = owner(&executable).dispatch(intent).unwrap();
    assert_ne!(output.exit_code, 0);
    let combined = format!("{}\n{}", output.stdout, output.stderr);
    assert!(
        combined.contains("no-npm")
            || combined.contains("npm:")
            || combined.contains("npm specifiers"),
        "expected an npm-resolution policy failure, got {combined}"
    );
    assert!(!combined.contains("unreachable"));
}
