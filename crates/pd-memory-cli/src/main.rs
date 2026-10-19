//! `pd-memory` command line: MCP server, agent hook handler, and memory admin.
//!
//! The binary is a thin host adapter: it owns config/env resolution and process
//! I/O, while all behavior lives in `pd-memory-core` + `pd-memory-llm` +
//! `pd-memory-mcp` + `pd-memory-hooks`.

mod commands;
mod config_loader;
mod host;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use config_loader::GlobalOptions;

/// Portable, file-native long-term memory for coding agents.
#[derive(Debug, Parser)]
#[command(name = "pd-memory", version, about, long_about = None)]
struct Cli {
    /// JSON configuration file (`MemoryConfig`).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Memory data root (overrides config and PD_MEMORY_DATA_ROOT).
    #[arg(long, global = true, value_name = "DIR")]
    data_root: Option<PathBuf>,
    /// Workspace id or project path (overrides config/environment).
    #[arg(long, global = true, value_name = "ID_OR_PATH")]
    workspace: Option<String>,
    /// Log level: error, warn, info, debug or trace.
    #[arg(long, global = true, default_value = "warn", value_name = "LEVEL")]
    log_level: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Write an example JSON configuration file.
    Init {
        /// Overwrite an existing config file.
        #[arg(long)]
        force: bool,
        /// Output path (defaults to --config or <data-root>/config.json).
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Run an integration server (MCP stdio).
    Serve {
        #[command(subcommand)]
        transport: ServeTransport,
    },
    /// Handle one agent lifecycle hook invocation (stdin JSON -> stdout JSON).
    Hook {
        /// WorkBuddy session-end flush and first-recall backlog barrier.
        #[arg(long, conflicts_with = "codex")]
        workbuddy: bool,
        /// Codex first-recall backlog barrier; SessionEnd only captures and
        /// Stop/SessionEnd output stays within Codex's strict schemas.
        #[arg(long)]
        codex: bool,
    },
    /// Capture one conversation turn into the raw L0 queue.
    Add {
        /// Session id used to group turns (default `cli`).
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// Role for positional content (default `user`).
        #[arg(long, default_value = "user")]
        role: String,
        /// Capture source label (default `cli`).
        #[arg(long, default_value = "cli")]
        source: String,
        /// ISO-8601 timestamp to use for the captured batch (parity tests/replay).
        #[arg(long, value_name = "ISO8601")]
        timestamp: Option<String>,
        /// Message text. Omit to read JSON messages from stdin.
        #[arg(value_name = "CONTENT", num_args = 0.., allow_hyphen_values = true)]
        content: Vec<String>,
    },
    /// Search long-term memory (model-guided, explicit).
    Search {
        /// Natural-language query.
        query: String,
        /// Keep at most this many selected memory ids.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// List memory manifest entries.
    List {
        /// Restrict to kinds (comma separated): user, feedback, project, general_project_meta.
        #[arg(long, value_delimiter = ',', value_name = "KIND")]
        kind: Vec<String>,
        /// Filter over names/descriptions.
        #[arg(long)]
        query: Option<String>,
        /// Maximum number of entries.
        #[arg(long)]
        limit: Option<usize>,
        /// Include deprecated entries.
        #[arg(long)]
        include_deprecated: bool,
    },
    /// Read memory files by id (relative paths such as Project/foo-123.md).
    Get {
        /// One or more memory ids.
        #[arg(value_name = "ID", required = true)]
        ids: Vec<String>,
        /// Maximum body lines per file (default 200).
        #[arg(long, default_value_t = 200)]
        max_lines: usize,
    },
    /// Print memory status: counts, freshness, backlog, project metadata.
    Overview,
    /// Run indexing now (turn pending captures into memory files).
    Flush {
        /// Maximum number of pending L0 batches to process.
        #[arg(long)]
        batch_size: Option<usize>,
        /// Reason recorded in pipeline traces (default `cli`).
        #[arg(long, default_value = "cli")]
        reason: String,
    },
    /// Run dream consolidation (reversible with `rollback`).
    Dream {
        /// Trigger label: manual or scheduled (default manual).
        #[arg(long, default_value = "manual")]
        trigger: String,
    },
    /// Roll back the last dream commit.
    Rollback,
    /// Export all memory to a JSON bundle file (atomic write).
    Export {
        /// Destination file.
        file: PathBuf,
    },
    /// Import a JSON memory bundle.
    Import {
        /// Source bundle file.
        file: PathBuf,
    },
    /// Print a UI/administration snapshot.
    Snapshot {
        /// Maximum entries included (default 50).
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Diagnose configuration, directories and model/provider state.
    Doctor,
}

#[derive(Debug, Subcommand)]
enum ServeTransport {
    /// Serve the Model Context Protocol over stdio (newline-delimited JSON-RPC).
    Mcp,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(&cli.log_level);
    let options = GlobalOptions {
        config: cli.config.clone(),
        data_root: cli.data_root.clone(),
        workspace: cli.workspace.clone(),
    };
    match run(&options, cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pd-memory error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(options: &GlobalOptions, command: Command) -> pd_memory_core::Result<()> {
    match command {
        Command::Init { force, path } => commands::init(options, force, path.as_deref()),
        Command::Serve { transport } => match transport {
            ServeTransport::Mcp => commands::serve_mcp(options),
        },
        Command::Hook { workbuddy, codex } => {
            let host = if workbuddy {
                Some(host::Host::WorkBuddy)
            } else if codex {
                Some(host::Host::Codex)
            } else {
                None
            };
            // The hook contract requires exit code 0 and a valid JSON line even
            // on failure; `commands::hook` handles that internally.
            commands::hook(options, host);
            Ok(())
        }
        Command::Add {
            session,
            role,
            source,
            timestamp,
            content,
        } => commands::add(options, session, role, source, timestamp, content),
        Command::Search { query, limit } => commands::search(options, query, limit),
        Command::List {
            kind,
            query,
            limit,
            include_deprecated,
        } => commands::list(options, kind, query, limit, include_deprecated),
        Command::Get { ids, max_lines } => commands::get(options, ids, max_lines),
        Command::Overview => commands::overview(options),
        Command::Flush { batch_size, reason } => commands::flush(options, batch_size, reason),
        Command::Dream { trigger } => commands::dream(options, trigger),
        Command::Rollback => commands::rollback(options),
        Command::Export { file } => commands::export(options, &file),
        Command::Import { file } => commands::import(options, &file),
        Command::Snapshot { limit } => commands::snapshot(options, limit),
        Command::Doctor => commands::doctor(options),
    }
}

/// Configure `env_logger` (stderr by default, so MCP/hook stdout stays clean).
fn init_logging(level: &str) {
    let level = level.trim();
    let level = if level.is_empty() { "warn" } else { level };
    let environment = env_logger::Env::default().default_filter_or(level);
    if env_logger::Builder::from_env(environment)
        .format_timestamp_millis()
        .try_init()
        .is_err()
    {
        // Logging was already initialised (e.g. embedded use); keep going.
    }
}
