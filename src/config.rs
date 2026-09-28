use crate::error::{FoxProError, Result};
use crate::sandbox::Sandbox;
use crate::vfp::DEFAULT_TIMEOUT_SECS;
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;

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

impl Config {
    pub fn load(
        config_path: Option<PathBuf>,
        workspace_cli: Option<PathBuf>,
        log_level_cli: Option<String>,
        vfp_path_cli: Option<PathBuf>,
        vfp_timeout_cli: Option<u64>,
    ) -> Result<Self> {
        let mut workspace =
            workspace_cli.or_else(|| env::var("FOXPRO_WORKSPACE").ok().map(PathBuf::from));
        let mut log_level = log_level_cli.or_else(|| env::var("FOXPRO_LOG_LEVEL").ok());
        let mut vfp_path = vfp_path_cli.or_else(|| env::var("FOXPRO_PATH").ok().map(PathBuf::from));
        let mut vfp_timeout = vfp_timeout_cli
            .or_else(|| env::var("FOXPRO_TIMEOUT").ok().and_then(|s| s.parse().ok()));

        let file_path = config_path
            .or_else(|| env::var("FOXPRO_CONFIG").ok().map(PathBuf::from))
            .or_else(|| Some(PathBuf::from("foxpro-mcp.json")))
            .filter(|p| p.exists());

        if let Some(path) = file_path {
            let contents = fs::read_to_string(&path)?;
            let file: ConfigFile = serde_json::from_str(&contents)?;
            if workspace.is_none() {
                workspace = file.workspace.map(PathBuf::from);
            }
            if log_level.is_none() {
                log_level = file.log_level;
            }
            if vfp_path.is_none() {
                vfp_path = file.vfp_path.map(PathBuf::from);
            }
            if vfp_timeout.is_none() {
                vfp_timeout = file.vfp_timeout;
            }
        }

        let workspace =
            workspace.unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        if !workspace.exists() {
            return Err(FoxProError::WorkspaceNotFound(workspace));
        }
        let workspace = Sandbox::canonicalize(&workspace)?;
        let log_level = log_level.unwrap_or_else(|| "info".to_string());
        let vfp_timeout = vfp_timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);

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
        assert_eq!(cfg.vfp_timeout, 30);
    }

    #[test]
    fn load_defaults_to_info() {
        let dir = TempDir::new().unwrap();
        let cfg = Config::load(None, Some(dir.path().to_path_buf()), None, None, None).unwrap();
        assert_eq!(cfg.log_level, "info");
    }
}
