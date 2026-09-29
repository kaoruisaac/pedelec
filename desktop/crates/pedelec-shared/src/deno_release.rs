use crate::paths::managed_deno_executable_path;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MANIFEST_JSON: &str = include_str!("../../../deno-runtime-manifest.json");

const SUPPORTED_TARGETS: &[&str] = &[
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenoArtifact {
    pub version: String,
    pub target: String,
    pub platform: String,
    pub artifact: String,
    pub primary_url: String,
    pub fallback_url: String,
    pub archive_sha256: String,
    pub archive_size_bytes: u64,
    pub executable_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawManifest {
    version: String,
    notice_resource: String,
    executable_stem: String,
    artifacts: BTreeMap<String, RawArtifact>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawArtifact {
    platform: String,
    artifact: String,
    primary_url: String,
    fallback_url: String,
    archive_sha256: String,
    archive_size_bytes: u64,
}

pub fn parse_deno_release_manifest(json: &str) -> Result<Vec<DenoArtifact>, String> {
    let raw: RawManifest = serde_json::from_str(json)
        .map_err(|err| format!("Deno release manifest is not valid JSON: {err}"))?;
    validate_raw_manifest(&raw)?;
    raw.artifacts
        .into_iter()
        .map(|(target, artifact)| artifact_from_raw(&raw.version, &target, artifact))
        .collect()
}

pub fn pinned_deno_artifacts() -> &'static [DenoArtifact] {
    static ARTIFACTS: std::sync::OnceLock<Vec<DenoArtifact>> = std::sync::OnceLock::new();
    ARTIFACTS.get_or_init(|| {
        parse_deno_release_manifest(MANIFEST_JSON).expect("embedded Deno release manifest is valid")
    })
}

pub fn current_target_triple() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => Ok("aarch64-pc-windows-msvc"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        (os, arch) => Err(format!("unsupported Deno runtime target {os}/{arch}")),
    }
}

pub fn current_deno_artifact() -> Result<&'static DenoArtifact, String> {
    let target = current_target_triple()?;
    pinned_deno_artifacts()
        .iter()
        .find(|artifact| artifact.target == target)
        .ok_or_else(|| format!("pinned Deno manifest has no artifact for {target}"))
}

pub fn managed_runtime_executable_for_current(home: &Path) -> Result<PathBuf, String> {
    let artifact = current_deno_artifact()?;
    managed_deno_executable_path(
        home,
        &artifact.version,
        &artifact.target,
        &artifact.executable_name,
    )
    .map_err(|err| err.message)
}

fn validate_raw_manifest(raw: &RawManifest) -> Result<(), String> {
    if !is_pinned_semver(&raw.version) {
        return Err(format!(
            "Deno release version must be a pinned semver, got \"{}\".",
            raw.version
        ));
    }
    if raw.notice_resource != "third-party-notices/DENO-LICENSE.txt" {
        return Err(format!(
            "Unexpected Deno notice resource \"{}\".",
            raw.notice_resource
        ));
    }
    if raw.executable_stem != "deno" {
        return Err(format!(
            "Unexpected Deno executable stem \"{}\".",
            raw.executable_stem
        ));
    }
    if raw.artifacts.len() != SUPPORTED_TARGETS.len()
        || SUPPORTED_TARGETS
            .iter()
            .any(|target| !raw.artifacts.contains_key(*target))
    {
        return Err(format!(
            "Deno release manifest must contain exactly: {}.",
            SUPPORTED_TARGETS.join(", ")
        ));
    }
    for target in SUPPORTED_TARGETS {
        let artifact = &raw.artifacts[*target];
        let expected_platform = platform_for_target(target)?;
        if artifact.platform != expected_platform {
            return Err(format!(
                "Deno target \"{target}\" must declare platform \"{expected_platform}\"."
            ));
        }
        let expected_artifact = format!("deno-{target}.zip");
        if artifact.artifact != expected_artifact {
            return Err(format!(
                "Unexpected artifact name for {target}: \"{}\".",
                artifact.artifact
            ));
        }
        let expected_primary = expected_primary_url(&raw.version, &artifact.artifact);
        if artifact.primary_url != expected_primary {
            return Err(format!(
                "Deno artifact primary URL for {target} does not match the pinned runtime."
            ));
        }
        let expected_fallback = expected_fallback_url(&raw.version, &artifact.artifact);
        if artifact.fallback_url != expected_fallback {
            return Err(format!(
                "Deno artifact fallback URL for {target} does not match the pinned release."
            ));
        }
        if !is_sha256_hex(&artifact.archive_sha256) {
            return Err(format!(
                "Invalid expected Deno artifact SHA-256 for {target}."
            ));
        }
        if artifact.archive_size_bytes == 0 {
            return Err(format!(
                "Deno artifact size for {target} must be a positive integer."
            ));
        }
    }
    Ok(())
}

fn artifact_from_raw(
    version: &str,
    target: &str,
    artifact: RawArtifact,
) -> Result<DenoArtifact, String> {
    let executable_name = executable_name_for_platform(&artifact.platform)?;
    Ok(DenoArtifact {
        version: version.to_string(),
        target: target.to_string(),
        platform: artifact.platform,
        artifact: artifact.artifact,
        primary_url: artifact.primary_url,
        fallback_url: artifact.fallback_url,
        archive_sha256: artifact.archive_sha256,
        archive_size_bytes: artifact.archive_size_bytes,
        executable_name,
    })
}

fn expected_primary_url(version: &str, artifact: &str) -> String {
    format!("https://runtime.pedelec.cc/deno/v{version}/{artifact}")
}

fn expected_fallback_url(version: &str, artifact: &str) -> String {
    format!("https://github.com/denoland/deno/releases/download/v{version}/{artifact}")
}

fn platform_for_target(target: &str) -> Result<&'static str, String> {
    match target {
        "x86_64-pc-windows-msvc" | "aarch64-pc-windows-msvc" => Ok("win32"),
        "aarch64-apple-darwin" | "x86_64-apple-darwin" => Ok("darwin"),
        "x86_64-unknown-linux-gnu" => Ok("linux"),
        _ => Err(format!("unsupported Deno target \"{target}\"")),
    }
}

fn executable_name_for_platform(platform: &str) -> Result<String, String> {
    match platform {
        "win32" => Ok("deno.exe".to_string()),
        "darwin" | "linux" => Ok("deno".to_string()),
        _ => Err(format!(
            "unsupported Deno executable platform \"{platform}\""
        )),
    }
}

fn is_pinned_semver(version: &str) -> bool {
    if version == "latest" {
        return false;
    }
    let mut parts = version.split('.');
    let Some(major) = parts.next() else {
        return false;
    };
    let Some(minor) = parts.next() else {
        return false;
    };
    let Some(patch) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && !major.is_empty()
        && !minor.is_empty()
        && !patch.is_empty()
        && [major, minor, patch]
            .iter()
            .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_manifest_resolves_every_supported_target() {
        let artifacts = pinned_deno_artifacts();
        assert_eq!(artifacts.len(), SUPPORTED_TARGETS.len());
        for target in SUPPORTED_TARGETS {
            let artifact = artifacts
                .iter()
                .find(|artifact| artifact.target == *target)
                .unwrap();
            assert!(is_pinned_semver(&artifact.version));
            assert_ne!(artifact.version, "latest");
            assert!(artifact.archive_size_bytes > 0);
            assert!(is_sha256_hex(&artifact.archive_sha256));
            assert_eq!(
                artifact.primary_url,
                expected_primary_url(&artifact.version, &artifact.artifact)
            );
            assert_eq!(
                artifact.fallback_url,
                expected_fallback_url(&artifact.version, &artifact.artifact)
            );
            assert!(artifact
                .primary_url
                .contains(&format!("/v{}/", artifact.version)));
            assert!(artifact
                .fallback_url
                .contains(&format!("/v{}/", artifact.version)));
            assert!(artifact.primary_url.ends_with(&artifact.artifact));
            assert!(artifact.fallback_url.ends_with(&artifact.artifact));
            assert_ne!(artifact.primary_url, artifact.fallback_url);
            assert!(artifact.executable_name == "deno" || artifact.executable_name == "deno.exe");
            assert!(!artifact.executable_name.contains("pedelec"));
        }
    }

    #[test]
    fn current_host_artifact_matches_the_process_target() {
        let artifact = current_deno_artifact().unwrap();
        assert_eq!(artifact.target, current_target_triple().unwrap());
        let expected_platform = match std::env::consts::OS {
            "windows" => "win32",
            "macos" => "darwin",
            "linux" => "linux",
            other => panic!("unexpected os {other}"),
        };
        assert_eq!(artifact.platform, expected_platform);
    }

    #[test]
    fn rejects_unpinned_malformed_and_unknown_metadata() {
        let mut raw: serde_json::Value = serde_json::from_str(MANIFEST_JSON).unwrap();
        raw["version"] = serde_json::json!("latest");
        assert!(parse_deno_release_manifest(&raw.to_string())
            .unwrap_err()
            .contains("pinned semver"));

        let mut raw: serde_json::Value = serde_json::from_str(MANIFEST_JSON).unwrap();
        raw["artifacts"]["x86_64-pc-windows-msvc"]["archiveSha256"] = serde_json::json!("abcd");
        assert!(parse_deno_release_manifest(&raw.to_string())
            .unwrap_err()
            .contains("SHA-256"));

        let mut raw: serde_json::Value = serde_json::from_str(MANIFEST_JSON).unwrap();
        raw["artifacts"]["x86_64-unknown-linux-gnu"]["archiveSizeBytes"] = serde_json::json!(0);
        assert!(parse_deno_release_manifest(&raw.to_string())
            .unwrap_err()
            .contains("positive integer"));

        let mut raw: serde_json::Value = serde_json::from_str(MANIFEST_JSON).unwrap();
        raw["artifacts"]["x86_64-unknown-linux-gnu"]["platform"] = serde_json::json!("win32");
        assert!(parse_deno_release_manifest(&raw.to_string())
            .unwrap_err()
            .contains("platform"));

        let mut raw: serde_json::Value = serde_json::from_str(MANIFEST_JSON).unwrap();
        let removed = raw["artifacts"]
            .as_object_mut()
            .unwrap()
            .remove("aarch64-apple-darwin");
        assert!(removed.is_some());
        assert!(parse_deno_release_manifest(&raw.to_string())
            .unwrap_err()
            .contains("exactly"));
    }

    #[test]
    fn rejects_urls_that_do_not_match_the_pinned_identity() {
        let mut raw: serde_json::Value = serde_json::from_str(MANIFEST_JSON).unwrap();
        raw["artifacts"]["x86_64-pc-windows-msvc"]["primaryUrl"] = serde_json::json!(
            "https://example.invalid/deno/v2.9.5/deno-x86_64-pc-windows-msvc.zip"
        );
        assert!(parse_deno_release_manifest(&raw.to_string())
            .unwrap_err()
            .contains("primary URL"));

        let mut raw: serde_json::Value = serde_json::from_str(MANIFEST_JSON).unwrap();
        raw["artifacts"]["x86_64-pc-windows-msvc"]["primaryUrl"] = serde_json::json!(
            "https://runtime.pedelec.cc/deno/v9.9.9/deno-x86_64-pc-windows-msvc.zip"
        );
        assert!(parse_deno_release_manifest(&raw.to_string())
            .unwrap_err()
            .contains("primary URL"));

        let mut raw: serde_json::Value = serde_json::from_str(MANIFEST_JSON).unwrap();
        raw["artifacts"]["aarch64-apple-darwin"]["fallbackUrl"] = serde_json::json!(
            "https://runtime.pedelec.cc/deno/v2.9.5/deno-aarch64-apple-darwin.zip"
        );
        assert!(parse_deno_release_manifest(&raw.to_string())
            .unwrap_err()
            .contains("fallback URL"));

        let mut raw: serde_json::Value = serde_json::from_str(MANIFEST_JSON).unwrap();
        raw["artifacts"]["x86_64-unknown-linux-gnu"]["fallbackUrl"] = serde_json::json!(
            "https://github.com/denoland/deno/releases/download/v2.9.5/deno-other.zip"
        );
        assert!(parse_deno_release_manifest(&raw.to_string())
            .unwrap_err()
            .contains("fallback URL"));

        let mut raw: serde_json::Value = serde_json::from_str(MANIFEST_JSON).unwrap();
        raw["version"] = serde_json::json!("9.9.9");
        assert!(parse_deno_release_manifest(&raw.to_string())
            .unwrap_err()
            .contains("primary URL"));

        let mut raw: serde_json::Value = serde_json::from_str(MANIFEST_JSON).unwrap();
        raw["artifacts"]["x86_64-apple-darwin"]["url"] = serde_json::json!(
            "https://github.com/denoland/deno/releases/download/v2.9.5/deno-x86_64-apple-darwin.zip"
        );
        assert!(parse_deno_release_manifest(&raw.to_string())
            .unwrap_err()
            .contains("url"));
    }
}
