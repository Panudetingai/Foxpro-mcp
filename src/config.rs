use crate::error::{FoxProError, Result};
use crate::sandbox::Sandbox;
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub workspace: PathBuf,
    pub log_level: String,
}

#[derive(Debug, Deserialize, Default)]
struct ConfigFile {
    workspace: Option<String>,
    log_level: Option<String>,
}

impl Config {
    pub fn load(
        config_path: Option<PathBuf>,
        workspace_cli: Option<PathBuf>,
        log_level_cli: Option<String>,
    ) -> Result<Self> {
        let mut workspace = workspace_cli
            .or_else(|| env::var("FOXPRO_WORKSPACE").ok().map(PathBuf::from));
        let mut log_level = log_level_cli.or_else(|| env::var("FOXPRO_LOG_LEVEL").ok());

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
        }

        let workspace = workspace.unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        if !workspace.exists() {
            return Err(FoxProError::WorkspaceNotFound(workspace));
        }
        let workspace = Sandbox::canonicalize(&workspace)?;
        let log_level = log_level.unwrap_or_else(|| "info".to_string());

        Ok(Config { workspace, log_level })
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
        file.write_all(br#"{ "workspace": "/tmp/ignored", "log_level": "debug" }"#).unwrap();

        let cfg = Config::load(Some(config_path), Some(ws.clone()), Some("warn".to_string())).unwrap();
        assert_eq!(cfg.workspace, Sandbox::canonicalize(&ws).unwrap());
        assert_eq!(cfg.log_level, "warn");
    }

    #[test]
    fn load_defaults_to_info() {
        let dir = TempDir::new().unwrap();
        let cfg = Config::load(None, Some(dir.path().to_path_buf()), None).unwrap();
        assert_eq!(cfg.log_level, "info");
    }
}
