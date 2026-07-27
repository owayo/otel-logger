use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::config::{Config, ProxyRouteConfig, ProxySignal, ProxyTransport};
use crate::path::expand_user_path;

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

    /// Validate startup and listener bind availability.
    /// 起動処理 (引数解析、ログファイル open、address 解決) だけ検証し、
    /// gRPC / HTTP listener が同時に bind できることを確認して終了する。CI の smoke test に使う。
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Forward Claude Code (`service.name=claude-code`) telemetry to this OTLP endpoint.
    /// `claude-code` の OTLP を JSONL 保存と同時にこの endpoint へ転送する。
    /// `--proxy-anthropic-transport` で `grpc` (既定) / `http-protobuf` を選ぶ。
    #[arg(long, env = "OTEL_LOGGER_PROXY_ANTHROPIC_ENDPOINT", value_name = "URL")]
    pub proxy_anthropic_endpoint: Option<String>,

    /// Transport used for `--proxy-anthropic-endpoint`.
    /// Anthropic 側 proxy の転送プロトコル。
    #[arg(
        long,
        value_enum,
        env = "OTEL_LOGGER_PROXY_ANTHROPIC_TRANSPORT",
        value_name = "TRANSPORT"
    )]
    pub proxy_anthropic_transport: Option<ProxyTransportArg>,

    /// Additional header to send with Anthropic proxy requests (`key=value`).
    /// Anthropic 側 proxy に付与する HTTP header。`Authorization=env:MY_TOKEN` のように
    /// value を `env:VAR` にすると環境変数から解決し、config / process 一覧に平文を残さない。
    /// 複数指定可。
    #[arg(
        long = "proxy-anthropic-header",
        env = "OTEL_LOGGER_PROXY_ANTHROPIC_HEADERS",
        value_name = "KEY=VALUE",
        value_delimiter = ',',
        num_args = 0..
    )]
    pub proxy_anthropic_headers: Vec<String>,

    /// Forward Codex (`service.name=codex_cli_rs` etc.) telemetry to this OTLP endpoint.
    /// Codex 系 (`codex_cli_rs` / `codex_exec` / `codex-app-server`) の OTLP を、
    /// JSONL 保存と同時にこの endpoint へ転送する。
    #[arg(long, env = "OTEL_LOGGER_PROXY_OPENAI_ENDPOINT", value_name = "URL")]
    pub proxy_openai_endpoint: Option<String>,

    /// Transport used for `--proxy-openai-endpoint`.
    /// OpenAI 側 proxy の転送プロトコル。
    #[arg(
        long,
        value_enum,
        env = "OTEL_LOGGER_PROXY_OPENAI_TRANSPORT",
        value_name = "TRANSPORT"
    )]
    pub proxy_openai_transport: Option<ProxyTransportArg>,

    /// Additional header to send with OpenAI proxy requests (`key=value`).
    /// OpenAI 側 proxy に付与する HTTP header。value に `env:VAR` を書ける (複数指定可)。
    #[arg(
        long = "proxy-openai-header",
        env = "OTEL_LOGGER_PROXY_OPENAI_HEADERS",
        value_name = "KEY=VALUE",
        value_delimiter = ',',
        num_args = 0..
    )]
    pub proxy_openai_headers: Vec<String>,

    /// Directory reserved for Phase B per-route proxy checkpoints.
    /// Phase B で proxy 転送の checkpoint (JSONL 内 offset) を保存する予約ディレクトリ。
    /// 未指定時は JSONL 出力先の親配下 `.otel-logger-proxy/` を使う。
    #[arg(long, env = "OTEL_LOGGER_PROXY_CHECKPOINT_DIR", value_name = "DIR")]
    pub proxy_checkpoint_dir: Option<PathBuf>,
}

/// CLI で受ける転送プロトコル指定。
#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
#[value(rename_all = "kebab-case")]
pub enum ProxyTransportArg {
    Grpc,
    HttpProtobuf,
}

impl From<ProxyTransportArg> for ProxyTransport {
    fn from(v: ProxyTransportArg) -> Self {
        match v {
            ProxyTransportArg::Grpc => ProxyTransport::Grpc,
            ProxyTransportArg::HttpProtobuf => ProxyTransport::HttpProtobuf,
        }
    }
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

/// notify channel の bounded 容量デフォルト。Phase A では overflow を drop counter に記録し、
/// JSONL からの catch-up は Phase B で実装する。
pub const DEFAULT_PROXY_QUEUE_CAPACITY: usize = 1024;
/// 1 回の proxy 送信の I/O timeout デフォルト (ミリ秒)。
pub const DEFAULT_PROXY_TIMEOUT_MS: u64 = 5000;
/// 送信失敗時の指数バックオフ試行回数デフォルト。Phase A では使い切った request を
/// 自動再送せず、JSONL からの再走査は Phase B で実装する。
pub const DEFAULT_PROXY_RETRY_MAX: u32 = 8;

/// 組み込みの vendor 既定マッピング。CLI 短縮フラグ / 明示的な
/// `[[proxy.routes]]` で `service_names` を上書き可能。
pub const DEFAULT_ANTHROPIC_SERVICES: &[&str] = &["claude-code"];
pub const DEFAULT_OPENAI_SERVICES: &[&str] = &["codex_cli_rs", "codex_exec", "codex-app-server"];

/// route 1 件分の解決済み設定。CLI/config/組み込み既定を merge 済みで、環境変数
/// (`env:VAR`) も展開済みなので、あとは forwarder が使うだけの状態。
#[derive(Debug, Clone)]
pub struct ProxyRoute {
    pub name: String,
    pub service_names: Vec<String>,
    pub signals: Vec<ProxySignal>,
    pub transport: ProxyTransport,
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
}

impl ProxyRoute {
    /// signal が有効か判定する。
    pub fn accepts_signal(&self, signal: ProxySignal) -> bool {
        self.signals.contains(&signal)
    }
}

/// proxy 転送の解決済み設定。route が 1 件以上あるときのみ `Some` になる。
#[derive(Debug, Clone)]
pub struct ProxySettings {
    pub queue_capacity: usize,
    pub timeout_ms: u64,
    pub retry_max: u32,
    /// Phase B 用に先行予約している。Phase A の worker はまだ参照しない。
    pub checkpoint_dir: PathBuf,
    pub routes: Vec<ProxyRoute>,
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
    /// proxy 転送設定 (未設定なら `None`)。
    pub proxy: Option<ProxySettings>,
}

impl Settings {
    /// merge 優先順位: CLI flag > env (clap が処理) > config > default。
    pub fn merge(cli: Cli, config: Config) -> anyhow::Result<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::merge_with_home(cli, config, home.as_deref())
    }

    fn merge_with_home(cli: Cli, config: Config, home: Option<&Path>) -> anyhow::Result<Self> {
        let keep_days = cli
            .log_keep_days
            .or(config.log_keep_days)
            .unwrap_or(DEFAULT_LOG_KEEP_DAYS);

        let expand = |p: PathBuf| expand_user_path(p, home);

        let log_sink = match (cli.log_file, cli.log_dir) {
            (Some(_), Some(_)) => {
                anyhow::bail!("`--log-file` and `--log-dir` are mutually exclusive");
            }
            (Some(file), None) => Some(LogSink::File(expand(file))),
            (None, Some(dir)) => Some(LogSink::Directory {
                dir: expand(dir),
                keep_days,
            }),
            (None, None) => match (config.log_file, config.log_dir) {
                (Some(_), Some(_)) => {
                    anyhow::bail!("`log-file` and `log-dir` are mutually exclusive");
                }
                (Some(file), None) => Some(LogSink::File(expand(file))),
                (None, Some(dir)) => Some(LogSink::Directory {
                    dir: expand(dir),
                    keep_days,
                }),
                (None, None) => None,
            },
        };

        let proxy = resolve_proxy_settings(
            ProxyCliInputs {
                anthropic_endpoint: cli.proxy_anthropic_endpoint,
                anthropic_transport: cli.proxy_anthropic_transport,
                anthropic_headers: cli.proxy_anthropic_headers,
                openai_endpoint: cli.proxy_openai_endpoint,
                openai_transport: cli.proxy_openai_transport,
                openai_headers: cli.proxy_openai_headers,
                checkpoint_dir: cli.proxy_checkpoint_dir,
            },
            config.proxy,
            log_sink.as_ref(),
            home,
        )?;

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
            proxy,
        })
    }
}

/// CLI 側の proxy 短縮フラグをまとめる中間 struct。`merge` から `resolve_proxy_settings` に渡す。
struct ProxyCliInputs {
    anthropic_endpoint: Option<String>,
    anthropic_transport: Option<ProxyTransportArg>,
    anthropic_headers: Vec<String>,
    openai_endpoint: Option<String>,
    openai_transport: Option<ProxyTransportArg>,
    openai_headers: Vec<String>,
    checkpoint_dir: Option<PathBuf>,
}

/// route 名で識別する組み込み既定 vendor。CLI 短縮フラグの endpoint はここに紐付ける。
const ROUTE_ANTHROPIC: &str = "anthropic";
const ROUTE_OPENAI: &str = "openai";

fn resolve_proxy_settings(
    cli: ProxyCliInputs,
    config: Option<crate::config::ProxyConfig>,
    log_sink: Option<&LogSink>,
    home: Option<&Path>,
) -> anyhow::Result<Option<ProxySettings>> {
    let config = config.unwrap_or_default();

    // config 側の routes を name をキーに map 化 (config 明示指定が組み込み既定を上書き)。
    let mut config_routes: BTreeMap<String, ProxyRouteConfig> = BTreeMap::new();
    for route in config.routes {
        if config_routes.contains_key(&route.name) {
            anyhow::bail!(
                "duplicate proxy route name `{name}` in configuration",
                name = route.name
            );
        }
        config_routes.insert(route.name.clone(), route);
    }

    let mut routes: Vec<ProxyRoute> = Vec::new();

    // Anthropic route を CLI or config から組み立てる。
    if let Some(route) = build_vendor_route(
        ROUTE_ANTHROPIC,
        DEFAULT_ANTHROPIC_SERVICES,
        cli.anthropic_endpoint,
        cli.anthropic_transport,
        cli.anthropic_headers,
        config_routes.remove(ROUTE_ANTHROPIC),
    )? {
        routes.push(route);
    }
    // OpenAI (Codex) route も同様に組み立てる。
    if let Some(route) = build_vendor_route(
        ROUTE_OPENAI,
        DEFAULT_OPENAI_SERVICES,
        cli.openai_endpoint,
        cli.openai_transport,
        cli.openai_headers,
        config_routes.remove(ROUTE_OPENAI),
    )? {
        routes.push(route);
    }
    // その他の任意 route (社内 collector・staging 等)。
    for (_, route_cfg) in config_routes {
        routes.push(build_custom_route(route_cfg)?);
    }

    if routes.is_empty() {
        return Ok(None);
    }

    // route 間で service.name が重複していないことを確認。同じ service.name が
    // 複数 route にマッチすると意図しない二重送信・順序不定になるため。
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for route in &routes {
        for svc in &route.service_names {
            if let Some(prev) = seen.insert(svc.clone(), route.name.clone()) {
                anyhow::bail!(
                    "proxy service_names `{svc}` is claimed by both routes `{prev}` and `{cur}`",
                    cur = route.name,
                );
            }
        }
    }

    // proxy を有効化するなら欠測しない前提を守るため JSONL 出力先が必須。
    // 単一ファイル (`--log-file`) と日次ローテーション (`--log-dir`) の両方に対応する。
    if log_sink.is_none() {
        anyhow::bail!(
            "proxy forwarding requires a JSONL outbox (set --log-file or --log-dir); \
             Phase A persists every accepted batch before forwarding, but does not replay it"
        );
    }
    let log_sink = log_sink.expect("checked non-empty above");

    let checkpoint_dir = match cli.checkpoint_dir.or(config.checkpoint_dir) {
        Some(dir) => expand_user_path(dir, home),
        None => default_checkpoint_dir(log_sink),
    };

    Ok(Some(ProxySettings {
        queue_capacity: config
            .queue_capacity
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_PROXY_QUEUE_CAPACITY),
        timeout_ms: config
            .timeout_ms
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_PROXY_TIMEOUT_MS),
        retry_max: config.retry_max.unwrap_or(DEFAULT_PROXY_RETRY_MAX),
        checkpoint_dir,
        routes,
    }))
}

/// Phase B の checkpoint 予約先を JSONL 出力先の隣へ解決する。
/// 単一ファイル出力なら親ディレクトリ、ディレクトリ出力ならそのディレクトリを起点にする。
fn default_checkpoint_dir(log_sink: &LogSink) -> PathBuf {
    let base = match log_sink {
        LogSink::File(path) => path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        LogSink::Directory { dir, .. } => dir.clone(),
    };
    base.join(".otel-logger-proxy")
}

fn build_vendor_route(
    name: &str,
    default_services: &[&str],
    cli_endpoint: Option<String>,
    cli_transport: Option<ProxyTransportArg>,
    cli_headers: Vec<String>,
    config: Option<ProxyRouteConfig>,
) -> anyhow::Result<Option<ProxyRoute>> {
    // CLI で endpoint を指定した場合、config の同名 route を base にしつつ CLI 値で override する。
    let (endpoint, transport, headers_map, signals, service_names) = match (cli_endpoint, config) {
        (Some(endpoint), Some(mut cfg)) => {
            let transport = cli_transport
                .map(ProxyTransport::from)
                .or(cfg.transport.take())
                .unwrap_or_default();
            let mut headers = cfg.headers;
            merge_cli_headers(&mut headers, &cli_headers)?;
            let signals = normalize_signals(cfg.signal_types);
            let service_names = normalize_service_names(cfg.service_names, default_services);
            (endpoint, transport, headers, signals, service_names)
        }
        (Some(endpoint), None) => {
            let mut headers = BTreeMap::new();
            merge_cli_headers(&mut headers, &cli_headers)?;
            let transport = cli_transport.map(ProxyTransport::from).unwrap_or_default();
            (
                endpoint,
                transport,
                headers,
                ProxySignal::ALL.to_vec(),
                default_services.iter().map(|s| (*s).to_string()).collect(),
            )
        }
        (None, Some(mut cfg)) => {
            if cli_transport.is_some() {
                anyhow::bail!(
                    "--proxy-{name}-transport requires --proxy-{name}-endpoint (or a config endpoint)"
                );
            }
            if !cli_headers.is_empty() {
                anyhow::bail!(
                    "--proxy-{name}-header requires --proxy-{name}-endpoint (or a config endpoint)"
                );
            }
            let transport = cfg.transport.take().unwrap_or_default();
            let signals = normalize_signals(cfg.signal_types);
            let service_names = normalize_service_names(cfg.service_names, default_services);
            (cfg.endpoint, transport, cfg.headers, signals, service_names)
        }
        (None, None) => {
            if cli_transport.is_some() || !cli_headers.is_empty() {
                anyhow::bail!(
                    "--proxy-{name}-transport/--proxy-{name}-header require --proxy-{name}-endpoint"
                );
            }
            return Ok(None);
        }
    };

    let headers = resolve_headers(headers_map, name)?;

    Ok(Some(ProxyRoute {
        name: name.to_string(),
        service_names,
        signals,
        transport,
        endpoint,
        headers,
    }))
}

fn build_custom_route(cfg: ProxyRouteConfig) -> anyhow::Result<ProxyRoute> {
    if cfg.endpoint.is_empty() {
        anyhow::bail!("proxy route `{name}` has empty endpoint", name = cfg.name);
    }
    let signals = normalize_signals(cfg.signal_types);
    let service_names = if cfg.service_names.is_empty() {
        anyhow::bail!(
            "custom proxy route `{name}` must specify at least one `service_names` entry",
            name = cfg.name
        );
    } else {
        cfg.service_names
    };
    let headers = resolve_headers(cfg.headers, &cfg.name)?;
    Ok(ProxyRoute {
        name: cfg.name,
        service_names,
        signals,
        transport: cfg.transport.unwrap_or_default(),
        endpoint: cfg.endpoint,
        headers,
    })
}

fn normalize_signals(signals: Vec<ProxySignal>) -> Vec<ProxySignal> {
    if signals.is_empty() {
        ProxySignal::ALL.to_vec()
    } else {
        let mut deduped: Vec<ProxySignal> = signals;
        deduped.sort();
        deduped.dedup();
        deduped
    }
}

fn normalize_service_names(explicit: Vec<String>, defaults: &[&str]) -> Vec<String> {
    if explicit.is_empty() {
        defaults.iter().map(|s| (*s).to_string()).collect()
    } else {
        explicit
    }
}

fn merge_cli_headers(
    dest: &mut BTreeMap<String, String>,
    entries: &[String],
) -> anyhow::Result<()> {
    for entry in entries {
        let (key, value) = entry
            .split_once('=')
            .with_context(|| format!("proxy header `{entry}` must use KEY=VALUE format"))?;
        let key = key.trim();
        if key.is_empty() {
            anyhow::bail!("proxy header `{entry}` has empty key");
        }
        dest.insert(key.to_string(), value.to_string());
    }
    Ok(())
}

/// `env:VAR` prefix を実際の環境変数値に展開する。それ以外は literal として扱う。
/// header 値には制御文字 (改行や CR) を含めない (HTTP header injection 対策)。
fn resolve_headers(
    raw: BTreeMap<String, String>,
    route_name: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let mut resolved = Vec::with_capacity(raw.len());
    for (key, value) in raw {
        validate_header_key(&key, route_name)?;
        let final_value = if let Some(var) = value.strip_prefix("env:") {
            std::env::var(var).with_context(|| {
                format!(
                    "proxy route `{route_name}`: env var `{var}` (referenced by header `{key}`) is not set"
                )
            })?
        } else {
            value
        };
        validate_header_value(&key, &final_value, route_name)?;
        resolved.push((key, final_value));
    }
    Ok(resolved)
}

fn validate_header_key(key: &str, route_name: &str) -> anyhow::Result<()> {
    if key.is_empty() {
        anyhow::bail!("proxy route `{route_name}` has an empty header key");
    }
    // RFC 7230: token = 1*tchar. 制御文字と区切り文字を排除。
    for ch in key.chars() {
        let ok = ch.is_ascii_graphic()
            && !matches!(
                ch,
                '(' | ')'
                    | ','
                    | '/'
                    | ':'
                    | ';'
                    | '<'
                    | '='
                    | '>'
                    | '?'
                    | '@'
                    | '['
                    | '\\'
                    | ']'
                    | '{'
                    | '}'
                    | '"'
            );
        if !ok {
            anyhow::bail!(
                "proxy route `{route_name}` header key `{key}` contains invalid character `{ch}`"
            );
        }
    }
    // Host / Content-Length / Transfer-Encoding を上書きさせないための最低限の denylist。
    // proxy 転送の完全性 (Content-Length ずれ) を守る。
    const DISALLOWED_KEYS: &[&str] = &["host", "content-length", "transfer-encoding"];
    let lower = key.to_ascii_lowercase();
    if DISALLOWED_KEYS.contains(&lower.as_str()) {
        anyhow::bail!(
            "proxy route `{route_name}`: header `{key}` is reserved and cannot be overridden"
        );
    }
    Ok(())
}

fn validate_header_value(key: &str, value: &str, route_name: &str) -> anyhow::Result<()> {
    for (idx, ch) in value.chars().enumerate() {
        // CR/LF を持ち込ませない (header injection 対策)。
        // タブは RFC 7230 で許容だが、それ以外の C0/C1 制御は拒否。
        if ch == '\n' || ch == '\r' {
            anyhow::bail!(
                "proxy route `{route_name}`: header `{key}` contains a CR/LF at position {idx}"
            );
        }
        if (ch.is_control() && ch != '\t') || ch == '\0' {
            anyhow::bail!(
                "proxy route `{route_name}`: header `{key}` contains a control character at position {idx}"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_cli() -> Cli {
        Cli {
            command: None,
            config: None,
            grpc_addr: None,
            http_addr: None,
            log_file: None,
            log_dir: None,
            log_keep_days: None,
            no_stdout: false,
            summary: false,
            color: None,
            dry_run: false,
            proxy_anthropic_endpoint: None,
            proxy_anthropic_transport: None,
            proxy_anthropic_headers: Vec::new(),
            proxy_openai_endpoint: None,
            proxy_openai_transport: None,
            proxy_openai_headers: Vec::new(),
            proxy_checkpoint_dir: None,
        }
    }

    #[test]
    fn merge_defaults_when_nothing_specified() {
        let settings = Settings::merge_with_home(empty_cli(), Config::default(), None).unwrap();
        assert_eq!(settings.grpc_addr.to_string(), "0.0.0.0:4317");
        assert_eq!(settings.http_addr.to_string(), "0.0.0.0:4318");
        assert!(settings.log_sink.is_none());
        assert!(!settings.no_stdout);
        assert!(!settings.summary);
        assert_eq!(settings.color, ColorMode::Auto);
        assert!(!settings.dry_run);
    }

    #[test]
    fn merge_log_file_and_log_dir_are_mutually_exclusive() {
        let mut cli = empty_cli();
        cli.log_file = Some(PathBuf::from("/tmp/a.jsonl"));
        cli.log_dir = Some(PathBuf::from("/tmp/dir"));
        let err = Settings::merge_with_home(cli, Config::default(), None).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn merge_config_log_file_and_log_dir_are_mutually_exclusive() {
        let config = Config {
            log_file: Some(PathBuf::from("/tmp/a.jsonl")),
            log_dir: Some(PathBuf::from("/tmp/dir")),
            ..Config::default()
        };
        let err = Settings::merge_with_home(empty_cli(), config, None).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn merge_log_file_from_cli_uses_file_sink() {
        let mut cli = empty_cli();
        cli.log_file = Some(PathBuf::from("/tmp/a.jsonl"));
        let settings = Settings::merge_with_home(cli, Config::default(), None).unwrap();
        match settings.log_sink {
            Some(LogSink::File(path)) => assert_eq!(path, PathBuf::from("/tmp/a.jsonl")),
            other => panic!("unexpected log_sink: {other:?}"),
        }
    }

    #[test]
    fn merge_log_dir_from_config_uses_directory_sink_with_keep_days() {
        let cli = empty_cli();
        let config = Config {
            log_dir: Some(PathBuf::from("/var/log/otel")),
            log_keep_days: Some(7),
            ..Config::default()
        };
        let settings = Settings::merge_with_home(cli, config, None).unwrap();
        match settings.log_sink {
            Some(LogSink::Directory { dir, keep_days }) => {
                assert_eq!(dir, PathBuf::from("/var/log/otel"));
                assert_eq!(keep_days, 7);
            }
            other => panic!("unexpected log_sink: {other:?}"),
        }
    }

    #[test]
    fn merge_log_dir_falls_back_to_default_keep_days() {
        let cli = empty_cli();
        let config = Config {
            log_dir: Some(PathBuf::from("/var/log/otel")),
            ..Config::default()
        };
        // log_keep_days を未指定にすると DEFAULT_LOG_KEEP_DAYS が使われる。
        let settings = Settings::merge_with_home(cli, config, None).unwrap();
        match settings.log_sink {
            Some(LogSink::Directory { keep_days, .. }) => {
                assert_eq!(keep_days, DEFAULT_LOG_KEEP_DAYS);
            }
            other => panic!("unexpected log_sink: {other:?}"),
        }
    }

    #[test]
    fn merge_cli_overrides_config_log_file() {
        let mut cli = empty_cli();
        cli.log_file = Some(PathBuf::from("/cli.jsonl"));
        let config = Config {
            log_file: Some(PathBuf::from("/config.jsonl")),
            ..Config::default()
        };
        let settings = Settings::merge_with_home(cli, config, None).unwrap();
        match settings.log_sink {
            Some(LogSink::File(path)) => assert_eq!(path, PathBuf::from("/cli.jsonl")),
            other => panic!("unexpected log_sink: {other:?}"),
        }
    }

    #[test]
    fn merge_cli_log_file_overrides_config_log_dir() {
        let mut cli = empty_cli();
        cli.log_file = Some(PathBuf::from("/cli.jsonl"));
        let config = Config {
            log_dir: Some(PathBuf::from("/config-dir")),
            ..Config::default()
        };
        let settings = Settings::merge_with_home(cli, config, None).unwrap();
        match settings.log_sink {
            Some(LogSink::File(path)) => assert_eq!(path, PathBuf::from("/cli.jsonl")),
            other => panic!("unexpected log_sink: {other:?}"),
        }
    }

    #[test]
    fn merge_cli_log_dir_overrides_config_log_file() {
        let mut cli = empty_cli();
        cli.log_dir = Some(PathBuf::from("/cli-dir"));
        let config = Config {
            log_file: Some(PathBuf::from("/config.jsonl")),
            ..Config::default()
        };
        let settings = Settings::merge_with_home(cli, config, None).unwrap();
        match settings.log_sink {
            Some(LogSink::Directory { dir, .. }) => assert_eq!(dir, PathBuf::from("/cli-dir")),
            other => panic!("unexpected log_sink: {other:?}"),
        }
    }

    #[test]
    fn merge_boolean_flags_or_together() {
        // CLI=false でも Config=true なら有効化される。
        let cli = empty_cli();
        let config = Config {
            no_stdout: Some(true),
            summary: Some(true),
            ..Config::default()
        };
        let settings = Settings::merge_with_home(cli, config, None).unwrap();
        assert!(settings.no_stdout);
        assert!(settings.summary);
    }

    #[test]
    fn merge_expands_tilde_in_config_log_dir() {
        // config 経由のチルダ展開 (実際に発生していたバグの回帰テスト)。
        let cli = empty_cli();
        let config = Config {
            log_dir: Some(PathBuf::from("~/tmp")),
            ..Config::default()
        };
        let home = PathBuf::from("/Users/alice");
        let settings = Settings::merge_with_home(cli, config, Some(&home)).unwrap();
        match settings.log_sink {
            Some(LogSink::Directory { dir, .. }) => {
                assert_eq!(dir, PathBuf::from("/Users/alice/tmp"));
            }
            other => panic!("unexpected log_sink: {other:?}"),
        }
    }

    #[test]
    fn merge_expands_tilde_in_cli_log_file() {
        let mut cli = empty_cli();
        cli.log_file = Some(PathBuf::from("~/logs/otel.jsonl"));
        let home = PathBuf::from("/Users/alice");
        let settings = Settings::merge_with_home(cli, Config::default(), Some(&home)).unwrap();
        match settings.log_sink {
            Some(LogSink::File(path)) => {
                assert_eq!(path, PathBuf::from("/Users/alice/logs/otel.jsonl"));
            }
            other => panic!("unexpected log_sink: {other:?}"),
        }
    }

    #[test]
    fn color_mode_auto_disables_when_no_color_env_is_set() {
        // SAFETY: テスト中の単純な env 操作で、process scope は隔離されていない点に注意。
        // 並列実行されうるため、固有の env 名にして他テストと衝突しないようにすべきだが、
        // ここでは NO_COLOR の存在判定ロジックそのものを検証する。
        unsafe { std::env::set_var("NO_COLOR", "1") };
        assert!(!ColorMode::Auto.enabled_for_stdout());
        unsafe { std::env::remove_var("NO_COLOR") };
    }

    #[test]
    fn color_mode_always_and_never_ignore_terminal_state() {
        assert!(ColorMode::Always.enabled_for_stdout());
        assert!(!ColorMode::Never.enabled_for_stdout());
    }

    // ----- proxy 統合テスト -----

    fn cli_with_log_file() -> Cli {
        let mut cli = empty_cli();
        cli.log_file = Some(PathBuf::from("/tmp/otel.jsonl"));
        cli
    }

    #[test]
    fn merge_proxy_defaults_to_none_without_endpoints() {
        let settings =
            Settings::merge_with_home(cli_with_log_file(), Config::default(), None).unwrap();
        assert!(settings.proxy.is_none());
    }

    #[test]
    fn merge_proxy_requires_jsonl_outbox() {
        // proxy を指定したのに --log-file / --log-dir が無いと startup で reject。
        let mut cli = empty_cli();
        cli.proxy_anthropic_endpoint = Some("https://collector.example:4317".to_string());
        let err = Settings::merge_with_home(cli, Config::default(), None).unwrap_err();
        assert!(
            err.to_string().contains("JSONL outbox"),
            "expected outbox requirement error, got: {err}"
        );
    }

    #[test]
    fn merge_proxy_anthropic_endpoint_uses_builtin_service_names() {
        let mut cli = cli_with_log_file();
        cli.proxy_anthropic_endpoint = Some("https://collector.example:4317".to_string());
        let settings = Settings::merge_with_home(cli, Config::default(), None).unwrap();
        let proxy = settings.proxy.expect("proxy should be configured");
        assert_eq!(proxy.routes.len(), 1);
        let route = &proxy.routes[0];
        assert_eq!(route.name, "anthropic");
        assert_eq!(route.service_names, vec!["claude-code".to_string()]);
        assert_eq!(route.transport, ProxyTransport::Grpc);
        assert_eq!(route.endpoint, "https://collector.example:4317");
    }

    #[test]
    fn merge_proxy_openai_endpoint_uses_codex_service_names() {
        let mut cli = cli_with_log_file();
        cli.proxy_openai_endpoint = Some("https://collector.example".to_string());
        cli.proxy_openai_transport = Some(ProxyTransportArg::HttpProtobuf);
        let settings = Settings::merge_with_home(cli, Config::default(), None).unwrap();
        let proxy = settings.proxy.expect("proxy should be configured");
        assert_eq!(proxy.routes.len(), 1);
        let route = &proxy.routes[0];
        assert_eq!(route.name, "openai");
        assert_eq!(
            route.service_names,
            vec![
                "codex_cli_rs".to_string(),
                "codex_exec".to_string(),
                "codex-app-server".to_string(),
            ]
        );
        assert_eq!(route.transport, ProxyTransport::HttpProtobuf);
    }

    #[test]
    fn merge_proxy_headers_resolve_env_prefix() {
        let key = "OTEL_LOGGER_TEST_TOKEN";
        // SAFETY: process 内でしか使わない test 用 env。
        unsafe { std::env::set_var(key, "s3cret-value") };
        let mut cli = cli_with_log_file();
        cli.proxy_anthropic_endpoint = Some("https://collector.example:4317".to_string());
        cli.proxy_anthropic_headers = vec![
            format!("Authorization=env:{key}"),
            "X-Tenant=corp".to_string(),
        ];
        let settings = Settings::merge_with_home(cli, Config::default(), None).unwrap();
        let proxy = settings.proxy.expect("proxy should be configured");
        let route = &proxy.routes[0];
        let headers: BTreeMap<_, _> = route.headers.iter().cloned().collect();
        assert_eq!(
            headers.get("Authorization"),
            Some(&"s3cret-value".to_string())
        );
        assert_eq!(headers.get("X-Tenant"), Some(&"corp".to_string()));
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn merge_proxy_rejects_missing_env_var() {
        let mut cli = cli_with_log_file();
        cli.proxy_anthropic_endpoint = Some("https://collector.example:4317".to_string());
        cli.proxy_anthropic_headers =
            vec!["Authorization=env:OTEL_LOGGER_NEVER_SET_TOKEN_XYZ".to_string()];
        let err = Settings::merge_with_home(cli, Config::default(), None).unwrap_err();
        assert!(
            err.to_string().contains("OTEL_LOGGER_NEVER_SET_TOKEN_XYZ"),
            "expected missing env var to be surfaced, got: {err}"
        );
    }

    #[test]
    fn merge_proxy_rejects_control_char_in_header_value() {
        let mut cli = cli_with_log_file();
        cli.proxy_anthropic_endpoint = Some("https://collector.example:4317".to_string());
        cli.proxy_anthropic_headers = vec!["X-Bad=line1\nline2".to_string()];
        let err = Settings::merge_with_home(cli, Config::default(), None).unwrap_err();
        assert!(
            err.to_string().contains("CR/LF"),
            "expected CR/LF rejection, got: {err}"
        );
    }

    #[test]
    fn merge_proxy_reserved_header_is_rejected() {
        let mut cli = cli_with_log_file();
        cli.proxy_anthropic_endpoint = Some("https://collector.example:4317".to_string());
        cli.proxy_anthropic_headers = vec!["Content-Length=999".to_string()];
        let err = Settings::merge_with_home(cli, Config::default(), None).unwrap_err();
        assert!(err.to_string().contains("reserved"), "got: {err}");
    }

    #[test]
    fn merge_proxy_config_route_overrides_service_names() {
        use crate::config::ProxyConfig;
        let cli = cli_with_log_file();
        let config = Config {
            proxy: Some(ProxyConfig {
                routes: vec![ProxyRouteConfig {
                    name: "anthropic".to_string(),
                    service_names: vec![
                        "claude-code".to_string(),
                        "claude-experimental".to_string(),
                    ],
                    signal_types: vec![],
                    transport: Some(ProxyTransport::HttpProtobuf),
                    endpoint: "https://custom.example".to_string(),
                    headers: BTreeMap::new(),
                }],
                ..ProxyConfig::default()
            }),
            ..Config::default()
        };
        let settings = Settings::merge_with_home(cli, config, None).unwrap();
        let proxy = settings.proxy.expect("proxy should be configured");
        assert_eq!(proxy.routes.len(), 1);
        let route = &proxy.routes[0];
        assert_eq!(
            route.service_names,
            vec!["claude-code".to_string(), "claude-experimental".to_string()],
        );
        assert_eq!(route.transport, ProxyTransport::HttpProtobuf);
        assert_eq!(route.endpoint, "https://custom.example");
    }

    #[test]
    fn merge_proxy_service_name_conflict_is_rejected() {
        // 別 route 同士で同じ service.name を主張したら error。
        use crate::config::ProxyConfig;
        let cli = cli_with_log_file();
        let config = Config {
            proxy: Some(ProxyConfig {
                routes: vec![
                    ProxyRouteConfig {
                        name: "anthropic".to_string(),
                        service_names: vec!["claude-code".to_string()],
                        signal_types: vec![],
                        transport: None,
                        endpoint: "https://a.example".to_string(),
                        headers: BTreeMap::new(),
                    },
                    ProxyRouteConfig {
                        name: "conflicting".to_string(),
                        service_names: vec!["claude-code".to_string()],
                        signal_types: vec![],
                        transport: None,
                        endpoint: "https://b.example".to_string(),
                        headers: BTreeMap::new(),
                    },
                ],
                ..ProxyConfig::default()
            }),
            ..Config::default()
        };
        let err = Settings::merge_with_home(cli, config, None).unwrap_err();
        assert!(err.to_string().contains("claude-code"), "got: {err}");
    }
}
