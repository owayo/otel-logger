use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use otel_logger::cli::{Cli, Commands, Settings};
use otel_logger::config::{self, Config, InitOutcome};
use otel_logger::path::expand_current_user_path;
use otel_logger::server;
use otel_logger::sink::Sink;

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    if let Some(Commands::Init { path, force }) = cli.command.as_ref() {
        return run_init(path.as_deref(), *force);
    }

    let config = Config::load(cli.config.as_deref())?;
    let settings = Settings::merge(cli, config)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let sink = Sink::from_settings(&settings).await?;

        if settings.dry_run {
            let (grpc, http) = server::probe_binds(settings.grpc_addr, settings.http_addr).await?;
            tracing::info!(
                grpc = %grpc,
                http = %http,
                log_sink = ?settings.log_sink,
                "dry run: probed both listeners successfully, exiting"
            );
            sink.flush().await?;
            return Ok(());
        }

        server::run(settings, sink).await
    })
}

fn run_init(path: Option<&Path>, force: bool) -> Result<()> {
    let dest: PathBuf = match path {
        Some(p) => expand_current_user_path(p.to_path_buf()),
        None => config::default_config_path().context(
            "cannot determine default config path: set $XDG_CONFIG_HOME or $HOME, or pass --path",
        )?,
    };
    let outcome = config::write_default(&dest, force)?;
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
