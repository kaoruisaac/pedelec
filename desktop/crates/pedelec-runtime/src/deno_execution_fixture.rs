//! Execution-level coverage for logical Deno Module imports.
//!
//! These tests dispatch through [`super::DenoRuntimeOwner`] and the production
//! `build_deno_command_args()` path. They use `PEDELEC_TEST_DENO` or a managed
//! runtime that has already been provisioned for the pinned Deno version.

use super::{
    build_deno_command_args, DenoRuntimeOwner, DenoRuntimePolicy, ManagedDenoRuntime,
    PreparedDenoExecution,
};
use pedelec_core::{
    workspace_deno_import_map_path, workspace_deno_modules_root,
    workspace_deno_workspace_modules_root, workspace_tmp_root, CoreRuntime,
    CreateDenoModuleUploadInput, CreateThreadDenoModuleInput, CreateThreadInput,
    CreateThreadSkillsInput, CreateWorkspaceDenoModuleUploadInput, DenoModuleUploadState,
    DenoRunInput, DenoRunTarget, EffortLevel, OpenWorkspaceInput, PrepareWorkspaceDenoModulesInput,
    ProviderCode, ThreadStatus, WorkspaceManager, WorkspaceRunInput,
};
use pedelec_shared::deno_release::managed_runtime_executable_for_current;
use pedelec_shared::paths::pedelec_home_dir;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const SPRITE_TOOLS_RUNTIME: &str = r#"
import { readFile, writeFile } from "node:fs/promises";

export const value = "sprite-tools-ok";

export async function roundTripWorkspaceFile(path, contents) {
  await writeFile(path, contents, "utf8");
  return await readFile(path, "utf8");
}

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
            "PEDELEC_TEST_DENO must point at the managed Deno executable, not pedelec-deno"
        );
        assert_ne!(
            path.file_name().and_then(|name| name.to_str()),
            Some("pedelec-deno.exe"),
            "PEDELEC_TEST_DENO must point at the managed Deno executable, not pedelec-deno"
        );
        return Some(path);
    }

    if let Ok(home) = pedelec_home_dir() {
        if let Ok(path) = managed_runtime_executable_for_current(&home) {
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

pub(super) fn test_deno_executable() -> Option<PathBuf> {
    match resolve_test_deno_executable() {
        Some(path) => Some(path),
        None => {
            eprintln!(
                "skipping real Deno execution fixture: set PEDELEC_TEST_DENO or provision the pinned managed runtime"
            );
            None
        }
    }
}

fn owner(executable: &Path, cache_dir: &Path) -> DenoRuntimeOwner {
    let owner = DenoRuntimeOwner::with_policy(DenoRuntimePolicy {
        execution_timeout: Duration::from_secs(30),
        stdout_cap_bytes: 64 * 1024,
        stderr_cap_bytes: 64 * 1024,
    });
    owner
        .configure(
            ManagedDenoRuntime::new(executable, cache_dir).expect("managed Deno cache directory"),
        )
        .expect("managed Deno runtime configuration");
    owner
}

fn assert_no_deno_owned_workspace_cache(workspace: &Path) {
    let deno_root = workspace.join(".pedelec-runtime").join("deno");
    if !deno_root.is_dir() {
        return;
    }
    for entry in fs::read_dir(&deno_root).unwrap() {
        let name = entry.unwrap().file_name();
        let name = name.to_string_lossy();
        assert!(
            name == "threads" || name == "workspace",
            "Deno-owned cache appeared in the workspace: {name}"
        );
    }
}

fn assert_injected_cache_was_used(cache_dir: &Path) {
    assert!(cache_dir.is_dir());
    assert!(
        fs::read_dir(cache_dir).unwrap().next().is_some(),
        "expected Deno to write cache files under {}",
        cache_dir.display()
    );
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
        "typesSource": "export declare const value: string;\nexport declare function roundTripWorkspaceFile(path: string, contents: string): Promise<string>;\nexport declare function loadWorkspaceModule(path: string): Promise<unknown>;\n",
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

fn materialize_workspace_scene_tools(
    runtime: &mut CoreRuntime,
    workspace: &Path,
    workspace_id: &str,
    origin: &str,
    value: &str,
) {
    let missing = runtime
        .prepare_workspace_deno_modules(
            PrepareWorkspaceDenoModulesInput {
                workspace_id: workspace_id.into(),
                module_names: vec!["scene-tools".into()],
            },
            origin,
        )
        .unwrap();
    assert_eq!(missing.missing_module_names, vec!["scene-tools"]);
    let bytes = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "format": "esm",
        "runtimeSource": format!("export const value = {};", serde_json::to_string(value).unwrap()),
        "typesSource": "export declare const value: string;",
    }))
    .unwrap();
    let ticket = runtime
        .create_workspace_deno_module_upload(
            CreateWorkspaceDenoModuleUploadInput {
                workspace_id: workspace_id.into(),
                module_name: "scene-tools".into(),
                expected_size_bytes: bytes.len() as u64,
            },
            origin,
        )
        .unwrap();
    runtime
        .deno_module_upload_tickets
        .get_mut(&ticket.upload_id)
        .unwrap()
        .state = DenoModuleUploadState::Uploading;
    let temporary_path =
        workspace_tmp_root(workspace).join(format!("{}-{}.json", ticket.upload_id, value));
    fs::create_dir_all(temporary_path.parent().unwrap()).unwrap();
    fs::write(&temporary_path, bytes).unwrap();
    let complete = runtime
        .complete_deno_module_upload(&ticket.upload_id, &temporary_path)
        .unwrap();
    assert!(complete.ready);
}

fn ready_module_thread_for_target(
    runtime: &mut CoreRuntime,
    workspace: &Path,
    target: DenoRunTarget,
    args: Vec<String>,
) -> (String, pedelec_core::DenoExecutionIntent) {
    let workspace_id = runtime
        .open_workspace(
            OpenWorkspaceInput {
                path: workspace.to_path_buf(),
            },
            "https://app.example.test",
            Some("0.3.3"),
        )
        .unwrap()
        .workspace_id;
    let thread_id = runtime
        .create_sdk_thread(
            CreateThreadInput {
                runtime_full_access: false,
                provider: ProviderCode::Codex,
                effort_level: Some(EffortLevel::Default),
                model: None,
                effort: None,
                skills: Some(CreateThreadSkillsInput {
                    guidance: String::new(),
                    tools: Vec::new(),
                    deno_modules: vec![CreateThreadDenoModuleInput {
                        name: "sprite-tools".into(),
                        description: "Sprite helpers".into(),
                        usage: "import { value } from \"sprite-tools\";".into(),
                        prefer_stdin_execution: false,
                    }],
                }),
                workspace_id: Some(workspace_id),
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
            target,
            args,
        })
        .unwrap();
    (thread_id, intent)
}

fn ready_module_thread(
    runtime: &mut CoreRuntime,
    workspace: &Path,
) -> (String, pedelec_core::DenoExecutionIntent) {
    ready_module_thread_for_target(
        runtime,
        workspace,
        DenoRunTarget::WorkspaceFile {
            entrypoint: "scripts/work.ts".into(),
        },
        Vec::new(),
    )
}

fn write_agent_script(workspace: &Path, body: &str) {
    let script = workspace.join("scripts").join("work.ts");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    fs::write(script, body).unwrap();
}

#[test]
fn real_deno_workspace_run_imports_core_uploaded_modules_and_cleans_per_run_maps() {
    let Some(executable) = test_deno_executable() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let mut core = runtime_for(&temp);
    let origin_a = "https://first.example.test";
    let origin_b = "https://second.example.test";
    let workspace_id = core
        .open_workspace(
            OpenWorkspaceInput {
                path: workspace.clone(),
            },
            origin_a,
            Some("0.4.17"),
        )
        .unwrap()
        .workspace_id;
    let second_id = core
        .open_workspace(
            OpenWorkspaceInput {
                path: workspace.clone(),
            },
            origin_b,
            Some("0.4.17"),
        )
        .unwrap()
        .workspace_id;
    assert_eq!(workspace_id, second_id);
    materialize_workspace_scene_tools(
        &mut core,
        &workspace,
        &workspace_id,
        origin_a,
        "origin-a-ok",
    );
    materialize_workspace_scene_tools(
        &mut core,
        &workspace,
        &workspace_id,
        origin_b,
        "origin-b-ok",
    );
    let deno = owner(&executable, &temp.path().join("managed-deno-cache"));

    let mut scope_roots = Vec::new();
    for (origin, expected) in [(origin_a, "origin-a-ok"), (origin_b, "origin-b-ok")] {
        let start = core
            .begin_workspace_run_for_origin(
                WorkspaceRunInput {
                    runtime_full_access: false,
                    workspace_id: workspace_id.clone(),
                    script: "import { value } from \"scene-tools\"; console.log(value);".into(),
                    timeout_ms: None,
                    deno_modules: vec!["scene-tools".into()],
                },
                origin,
            )
            .unwrap();
        let map = start
            .intent
            .import_map_path
            .clone()
            .expect("Core per-run import map");
        assert!(map.is_file());
        let scope_id = map
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        let module_file = workspace_deno_workspace_modules_root(&workspace, scope_id)
            .join("scene-tools/index.mjs");
        assert!(module_file.is_file());
        scope_roots.push(module_file.clone());
        let import_map = fs::read_to_string(&map).unwrap();
        assert!(import_map.contains("../../modules/scene-tools/index.mjs"));
        let prepared = PreparedDenoExecution::prepare(&start.intent).unwrap();
        let args = build_deno_command_args(&prepared);
        assert_eq!(
            args.iter()
                .filter(|arg| arg.to_string_lossy().starts_with("--import-map="))
                .count(),
            1
        );
        for flag in ["--no-remote", "--cached-only", "--no-npm", "--deny-net"] {
            assert!(args.iter().any(|arg| arg == flag), "missing {flag}");
        }
        let output = deno.dispatch(start.intent).unwrap();
        core.finish_workspace_run(&workspace_id, &start.run_id);
        assert_eq!(
            output.exit_code, 0,
            "stdout={} stderr={}",
            output.stdout, output.stderr
        );
        assert!(output.stdout.contains(expected), "{}", output.stdout);
        assert!(!map.exists(), "per-run import map must be removed");
        assert!(
            module_file.is_file(),
            "ready scope module must remain cached"
        );
    }
    assert_ne!(
        scope_roots[0], scope_roots[1],
        "origins must have separate module caches"
    );

    let no_module = core
        .begin_workspace_run_for_origin(
            WorkspaceRunInput {
                runtime_full_access: false,
                workspace_id: workspace_id.clone(),
                script: "console.log('workspace-no-modules-ok');".into(),
                timeout_ms: None,
                deno_modules: vec![],
            },
            origin_a,
        )
        .unwrap();
    assert!(no_module.intent.import_map_path.is_none());
    let prepared = PreparedDenoExecution::prepare(&no_module.intent).unwrap();
    assert!(build_deno_command_args(&prepared)
        .iter()
        .all(|arg| !arg.to_string_lossy().starts_with("--import-map=")));
    let output = deno.dispatch(no_module.intent).unwrap();
    core.finish_workspace_run(&workspace_id, &no_module.run_id);
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert!(output.stdout.contains("workspace-no-modules-ok"));

    let failed = core
        .begin_workspace_run_for_origin(
            WorkspaceRunInput {
                runtime_full_access: false,
                workspace_id: workspace_id.clone(),
                script: "import { value } from \"scene-tools\"; throw new Error(value);".into(),
                timeout_ms: None,
                deno_modules: vec!["scene-tools".into()],
            },
            origin_a,
        )
        .unwrap();
    let failed_map = failed.intent.import_map_path.clone().unwrap();
    let failed_output = deno.dispatch(failed.intent).unwrap();
    core.finish_workspace_run(&workspace_id, &failed.run_id);
    assert_ne!(failed_output.exit_code, 0);
    assert!(
        !failed_map.exists(),
        "failing run must also clean its temporary map"
    );
    assert!(scope_roots.iter().all(|path| path.is_file()));
    assert_no_deno_owned_workspace_cache(&workspace);
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
import { value, loadWorkspaceModule, roundTripWorkspaceFile } from "sprite-tools";
import { localValue } from "./helper.ts";

const { loaded } = await loadWorkspaceModule("agent-data.ts");
const nodeBuiltinValue = await roundTripWorkspaceFile("node-builtins.txt", "node-builtins-ok");
console.log(JSON.stringify({ value, localValue, loaded, nodeBuiltinValue }));
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
    assert!(args
        .iter()
        .any(|arg| arg.to_string_lossy().starts_with("--allow-read=")));
    assert!(args
        .iter()
        .any(|arg| arg.to_string_lossy().starts_with("--allow-write=")));
    for denied in [
        "--deny-net",
        "--deny-env",
        "--deny-run",
        "--deny-ffi",
        "--deny-sys",
    ] {
        assert!(args.iter().any(|arg| arg == denied));
    }

    let cache_dir = temp.path().join("managed-deno-cache");
    let output = owner(&executable, &cache_dir).dispatch(intent).unwrap();
    assert_eq!(
        output.exit_code, 0,
        "stdout={} stderr={}",
        output.stdout, output.stderr
    );
    assert_injected_cache_was_used(&cache_dir);
    assert!(!cache_dir.starts_with(&workspace));
    assert!(expected_import_map.is_file());
    assert!(workspace_deno_modules_root(&workspace, &thread_id)
        .join("sprite-tools")
        .join("index.mjs")
        .is_file());
    assert_no_deno_owned_workspace_cache(&workspace);
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
    assert!(
        output.stdout.contains("node-builtins-ok"),
        "{}",
        output.stdout
    );
    assert_eq!(
        fs::read_to_string(workspace.join("node-builtins.txt")).unwrap(),
        "node-builtins-ok"
    );
}

#[test]
fn real_deno_executes_stdin_source_with_logical_and_relative_imports() {
    let Some(executable) = test_deno_executable() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(
        workspace.join("helper.ts"),
        "export const localValue = \"stdin-relative-ok\";\n",
    )
    .unwrap();
    let source = r#"
import { value } from "sprite-tools";
import { localValue } from "./helper.ts";
console.log(JSON.stringify({ value, localValue, arg: Deno.args[0] }));
"#;

    let mut runtime = runtime_for(&temp);
    let (_thread_id, intent) = ready_module_thread_for_target(
        &mut runtime,
        &workspace,
        DenoRunTarget::StdinSource {
            source: source.into(),
        },
        vec!["stdin-arg-ok".into()],
    );
    let prepared = PreparedDenoExecution::prepare(&intent).unwrap();
    let args = build_deno_command_args(&prepared);
    let separator = args.iter().position(|arg| arg == "--").unwrap();
    assert_eq!(args[separator + 1], "-");
    assert!(args
        .iter()
        .any(|arg| arg.to_string_lossy().starts_with("--import-map=")));

    let cache_dir = temp.path().join("managed-deno-cache");
    let output = owner(&executable, &cache_dir).dispatch(intent).unwrap();
    assert_eq!(
        output.exit_code, 0,
        "stdout={} stderr={}",
        output.stdout, output.stderr
    );
    assert_injected_cache_was_used(&cache_dir);
    assert!(!cache_dir.starts_with(&workspace));
    assert_no_deno_owned_workspace_cache(&workspace);
    assert!(
        output.stdout.contains("sprite-tools-ok"),
        "{}",
        output.stdout
    );
    assert!(
        output.stdout.contains("stdin-relative-ok"),
        "{}",
        output.stdout
    );
    assert!(output.stdout.contains("stdin-arg-ok"), "{}", output.stdout);
    assert!(!workspace.join("scripts/work.ts").exists());
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
    let cache_dir = temp.path().join("managed-deno-cache");
    let output = owner(&executable, &cache_dir).dispatch(intent).unwrap();
    assert_no_deno_owned_workspace_cache(&workspace);
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
    let cache_dir = temp.path().join("managed-deno-cache");
    let output = owner(&executable, &cache_dir).dispatch(intent).unwrap();
    assert_no_deno_owned_workspace_cache(&workspace);
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
