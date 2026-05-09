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
    /// Optional subcommand. Omitting it runs the OTLP receiver.
    /// 任意のサブコマンド。省略すると OTLP receiver を起動する。
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Path to a TOML configuration file.
    /// TOML 設定ファイルのパス。既定では
    /// `$XDG_CONFIG_HOME/otel-logger/config.toml` (または `~/.config/otel-logger/config.toml`)。
    #[arg(long, env = "OTEL_LOGGER_CONFIG", value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Bind address for OTLP/gRPC (spec port 4317).
    /// OTLP/gRPC の bind address (仕様上のポートは 4317)。
    #[arg(long, env = "OTEL_LOGGER_GRPC_ADDR", value_name = "ADDR")]
    pub grpc_addr: Option<SocketAddr>,

    /// Bind address for OTLP/HTTP (spec port 4318).
    /// OTLP/HTTP の bind address (仕様上のポートは 4318)。
    #[arg(long, env = "OTEL_LOGGER_HTTP_ADDR", value_name = "ADDR")]
    pub http_addr: Option<SocketAddr>,

    /// Persist received telemetry as lossless JSON Lines into this file.
    /// 受信した telemetry を欠落のない JSON Lines としてこのファイルへ保存する。
    /// ファイルは追記モードで開き、graceful shutdown 時に fsync する。
    /// `--log-dir` とは同時に指定できない。
    #[arg(long, env = "OTEL_LOGGER_LOG_FILE", value_name = "PATH")]
    pub log_file: Option<PathBuf>,

    /// Persist JSON Lines into a directory with daily rotation.
    /// JSON Lines を日次ローテーション付きでディレクトリへ保存する。
    /// ファイル名はローカル時刻の `otel-logger.YYYY-MM-DD` で、`--log-keep-days`
    /// 日分を保持する。`--log-file` とは同時に指定できない。
    #[arg(long, env = "OTEL_LOGGER_LOG_DIR", value_name = "DIR")]
    pub log_dir: Option<PathBuf>,

    /// Number of rotated daily JSONL files to retain when `--log-dir` is used.
    /// `--log-dir` 利用時に保持する日次 JSONL ファイル数。
    /// 古いファイルは起動時に削除し、ローテーション時にも上限を適用する。
    /// 既定値は 10。
    #[arg(long, env = "OTEL_LOGGER_LOG_KEEP_DAYS", value_name = "DAYS")]
    pub log_keep_days: Option<u32>,

    /// Suppress the human-readable stdout stream.
    /// 人が読める stdout 出力を抑止する。
    /// JSONL ファイルだけを書き出したい場合に使う。
    #[arg(long, env = "OTEL_LOGGER_NO_STDOUT")]
    pub no_stdout: bool,

    /// Append a cumulative usage summary when Claude/Codex usage changes.
    /// Claude/Codex の使用量を更新したタイミングで、stdout に累計サマリーを追記する。
    /// HTTP endpoint `GET /stats` はこのフラグに関係なく常に有効で、
    /// 同じ累計値を JSON として返す。
    #[arg(long, env = "OTEL_LOGGER_SUMMARY")]
    pub summary: bool,

    /// Color mode for the human-readable stdout stream.
    /// 人が読める stdout 出力の色設定。
    #[arg(long, value_enum, env = "OTEL_LOGGER_COLOR", value_name = "WHEN")]
    pub color: Option<ColorMode>,

    /// Validate startup without binding listeners.
    /// 起動処理 (引数解析、ログファイル open、address 解決) だけ検証し、
    /// listener を bind せず終了する。CI の smoke test に使う。
    #[arg(short = 'n', long)]
    pub dry_run: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Generate a default configuration file.
    /// 既定の設定ファイルを生成する (既定の出力先は
    /// `~/.config/otel-logger/config.toml`)。
    Init {
        /// Destination path. Omit to use the XDG default location.
        /// 出力先パス。省略時は XDG の既定パスを使う。
        #[arg(long, short = 'p', value_name = "PATH")]
        path: Option<PathBuf>,

        /// Overwrite an existing file.
        /// 既存ファイルを上書きする。未指定の場合、init は上書きを拒否する。
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
    /// 現在の stdout に ANSI color を出すか判定する。
    /// `Auto` mode では `NO_COLOR` (https://no-color.org/) を尊重する。
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

pub const DEFAULT_LOG_KEEP_DAYS: u32 = 10;

/// JSONL 出力先。追記専用ファイルか日次ローテーション付きディレクトリのどちらか一方。
#[derive(Debug, Clone)]
pub enum LogSink {
    File(PathBuf),
    /// ディレクトリと、保持する日次ローテーションファイル数。
    Directory {
        dir: PathBuf,
        keep_days: u32,
    },
}

/// 解決済み設定。CLI flags、環境変数、設定ファイル、組み込み既定値を merge した後なので、
/// すべての field が確定値を持つ。
#[derive(Debug, Clone)]
pub struct Settings {
    pub grpc_addr: SocketAddr,
    pub http_addr: SocketAddr,
    pub log_sink: Option<LogSink>,
    pub no_stdout: bool,
    pub summary: bool,
    pub color: ColorMode,
    pub dry_run: bool,
}

impl Settings {
    /// merge 優先順位: CLI flag > env (clap が処理) > config > default。
    pub fn merge(cli: Cli, config: Config) -> anyhow::Result<Self> {
        let log_file = cli.log_file.or(config.log_file);
        let log_dir = cli.log_dir.or(config.log_dir);
        let keep_days = cli
            .log_keep_days
            .or(config.log_keep_days)
            .unwrap_or(DEFAULT_LOG_KEEP_DAYS);
        let log_sink = match (log_file, log_dir) {
            (Some(_), Some(_)) => {
                anyhow::bail!("`--log-file` and `--log-dir` are mutually exclusive");
            }
            (Some(file), None) => Some(LogSink::File(file)),
            (None, Some(dir)) => Some(LogSink::Directory { dir, keep_days }),
            (None, None) => None,
        };

        Ok(Self {
            grpc_addr: cli
                .grpc_addr
                .or(config.grpc_addr)
                .unwrap_or_else(|| DEFAULT_GRPC_ADDR.parse().expect("valid default")),
            http_addr: cli
                .http_addr
                .or(config.http_addr)
                .unwrap_or_else(|| DEFAULT_HTTP_ADDR.parse().expect("valid default")),
            log_sink,
            no_stdout: cli.no_stdout || config.no_stdout.unwrap_or(false),
            summary: cli.summary || config.summary.unwrap_or(false),
            color: cli.color.or(config.color).unwrap_or(ColorMode::Auto),
            dry_run: cli.dry_run,
        })
    }
}
