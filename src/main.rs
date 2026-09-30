use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use otel_logger::cli::{Cli, Commands, Settings};
use otel_logger::config::{self, Config, InitOutcome, InitProfile};
use otel_logger::forward;
use otel_logger::path::expand_current_user_path;
use otel_logger::server;
use otel_logger::sink::Sink;
use tokio_util::sync::CancellationToken;

/// 受信処理を終えた後、残っている blocking task の完了を待つ上限。
///
/// `Runtime` を drop すると `spawn_blocking` の完了を無期限に待つ。読み手が止まった
/// stdout の write(2) で人が読める出力の writer が止まっていると、SIGTERM を受けても
/// プロセスが終わらず、launchd / systemd の SIGKILL まで居座る。JSONL の flush と fsync は
/// `block_on` の中で完了しているので、ここで打ち切っても永続化は失われない。
const BLOCKING_TASK_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

fn main() -> Result<()> {
    clear_empty_otel_logger_env();
    let cli = Cli::parse();
    init_tracing();

    if let Some(Commands::Init {
        path,
        force,
        daemon,
    }) = cli.command.as_ref()
    {
        let profile = if *daemon {
            InitProfile::Daemon
        } else {
            InitProfile::Default
        };
        return run_init(path.as_deref(), *force, profile);
    }

    let config = Config::load(cli.config.as_deref())?;
    let settings = Settings::merge(cli, config)?;
    warn_if_stdout_unrotated(&settings);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let result = runtime.block_on(async move {
        // proxy 転送が設定されていれば router + worker を先に起動し、Sink に装着する。
        // shutdown token は server と共有し、Ctrl-C で worker も止める。
        let proxy_shutdown = CancellationToken::new();
        let proxy_handle = if let Some(proxy_cfg) = settings.proxy.as_ref() {
            match forward::spawn_router(proxy_cfg, proxy_shutdown.clone()).await {
                Ok(handle) => Some(handle),
                Err(e) => {
                    tracing::error!(error = %e, "failed to start OTLP proxy workers");
                    return Err(e);
                }
            }
        } else {
            None
        };

        let router = proxy_handle.as_ref().map(|h| h.router());
        let sink = Sink::from_settings_with_proxy(&settings, router).await?;

        if settings.dry_run {
            let (grpc, http) = server::probe_binds(settings.grpc_addr, settings.http_addr).await?;
            tracing::info!(
                grpc = %grpc,
                http = %http,
                log_sink = ?settings.log_sink,
                pretty_log_dir = ?settings.pretty_log_dir,
                "dry run: probed both listeners successfully, exiting"
            );
            sink.flush().await?;
            // dry run でも worker は shutdown を通知して join。
            proxy_shutdown.cancel();
            if let Some(handle) = proxy_handle {
                handle.join().await;
            }
            return Ok(());
        }

        let run_result = server::run(settings, sink).await;
        // server が終了したので proxy worker にも shutdown を通知して join する。
        proxy_shutdown.cancel();
        if let Some(handle) = proxy_handle {
            handle.join().await;
        }
        run_result
    });
    runtime.shutdown_timeout(BLOCKING_TASK_SHUTDOWN_GRACE);
    result
}

/// stdout がローテーションされないまま増え続ける構成なら、起動時に一度だけ警告する。
///
/// 警告は stderr (tracing) に出す。stdout へ書くと、抑止したい当の出力に混ざるため。
/// `--dry-run` でも出す。CI の smoke test で常駐構成の取り違えに気づけるようにするため。
fn warn_if_stdout_unrotated(settings: &Settings) {
    // TTY 判定は ColorMode::enabled_for_stdout と同じ経路に揃える。
    let stdout_is_terminal = is_terminal::IsTerminal::is_terminal(&std::io::stdout());
    if !settings.warns_unrotated_stdout(stdout_is_terminal) {
        return;
    }
    tracing::warn!(
        "stdout is not a terminal and human-readable output is enabled. \
         otel-logger never rotates stdout, so the redirect target grows without bound \
         (`--log-keep-days` only applies to the daily files in `--log-dir`). \
         Pass `--no-stdout` (or set `no-stdout = true`) for unattended runs; \
         to keep the human-readable output, also pass `--pretty-log` (or set \
         `pretty-log = true`) to write it as daily-rotated files in `--log-dir`. \
         `otel-logger init --daemon` generates a config for unattended runs. \
         / stdout が端末ではない状態で、人が読める出力が有効です。\
         otel-logger は stdout をローテーションしないため、リダイレクト先が際限なく\
         肥大化します (`--log-keep-days` は `--log-dir` 内の日次ファイルにしか適用されません)。\
         常駐運用では `--no-stdout` (または設定ファイルの `no-stdout = true`) を\
         使ってください。人が読める出力を残したい場合は、`--pretty-log` (または\
         `pretty-log = true`) も指定すると `--log-dir` に日次ローテーション付きのファイルとして\
         書き出します。`otel-logger init --daemon` が常駐運用向けの設定を生成します。"
    );
}

fn run_init(path: Option<&Path>, force: bool, profile: InitProfile) -> Result<()> {
    let dest: PathBuf = match path {
        Some(p) => expand_current_user_path(p.to_path_buf()),
        None => config::default_config_path().context(
            "cannot determine default config path: set $XDG_CONFIG_HOME or $HOME, or pass --path",
        )?,
    };
    let outcome = config::write_with_profile(&dest, force, profile)?;
    let verb = match outcome {
        InitOutcome::Created => "Created",
        InitOutcome::Overwrote => "Overwrote",
    };
    println!("{verb} config file: {}", dest.display());
    Ok(())
}

/// 空文字の `OTEL_LOGGER_*` 環境変数を未設定として扱う。
///
/// clap は空文字でも「指定あり」とみなすため、`OTEL_LOGGER_LOG_FILE=` のような
/// 空値を並べた docker-compose の `environment:` / systemd の `Environment=` では
/// `a value is required for '--log-file <PATH>'` になって常駐プロセスが上がらない。
/// `OTEL_LOGGER_PROXY_*_HEADERS=` に至っては「header を指定した」と誤判定されて
/// endpoint を要求する。`config.rs` の `XDG_CONFIG_HOME` / `HOME` や `path.rs` の
/// `~` 展開が空文字を未設定として扱うのと挙動を揃える。
fn clear_empty_otel_logger_env() {
    let empty: Vec<std::ffi::OsString> = std::env::vars_os()
        .filter(|(key, value)| {
            value.is_empty() && key.to_string_lossy().starts_with("OTEL_LOGGER_")
        })
        .map(|(key, _)| key)
        .collect();
    for key in empty {
        // SAFETY: main の先頭、tokio runtime も追加スレッドも起こす前に呼んでいるため、
        // 環境変数を触っている他スレッドは存在しない。
        unsafe { std::env::remove_var(&key) };
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt;

    let filter = EnvFilter::try_from_env("OTEL_LOGGER_LOG")
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}
