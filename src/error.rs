use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum FoxProError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Workspace not found: {0}")]
    WorkspaceNotFound(PathBuf),

    #[error("Path not found: {0}")]
    PathNotFound(PathBuf),

    #[error("Sandbox violation: requested path '{requested}' is outside workspace '{workspace}'")]
    SandboxViolation { workspace: PathBuf, requested: PathBuf },

    #[error("Missing workspace: specify --workspace, FOXPRO_WORKSPACE, or workspace in foxpro-mcp.json")]
    MissingWorkspace,

    #[error("Invalid RPC request: {0}")]
    Rpc(String),

    #[error("Directory traversal error: {0}")]
    WalkDir(#[from] walkdir::Error),
}

pub type Result<T> = std::result::Result<T, FoxProError>;
