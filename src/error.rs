use serde_json::{Value, json};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
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
    SandboxViolation {
        workspace: PathBuf,
        requested: PathBuf,
    },

    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Validation failed: {0}")]
    Validation(String),

    #[error("Encoding error: {0}")]
    Encoding(String),

    #[error("Malformed file '{path}': {message}")]
    Malformed { path: PathBuf, message: String },

    #[error("Unsupported: {0}")]
    Unsupported(String),

    #[error("Directory traversal error: {0}")]
    WalkDir(#[from] walkdir::Error),

    #[error("VFP runtime not found or not configured: {0}")]
    VfpNotConfigured(String),

    #[error("Operation timed out after {0} seconds")]
    Timeout(u64),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl FoxProError {
    /// Stable, machine-readable error code returned to MCP clients.
    pub fn code(&self) -> &'static str {
        match self {
            FoxProError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => "FILE_NOT_FOUND",
            FoxProError::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                "PERMISSION_DENIED"
            }
            FoxProError::Io(_) => "IO_ERROR",
            FoxProError::Json(_) => "JSON_ERROR",
            FoxProError::Config(_) => "CONFIG_ERROR",
            FoxProError::WorkspaceNotFound(_) => "WORKSPACE_NOT_FOUND",
            FoxProError::PathNotFound(_) => "FILE_NOT_FOUND",
            FoxProError::SandboxViolation { .. } => "SANDBOX_VIOLATION",
            FoxProError::InvalidArgument(_) => "INVALID_ARGUMENT",
            FoxProError::NotFound(_) => "NOT_FOUND",
            FoxProError::Conflict(_) => "CONFLICT",
            FoxProError::Validation(_) => "VALIDATION_FAILED",
            FoxProError::Encoding(_) => "ENCODING_ERROR",
            FoxProError::Malformed { .. } => "MALFORMED_FILE",
            FoxProError::Unsupported(_) => "UNSUPPORTED",
            FoxProError::WalkDir(_) => "IO_ERROR",
            FoxProError::VfpNotConfigured(_) => "VFP_NOT_CONFIGURED",
            FoxProError::Timeout(_) => "TIMEOUT",
            FoxProError::Internal(_) => "INTERNAL_ERROR",
        }
    }

    /// Structured error payload: `{ "success": false, "error": { code, message, path? } }`.
    pub fn to_json(&self) -> Value {
        let mut error = json!({
            "code": self.code(),
            "message": self.to_string(),
        });
        let path = match self {
            FoxProError::PathNotFound(p) | FoxProError::WorkspaceNotFound(p) => Some(p),
            FoxProError::SandboxViolation { requested, .. } => Some(requested),
            FoxProError::Malformed { path, .. } => Some(path),
            _ => None,
        };
        if let Some(p) = path {
            error["path"] = json!(p.to_string_lossy());
        }
        json!({ "success": false, "error": error })
    }

    pub fn malformed(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        FoxProError::Malformed {
            path: path.into(),
            message: message.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, FoxProError>;
