use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::cli::Settings;
use crate::grpc::OtlpService;
use crate::http;
use crate::sink::Sink;

/// `shutdown` が cancel されるまで、`addr` で OTLP/gRPC server を動かす。
pub async fn serve_grpc(addr: SocketAddr, sink: Sink, shutdown: CancellationToken) -> Result<()> {
    let (trace_srv, metrics_srv, logs_srv) = OtlpService::new(sink).into_servers();
    tracing::info!(%addr, "OTLP/gRPC server listening");
    tonic::transport::Server::builder()
        .add_service(trace_srv)
        .add_service(metrics_srv)
        .add_service(logs_srv)
        .serve_with_shutdown(addr, async move { shutdown.cancelled().await })
        .await
        .context("OTLP/gRPC server")?;
    Ok(())
}

/// SIGINT または (Unix では) SIGTERM を待つ。Docker/Kubernetes などの container runtime は
/// shutdown 時に SIGTERM を送るため、SIGTERM も扱う必要がある。
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %e, "ctrl_c handler failed");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "SIGTERM handler failed");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT"),
        _ = terminate => tracing::info!("received SIGTERM"),
    }
}

/// cancellation を考慮しながら gRPC + HTTP listener をまとめて動かす。
pub async fn run(settings: Settings, sink: Sink) -> Result<()> {
    let shutdown = CancellationToken::new();

    let grpc_handle = {
        let sink = sink.clone();
        let token = shutdown.clone();
        let addr = settings.grpc_addr;
        tokio::spawn(async move { serve_grpc(addr, sink, token).await })
    };
    let http_handle = {
        let sink = sink.clone();
        let token = shutdown.clone();
        let addr = settings.http_addr;
        tokio::spawn(async move { http::serve(addr, sink, token).await })
    };

    let signal_token = shutdown.clone();
    let signal_handle = tokio::spawn(async move {
        shutdown_signal().await;
        signal_token.cancel();
    });

    let result = tokio::select! {
        res = wait_for(grpc_handle.id(), grpc_handle) => {
            shutdown.cancel();
            res.context("gRPC task")
        }
        res = wait_for(http_handle.id(), http_handle) => {
            shutdown.cancel();
            res.context("HTTP task")
        }
        _ = shutdown.cancelled() => Ok(()),
    };

    signal_handle.abort();
    sink.flush().await?;
    result
}

async fn wait_for(id: tokio::task::Id, handle: tokio::task::JoinHandle<Result<()>>) -> Result<()> {
    match handle.await {
        Ok(Ok(())) => {
            tracing::info!(?id, "task completed");
            Ok(())
        }
        Ok(Err(e)) => Err(e),
        Err(e) => Err(anyhow::anyhow!("task panicked: {e}")),
    }
}

/// `TcpListener` を bind してすぐ閉じる。dry-run で address が実際に使えるか確認し、
/// 成功扱いにする前に port 使用中、権限不足、address 解決失敗を検出する。
pub async fn probe_bind(addr: SocketAddr) -> Result<SocketAddr> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    let local = listener.local_addr()?;
    drop(listener);
    Ok(local)
}
