use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::config::Config;

const DEFAULT_GRPC_ADDR: &str = "0.0.0.0:4317";
const DEFAULT_HTTP_ADDR: &str = "0.0.0.0:4318";

#[derive(Parser, Debug, Clone)]
#[command(name = "otel-logger")]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// Optional subcommand. Omitting it runs the OTLP receiver (default action).
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Path to a TOML configuration file. Defaults to
    /// `$XDG_CONFIG_HOME/otel-logger/config.toml` (or `~/.config/otel-logger/config.toml`).
    #[arg(long, env = "OTEL_LOGGER_CONFIG", value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// gRPC bind address for OTLP/gRPC (port 4317 by spec).
    #[arg(long, env = "OTEL_LOGGER_GRPC_ADDR", value_name = "ADDR")]
    pub grpc_addr: Option<SocketAddr>,

    /// HTTP bind address for OTLP/HTTP (port 4318 by spec).
    #[arg(long, env = "OTEL_LOGGER_HTTP_ADDR", value_name = "ADDR")]
    pub http_addr: Option<SocketAddr>,

    /// Persist received telemetry as lossless JSON Lines into this file.
    /// The file is opened in append mode and is fsync'd on graceful shutdown.
    #[arg(long, env = "OTEL_LOGGER_LOG_FILE", value_name = "PATH")]
    pub log_file: Option<PathBuf>,

    /// Suppress the human-readable stdout stream.
    /// Useful when you only want the JSONL file to be written.
    #[arg(long, env = "OTEL_LOGGER_NO_STDOUT")]
    pub no_stdout: bool,

    /// Append a cumulative usage summary to stdout each time a
    /// `claude_code.api_request` event is received.
    /// The HTTP endpoint `GET /stats` is always available regardless of this
    /// flag and returns the same totals as JSON.
    #[arg(long, env = "OTEL_LOGGER_SUMMARY")]
    pub summary: bool,

    /// Color mode for the human-readable stdout stream.
    #[arg(long, value_enum, env = "OTEL_LOGGER_COLOR", value_name = "WHEN")]
    pub color: Option<ColorMode>,

    /// Validate startup (parse args, open log file, resolve addresses) but exit
    /// without binding the listeners. Useful as a smoke test in CI.
    #[arg(short = 'n', long)]
    pub dry_run: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Generate a default configuration file (defaults to
    /// `~/.config/otel-logger/config.toml`).
    Init {
        /// Destination path. Omit to use the XDG default location.
        #[arg(long, short = 'p', value_name = "PATH")]
        path: Option<PathBuf>,

        /// Overwrite an existing file. Without this, init refuses to clobber.
        #[arg(long, short = 'f')]
        force: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq, Serialize, Deserialize)]
#[value(rename_all = "lower")]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

impl ColorMode {
    /// Decide whether ANSI colors should be emitted right now.
    /// Honors `NO_COLOR` (https://no-color.org/) when in `Auto` mode.
    pub fn enabled_for_stdout(self) -> bool {
        match self {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => {
                if std::env::var_os("NO_COLOR").is_some() {
                    return false;
                }
                is_terminal::IsTerminal::is_terminal(&std::io::stdout())
            }
        }
    }
}

/// Resolved configuration: every field has a definite value after merging
/// CLI flags, environment variables, the config file, and built-in defaults.
#[derive(Debug, Clone)]
pub struct Settings {
    pub grpc_addr: SocketAddr,
    pub http_addr: SocketAddr,
    pub log_file: Option<PathBuf>,
    pub no_stdout: bool,
    pub summary: bool,
    pub color: ColorMode,
    pub dry_run: bool,
}

impl Settings {
    /// Merge precedence: CLI flag > env (handled by clap) > config > default.
    pub fn merge(cli: Cli, config: Config) -> Self {
        Self {
            grpc_addr: cli
                .grpc_addr
                .or(config.grpc_addr)
                .unwrap_or_else(|| DEFAULT_GRPC_ADDR.parse().expect("valid default")),
            http_addr: cli
                .http_addr
                .or(config.http_addr)
                .unwrap_or_else(|| DEFAULT_HTTP_ADDR.parse().expect("valid default")),
            log_file: cli.log_file.or(config.log_file),
            no_stdout: cli.no_stdout || config.no_stdout.unwrap_or(false),
            summary: cli.summary || config.summary.unwrap_or(false),
            color: cli.color.or(config.color).unwrap_or(ColorMode::Auto),
            dry_run: cli.dry_run,
        }
    }
}
