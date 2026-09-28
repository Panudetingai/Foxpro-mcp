use crate::error::{FoxProError, Result};
use std::fs;
use std::path::{Component, Path, PathBuf};

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

    /// Join `path` onto the workspace (when relative) and resolve `.`/`..`
    /// lexically, so later checks never see traversal components.
    fn absolutize(&self, path: &Path) -> Result<PathBuf> {
        if path.as_os_str().is_empty() {
            return Err(FoxProError::InvalidArgument(
                "path must not be empty".into(),
            ));
        }
        let combined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        };

        let mut out = PathBuf::new();
        for component in combined.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => {
                    if out.parent().is_none() {
                        return Err(self.violation(combined.clone()));
                    }
                    out.pop();
                }
                Component::Normal(part) => out.push(part),
            }
        }
        Ok(out)
    }

    fn violation(&self, requested: PathBuf) -> FoxProError {
        FoxProError::SandboxViolation {
            workspace: self.workspace.clone(),
            requested,
        }
    }

    fn ensure_inside(&self, canonical: PathBuf) -> Result<PathBuf> {
        if canonical.starts_with(&self.workspace) {
            Ok(canonical)
        } else {
            Err(self.violation(canonical))
        }
    }

    /// Ensure `path` resolves (following symlinks/junctions) to a location
    /// inside the workspace. The path must already exist.
    pub fn validate(&self, path: &Path) -> Result<PathBuf> {
        let normalized = self.absolutize(path)?;
        match dunce::canonicalize(&normalized) {
            Ok(canonical) => self.ensure_inside(canonical),
            // Report a missing file outside the workspace as a violation, so
            // errors never reveal whether files outside the sandbox exist.
            Err(_) if self.lexically_inside(&normalized) => {
                Err(FoxProError::PathNotFound(normalized))
            }
            Err(_) => Err(self.violation(normalized)),
        }
    }

    fn lexically_inside(&self, path: &Path) -> bool {
        let mut inner = path.components();
        self.workspace.components().all(|w| {
            inner.next().is_some_and(|p| {
                if cfg!(windows) {
                    p.as_os_str().eq_ignore_ascii_case(w.as_os_str())
                } else {
                    p == w
                }
            })
        })
    }

    /// Resolve a path against the workspace, even if it does not exist yet.
    ///
    /// The deepest existing ancestor is canonicalized (following links) and
    /// must be inside the workspace. Dangling symlinks are rejected because a
    /// later write would follow them outside the sandbox.
    pub fn resolve(&self, path: &Path) -> Result<PathBuf> {
        let normalized = self.absolutize(path)?;

        let mut existing = normalized.clone();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        loop {
            if fs::symlink_metadata(&existing).is_ok() {
                break;
            }
            match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    tail.push(name.to_os_string());
                    existing = parent.to_path_buf();
                }
                _ => return Err(FoxProError::PathNotFound(normalized)),
            }
        }

        let base = dunce::canonicalize(&existing).map_err(|_| self.violation(existing.clone()))?;
        let mut result = self.ensure_inside(base)?;
        for part in tail.into_iter().rev() {
            result.push(part);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn sandbox(dir: &TempDir) -> Sandbox {
        let ws = dir.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        Sandbox::new(Sandbox::canonicalize(&ws).unwrap())
    }

    #[test]
    fn allows_paths_inside_workspace() {
        let dir = TempDir::new().unwrap();
        let sandbox = sandbox(&dir);
        let file = sandbox.workspace().join("sub").join("file.txt");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::File::create(&file).unwrap();

        assert!(sandbox.validate(Path::new("sub/file.txt")).is_ok());
        assert!(sandbox.validate(&file).is_ok());
        assert!(sandbox.validate(Path::new("./sub/../sub/file.txt")).is_ok());
    }

    #[test]
    fn rejects_paths_outside_workspace() {
        let dir = TempDir::new().unwrap();
        let sandbox = sandbox(&dir);
        let outside = dir.path().join("outside.txt");
        fs::File::create(&outside).unwrap();

        let err = sandbox.validate(&outside).unwrap_err();
        assert!(matches!(err, FoxProError::SandboxViolation { .. }));
    }

    #[test]
    fn rejects_traversal_attempts() {
        let dir = TempDir::new().unwrap();
        let sandbox = sandbox(&dir);
        fs::File::create(dir.path().join("outside.txt")).unwrap();

        let err = sandbox.validate(Path::new("../outside.txt")).unwrap_err();
        assert!(matches!(err, FoxProError::SandboxViolation { .. }));
        // Missing files outside the sandbox must not be distinguishable.
        let err = sandbox.validate(Path::new("../missing.txt")).unwrap_err();
        assert!(matches!(err, FoxProError::SandboxViolation { .. }));
        let err = sandbox.validate(Path::new("missing.txt")).unwrap_err();
        assert!(matches!(err, FoxProError::PathNotFound(_)));
        let err = sandbox
            .resolve(Path::new("missing/../../outside-new.txt"))
            .unwrap_err();
        assert!(matches!(err, FoxProError::SandboxViolation { .. }));
    }

    #[test]
    fn resolve_new_file_inside_workspace() {
        let dir = TempDir::new().unwrap();
        let sandbox = sandbox(&dir);
        let resolved = sandbox.resolve(Path::new("new/dir/../file.prg")).unwrap();
        assert_eq!(resolved, sandbox.workspace().join("new").join("file.prg"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let dir = TempDir::new().unwrap();
        let sandbox = sandbox(&dir);
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, sandbox.workspace().join("link")).unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("nowhere.txt"),
            sandbox.workspace().join("dangling"),
        )
        .unwrap();

        assert!(sandbox.resolve(Path::new("link/new.txt")).is_err());
        assert!(sandbox.resolve(Path::new("dangling")).is_err());
    }
}
