use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "foxpro-mcp", about = "FoxPro Model Context Protocol server", version)]
pub struct Args {
    #[arg(long, help = "Workspace directory that all file operations are restricted to")]
    pub workspace: Option<PathBuf>,

    #[arg(long, help = "Path to a foxpro-mcp.json configuration file")]
    pub config: Option<PathBuf>,

    #[arg(long, help = "Log level filter, e.g. debug, info, warn, error")]
    pub log_level: Option<String>,
}
