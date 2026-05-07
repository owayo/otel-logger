use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::ColorMode;

/// Reference content emitted by `otel-logger init`. Embedded directly so the
/// binary can generate a fresh configuration without needing the source tree.
const DEFAULT_CONFIG_TEMPLATE: &str = r#"# otel-logger configuration file
#
# Default location: $XDG_CONFIG_HOME/otel-logger/config.toml
#                  (or ~/.config/otel-logger/config.toml when XDG_CONFIG_HOME is unset)
#
# Override with `otel-logger --config /path/to/config.toml`.
#
# Precedence: CLI flag > environment variable > config file > built-in default.
# Every key below is optional. Comment a key out to fall back to the default.

# Persist received OTLP telemetry as lossless JSON Lines. The parent directory
# is created on startup; the file is opened in append mode and fsync'd on
# graceful shutdown.
log-file = "/var/log/otel-logger/otel-logger.jsonl"

# Suppress the human-readable stdout stream. Use this when you only want
# the JSONL file to be written.
no-stdout = false

# Append a cumulative usage summary (input/output/cache tokens, cost, by
# provider/model/effort) to stdout each time a `claude_code.api_request` log
# or a Codex `handle_responses` trace span is received. The HTTP endpoint
# `GET /stats` is always available regardless of this flag.
summary = false

# Color mode for stdout: "auto" | "always" | "never". `auto` honors NO_COLOR
# and skips ANSI codes when stdout is not a TTY.
color = "auto"

# Bind addresses. Defaults are 0.0.0.0:4317 (OTLP/gRPC) and 0.0.0.0:4318 (OTLP/HTTP).
# grpc-addr = "0.0.0.0:4317"
# http-addr = "0.0.0.0:4318"
"#;

/// On-disk configuration loaded from `~/.config/otel-logger/config.toml`.
/// Every field is optional so a partial config still merges cleanly with
/// CLI flags and environment variables.
///
/// Precedence: CLI flag > environment variable > config file > built-in default.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// gRPC bind address (OTLP/gRPC).
    pub grpc_addr: Option<SocketAddr>,
    /// HTTP bind address (OTLP/HTTP).
    pub http_addr: Option<SocketAddr>,
    /// Persist received telemetry as JSON Lines into this file.
    pub log_file: Option<PathBuf>,
    /// Suppress the human-readable stdout stream.
    pub no_stdout: Option<bool>,
    /// Append a cumulative usage summary to stdout on each `claude_code.api_request`.
    pub summary: Option<bool>,
    /// Color mode for the human-readable stdout stream.
    pub color: Option<ColorMode>,
}

impl Config {
    /// Load configuration. If `explicit` is given the file MUST exist.
    /// Otherwise the default XDG path is consulted; a missing file is not an
    /// error and yields an empty configuration.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        match explicit {
            Some(path) => {
                if !path.exists() {
                    anyhow::bail!("configuration file does not exist: {}", path.display());
                }
                Self::load_from(path)
            }
            None => match default_config_path() {
                Some(path) if path.exists() => Self::load_from(&path),
                _ => Ok(Self::default()),
            },
        }
    }

    fn load_from(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("read config file {}", path.display()))?;
        let config: Self =
            toml::from_str(&body).with_context(|| format!("parse TOML in {}", path.display()))?;
        Ok(config)
    }
}

/// Resolve the default config path: `$XDG_CONFIG_HOME/otel-logger/config.toml`
/// when set, otherwise `$HOME/.config/otel-logger/config.toml`. Returns
/// `None` only if both env vars are unset, which only happens in tests.
pub fn default_config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("otel-logger").join("config.toml"));
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("otel-logger")
            .join("config.toml"),
    )
}

/// Outcome of `write_default`. Used by callers to print a friendly message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOutcome {
    Created,
    Overwrote,
}

/// Write the bundled `DEFAULT_CONFIG_TEMPLATE` to `path`. Creates parent
/// directories as needed. Refuses to overwrite an existing file unless
/// `force` is set.
pub fn write_default(path: &Path, force: bool) -> Result<InitOutcome> {
    let already_exists = path.exists();
    if already_exists && !force {
        anyhow::bail!(
            "{} already exists; pass --force / -f to overwrite",
            path.display()
        );
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent directory of {}", path.display()))?;
    }
    std::fs::write(path, DEFAULT_CONFIG_TEMPLATE)
        .with_context(|| format!("write config file {}", path.display()))?;
    Ok(if already_exists {
        InitOutcome::Overwrote
    } else {
        InitOutcome::Created
    })
}
