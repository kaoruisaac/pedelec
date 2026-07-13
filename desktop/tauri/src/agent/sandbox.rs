use super::error::AgentError;
use std::fs;
use std::path::{Component, Path, PathBuf};

const IGNORED_DIRS: [&str; 5] = [".git", "node_modules", "target", "dist", "build"];

#[derive(Debug, Clone)]
pub struct Sandbox {
    root: PathBuf,
    max_file_bytes: u64,
    max_image_bytes: u64,
    max_list_files: usize,
}

impl Sandbox {
    pub fn new(
        path: &Path,
        max_file_bytes: u64,
        max_image_bytes: u64,
        max_list_files: usize,
    ) -> Result<Self, AgentError> {
        let root = path.canonicalize().map_err(|err| {
            AgentError::with_details(
                "SANDBOX_PATH_REQUIRED",
                "Sandbox path does not exist or cannot be read",
                serde_json::json!({ "path": path, "error": err.to_string() }),
            )
        })?;
        if !root.is_dir() {
            return Err(AgentError::with_details(
                "SANDBOX_PATH_REQUIRED",
                "Sandbox path must be a directory",
                serde_json::json!({ "path": root }),
            ));
        }
        Ok(Self {
            root,
            max_file_bytes,
            max_image_bytes,
            max_list_files,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn list_text_files(&self, dir: &str, max_depth: usize) -> Result<Vec<String>, AgentError> {
        let start = self.resolve_relative_dir(dir)?;
        let mut files = Vec::new();
        self.walk(&start, 0, max_depth, &mut files)?;
        files.sort();
        Ok(files)
    }

    pub fn list_image_files(
        &self,
        dir: &str,
        max_depth: usize,
    ) -> Result<(Vec<ImageInfo>, bool), AgentError> {
        let start = self.resolve_relative_dir(dir)?;
        let mut files = Vec::new();
        self.walk_images(&start, 0, max_depth, &mut files)?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let truncated = files.len() >= self.max_list_files;
        Ok((files, truncated))
    }

    pub fn read_image(&self, path: &str) -> Result<(ImageInfo, Vec<u8>), AgentError> {
        let resolved = self.resolve_relative_file(path)?;
        let meta = fs::symlink_metadata(&resolved).map_err(|_| {
            AgentError::with_details(
                "FILE_NOT_FOUND",
                "File not found",
                serde_json::json!({"path":path}),
            )
        })?;
        if meta.file_type().is_symlink() {
            return Err(AgentError::with_details(
                "SANDBOX_ESCAPE_DENIED",
                "Symlinks are not allowed",
                serde_json::json!({"path":path}),
            ));
        }
        if !meta.is_file() {
            return Err(AgentError::with_details(
                "FILE_NOT_FOUND",
                "Path is not a file",
                serde_json::json!({"path":path}),
            ));
        }
        let media_type = image_media_type(&resolved).ok_or_else(|| {
            AgentError::with_details(
                "IMAGE_FORMAT_UNSUPPORTED",
                "Unsupported image format",
                serde_json::json!({"path":path}),
            )
        })?;
        if meta.len() > self.max_image_bytes {
            return Err(AgentError::with_details(
                "IMAGE_TOO_LARGE",
                "Image exceeds size limit",
                serde_json::json!({"path":path,"sizeBytes":meta.len(),"maxBytes":self.max_image_bytes}),
            ));
        }
        let bytes = fs::read(&resolved).map_err(|e| {
            AgentError::with_details(
                "READ_FILE_FAILED",
                "Failed to read image",
                serde_json::json!({"path":path,"error":e.to_string()}),
            )
        })?;
        Ok((
            ImageInfo {
                path: path.replace('\\', "/"),
                size_bytes: meta.len(),
                media_type: media_type.to_string(),
            },
            bytes,
        ))
    }

    pub fn read_text_file(&self, path: &str) -> Result<(String, bool), AgentError> {
        let resolved = self.resolve_relative_file(path)?;
        if !resolved.exists() {
            return Err(AgentError::with_details(
                "FILE_NOT_FOUND",
                format!("File not found: {path}"),
                serde_json::json!({ "path": path }),
            ));
        }
        let metadata = fs::metadata(&resolved).map_err(|err| {
            AgentError::with_details(
                "READ_FILE_FAILED",
                "Failed to read file metadata",
                serde_json::json!({ "path": path, "error": err.to_string() }),
            )
        })?;
        if !metadata.is_file() {
            return Err(AgentError::with_details(
                "FILE_NOT_FOUND",
                "Path is not a file",
                serde_json::json!({ "path": path }),
            ));
        }
        let bytes = fs::read(&resolved).map_err(|err| {
            AgentError::with_details(
                "READ_FILE_FAILED",
                "Failed to read file",
                serde_json::json!({ "path": path, "error": err.to_string() }),
            )
        })?;
        let truncated = bytes.len() as u64 > self.max_file_bytes;
        let slice_len = if truncated {
            self.max_file_bytes as usize
        } else {
            bytes.len()
        };
        let text = std::str::from_utf8(&bytes[..slice_len])
            .map_err(|_| {
                AgentError::with_details(
                    "FILE_NOT_TEXT",
                    "File is not valid UTF-8 text",
                    serde_json::json!({ "path": path }),
                )
            })?
            .to_string();
        Ok((text, truncated))
    }

    fn walk(
        &self,
        dir: &Path,
        depth: usize,
        max_depth: usize,
        files: &mut Vec<String>,
    ) -> Result<(), AgentError> {
        if files.len() >= self.max_list_files || depth > max_depth {
            return Ok(());
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(()),
        };
        for entry in entries.flatten() {
            if files.len() >= self.max_list_files {
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                if !IGNORED_DIRS.contains(&name.as_str()) {
                    self.walk(&path, depth + 1, max_depth, files)?;
                }
                continue;
            }
            if metadata.is_file() && self.is_text_file(&path) {
                if let Ok(relative) = path.strip_prefix(&self.root) {
                    files.push(relative.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        Ok(())
    }

    fn walk_images(
        &self,
        dir: &Path,
        depth: usize,
        max_depth: usize,
        files: &mut Vec<ImageInfo>,
    ) -> Result<(), AgentError> {
        if files.len() >= self.max_list_files || depth > max_depth {
            return Ok(());
        }
        let entries = match fs::read_dir(dir) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        for entry in entries.flatten() {
            if files.len() >= self.max_list_files {
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = match fs::symlink_metadata(&path) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if !IGNORED_DIRS.contains(&name.as_str()) {
                    self.walk_images(&path, depth + 1, max_depth, files)?;
                }
            } else if meta.is_file() {
                if let Some(media_type) = image_media_type(&path) {
                    if let Ok(relative) = path.strip_prefix(&self.root) {
                        files.push(ImageInfo {
                            path: relative.to_string_lossy().replace('\\', "/"),
                            size_bytes: meta.len(),
                            media_type: media_type.to_string(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn is_text_file(&self, path: &Path) -> bool {
        let Ok(bytes) = fs::read(path) else {
            return false;
        };
        if bytes.contains(&0) {
            return false;
        }
        std::str::from_utf8(&bytes).is_ok()
    }

    fn resolve_relative_dir(&self, input: &str) -> Result<PathBuf, AgentError> {
        let path = self.resolve_relative_path(input)?;
        if !path.is_dir() {
            return Err(AgentError::with_details(
                "FILE_NOT_FOUND",
                "Directory not found",
                serde_json::json!({ "path": input }),
            ));
        }
        Ok(path)
    }

    fn resolve_relative_file(&self, input: &str) -> Result<PathBuf, AgentError> {
        self.resolve_relative_path(input)
    }

    fn resolve_relative_path(&self, input: &str) -> Result<PathBuf, AgentError> {
        let relative = Path::new(input);
        if input.trim().is_empty()
            || relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(AgentError::with_details(
                "SANDBOX_ESCAPE_DENIED",
                "Path must be relative and stay inside sandbox",
                serde_json::json!({ "path": input }),
            ));
        }
        let joined = self.root.join(relative);
        let mut current = self.root.clone();
        for component in relative.components() {
            if let Component::Normal(part) = component {
                current.push(part);
                if current.exists()
                    && fs::symlink_metadata(&current)
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(false)
                {
                    return Err(AgentError::with_details(
                        "SANDBOX_ESCAPE_DENIED",
                        "Symlinks are not allowed",
                        serde_json::json!({"path":input}),
                    ));
                }
            }
        }
        let canonical = if joined.exists() {
            joined.canonicalize().map_err(|err| {
                AgentError::with_details(
                    "READ_FILE_FAILED",
                    "Failed to resolve path",
                    serde_json::json!({ "path": input, "error": err.to_string() }),
                )
            })?
        } else {
            joined
        };
        if !canonical.starts_with(&self.root) {
            return Err(AgentError::with_details(
                "SANDBOX_ESCAPE_DENIED",
                "Path escapes sandbox",
                serde_json::json!({ "path": input }),
            ));
        }
        Ok(canonical)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageInfo {
    pub path: String,
    pub size_bytes: u64,
    pub media_type: String,
}

fn image_media_type(path: &Path) -> Option<&'static str> {
    match path
        .extension()?
        .to_string_lossy()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_dir_escape() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::new(temp.path(), 1024, 20 * 1024 * 1024, 20).unwrap();

        let err = sandbox.read_text_file("../x").unwrap_err();

        assert_eq!(err.code, "SANDBOX_ESCAPE_DENIED");
    }

    #[test]
    fn reads_and_truncates_utf8_file() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.txt"), "abcdef").unwrap();
        let sandbox = Sandbox::new(temp.path(), 3, 20 * 1024 * 1024, 20).unwrap();

        let (text, truncated) = sandbox.read_text_file("a.txt").unwrap();

        assert_eq!(text, "abc");
        assert!(truncated);
    }

    #[test]
    fn lists_text_files_and_skips_ignored_dirs() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.txt"), "hello").unwrap();
        fs::create_dir_all(temp.path().join("target")).unwrap();
        fs::write(temp.path().join("target").join("b.txt"), "skip").unwrap();
        let sandbox = Sandbox::new(temp.path(), 1024, 20 * 1024 * 1024, 20).unwrap();

        let files = sandbox.list_text_files(".", 3).unwrap();

        assert_eq!(files, vec!["a.txt"]);
    }
}
