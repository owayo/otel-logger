use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use otel_logger::cli::{Cli, Commands, Settings};
use otel_logger::config::{self, Config, InitOutcome, InitProfile};
use otel_logger::forward;
use otel_logger::path::expand_current_user_path;
use otel_logger::server;
use otel_logger::sink::Sink;
use tokio_util::sync::CancellationToken;

fn main() -> Result<()> {
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

    runtime.block_on(async move {
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
    })
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
         (`--log-keep-days` only applies to JSONL). \
         Pass `--no-stdout` (or set `no-stdout = true`) for unattended runs; \
         `otel-logger init --daemon` generates such a config. \
         / stdout が端末ではない状態で、人が読める出力が有効です。\
         otel-logger は stdout をローテーションしないため、リダイレクト先が際限なく\
         肥大化します (`--log-keep-days` は JSONL にしか適用されません)。\
         常駐運用では `--no-stdout` (または設定ファイルの `no-stdout = true`) を\
         使ってください。`otel-logger init --daemon` がその設定を生成します。"
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
