use crate::error::{FoxProError, Result};
use crate::sandbox::Sandbox;
use crate::vfp::{DEFAULT_TIMEOUT_SECS, MAX_TIMEOUT_SECS};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub workspace: PathBuf,
    pub log_level: String,
    pub vfp_path: Option<PathBuf>,
    pub vfp_timeout: u64,
}

#[derive(Debug, Deserialize, Default)]
struct ConfigFile {
    workspace: Option<String>,
    log_level: Option<String>,
    vfp_path: Option<String>,
    vfp_timeout: Option<u64>,
}

/// Relative paths in a config file are relative to the file's directory, not
/// to whatever directory the MCP client happened to start the server from.
fn relative_to(base: &Path, value: String) -> PathBuf {
    let p = PathBuf::from(value);
    if p.is_absolute() { p } else { base.join(p) }
}

impl Config {
    pub fn load(
        config_path: Option<PathBuf>,
        workspace_cli: Option<PathBuf>,
        log_level_cli: Option<String>,
        vfp_path_cli: Option<PathBuf>,
        vfp_timeout_cli: Option<u64>,
    ) -> Result<Self> {
        let mut workspace =
            workspace_cli.or_else(|| env::var_os("FOXPRO_WORKSPACE").map(PathBuf::from));
        let mut log_level = log_level_cli.or_else(|| env::var("FOXPRO_LOG_LEVEL").ok());
        let mut vfp_path = vfp_path_cli.or_else(|| env::var_os("FOXPRO_PATH").map(PathBuf::from));
        let mut vfp_timeout = match vfp_timeout_cli {
            Some(t) => Some(t),
            None => match env::var("FOXPRO_TIMEOUT") {
                Ok(s) => Some(s.trim().parse().map_err(|_| {
                    FoxProError::Config(format!("FOXPRO_TIMEOUT is not a number: {s}"))
                })?),
                Err(_) => None,
            },
        };

        // An explicitly requested config file must exist; the default one is optional.
        let explicit = config_path.or_else(|| env::var_os("FOXPRO_CONFIG").map(PathBuf::from));
        let file_path = match explicit {
            Some(p) if !p.is_file() => {
                return Err(FoxProError::Config(format!(
                    "config file not found: {}",
                    p.display()
                )));
            }
            Some(p) => Some(p),
            None => Some(PathBuf::from("foxpro-mcp.json")).filter(|p| p.is_file()),
        };

        if let Some(path) = file_path {
            let contents = fs::read_to_string(&path)?;
            let file: ConfigFile = serde_json::from_str(&contents).map_err(|e| {
                FoxProError::Config(format!("invalid config file {}: {e}", path.display()))
            })?;
            let base = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            if workspace.is_none() {
                workspace = file.workspace.map(|w| relative_to(&base, w));
            }
            if log_level.is_none() {
                log_level = file.log_level;
            }
            if vfp_path.is_none() {
                vfp_path = file.vfp_path.map(|v| {
                    // Bare executable names ("vfp9") are looked up on PATH later.
                    if v.contains('/') || v.contains('\\') {
                        relative_to(&base, v)
                    } else {
                        PathBuf::from(v)
                    }
                });
            }
            if vfp_timeout.is_none() {
                vfp_timeout = file.vfp_timeout;
            }
        }

        let workspace = match workspace {
            Some(w) => w,
            None => env::current_dir()?,
        };
        if !workspace.is_dir() {
            return Err(FoxProError::WorkspaceNotFound(workspace));
        }
        let workspace = Sandbox::canonicalize(&workspace)?;
        let log_level = log_level.unwrap_or_else(|| "info".to_string());
        let vfp_timeout = vfp_timeout
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);

        Ok(Config {
            workspace,
            log_level,
            vfp_path,
            vfp_timeout,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn load_from_cli_overrides_file() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path().join("ws");
        fs::create_dir(&ws).unwrap();
        let config_path = dir.path().join("foxpro-mcp.json");
        let mut file = fs::File::create(&config_path).unwrap();
        file.write_all(br#"{ "workspace": "/tmp/ignored", "log_level": "debug" }"#)
            .unwrap();

        let cfg = Config::load(
            Some(config_path),
            Some(ws.clone()),
            Some("warn".to_string()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(cfg.workspace, Sandbox::canonicalize(&ws).unwrap());
        assert_eq!(cfg.log_level, "warn");
        assert_eq!(cfg.vfp_timeout, DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn load_defaults_to_info() {
        let dir = TempDir::new().unwrap();
        let cfg = Config::load(None, Some(dir.path().to_path_buf()), None, None, None).unwrap();
        assert_eq!(cfg.log_level, "info");
    }

    #[test]
    fn workspace_in_config_is_relative_to_config_file() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("project")).unwrap();
        let config_path = dir.path().join("foxpro-mcp.json");
        fs::write(
            &config_path,
            r#"{ "workspace": "project", "vfp_timeout": 0 }"#,
        )
        .unwrap();

        let cfg = Config::load(Some(config_path), None, None, None, None).unwrap();
        assert_eq!(
            cfg.workspace,
            Sandbox::canonicalize(&dir.path().join("project")).unwrap()
        );
        assert_eq!(cfg.vfp_timeout, 1);
    }

    #[test]
    fn missing_explicit_config_is_an_error() {
        let dir = TempDir::new().unwrap();
        let err = Config::load(
            Some(dir.path().join("nope.json")),
            Some(dir.path().to_path_buf()),
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, FoxProError::Config(_)));
    }
}
