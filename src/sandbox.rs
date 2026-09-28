use crate::error::{FoxProError, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Sandbox {
    workspace: PathBuf,
}

impl Sandbox {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    pub fn canonicalize(path: &Path) -> Result<PathBuf> {
        dunce::canonicalize(path).map_err(|_| FoxProError::PathNotFound(path.to_path_buf()))
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Ensure `path` resolves to a location inside the workspace.
    /// The path must already exist.
    #[allow(dead_code)]
    pub fn validate(&self, path: &Path) -> Result<PathBuf> {
        let combined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        };
        let requested = Self::canonicalize(&combined)?;
        if !requested.starts_with(&self.workspace) {
            return Err(FoxProError::SandboxViolation {
                workspace: self.workspace.clone(),
                requested,
            });
        }
        Ok(requested)
    }

    /// Resolve a path against the workspace, even if it does not exist yet.
    /// Validates that the deepest existing ancestor is inside the sandbox.
    pub fn resolve(&self, path: &Path) -> Result<PathBuf> {
        let combined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        };

        let mut existing = combined.clone();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();

        loop {
            if existing.exists() {
                let base = Self::canonicalize(&existing)?;
                if !base.starts_with(&self.workspace) {
                    return Err(FoxProError::SandboxViolation {
                        workspace: self.workspace.clone(),
                        requested: base,
                    });
                }
                let mut result = base;
                for part in tail.into_iter().rev() {
                    result = result.join(part);
                }
                return Ok(result);
            }

            match existing.parent() {
                Some(parent) => {
                    if let Some(name) = existing.file_name() {
                        tail.push(name.to_os_string());
                    }
                    existing = parent.to_path_buf();
                }
                None => return Err(FoxProError::PathNotFound(combined)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn allows_paths_inside_workspace() {
        let dir = TempDir::new().unwrap();
        let sandbox = Sandbox::new(Sandbox::canonicalize(dir.path()).unwrap());
        let file = dir.path().join("sub").join("file.txt");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::File::create(&file).unwrap();

        assert!(sandbox.validate(Path::new("sub/file.txt")).is_ok());
        assert!(sandbox.validate(&file).is_ok());
    }

    #[test]
    fn rejects_paths_outside_workspace() {
        let dir = TempDir::new().unwrap();
        let sandbox = Sandbox::new(Sandbox::canonicalize(dir.path()).unwrap());
        let outside = dir.path().parent().unwrap().join("foxpro-mcp-outside-test.txt");
        fs::File::create(&outside).unwrap();

        let err = sandbox.validate(&outside).unwrap_err();
        fs::remove_file(&outside).ok();
        assert!(matches!(err, FoxProError::SandboxViolation { .. }));
    }

    #[test]
    fn rejects_traversal_attempts() {
        let dir = TempDir::new().unwrap();
        let sandbox = Sandbox::new(Sandbox::canonicalize(dir.path()).unwrap());
        let outside = dir.path().parent().unwrap().join("foxpro-mcp-traversal-test.txt");
        fs::File::create(&outside).unwrap();

        let err = sandbox.validate(Path::new("../foxpro-mcp-traversal-test.txt")).unwrap_err();
        fs::remove_file(&outside).ok();
        assert!(matches!(err, FoxProError::SandboxViolation { .. }));
    }
}
