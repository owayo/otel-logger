use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::cli::Settings;
use crate::grpc::OtlpService;
use crate::http;
use crate::sink::Sink;

/// OTLP/gRPC・OTLP/HTTP が 1 リクエストあたりに decode を許す最大バイト数。
///
/// 受信した payload を欠落なく保存する方針上、tonic 既定の 4MiB / axum 既定の 2MiB では
/// 大きな batch が `RESOURCE_EXHAUSTED` / `413 Payload Too Large` で恒久的に拒否され、
/// exporter が retry しても同じサイズのため回復できず欠落する。実測の最大 batch (約 0.6MiB)
/// に十分な余裕を取りつつ、`0.0.0.0` 公開 bind 時のメモリ枯渇を避けるため 32MiB を上限とする。
pub const OTLP_MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;

/// graceful shutdown で in-flight request の完了を待つ上限。
///
/// この猶予を超えたら listener task を abort して `sink.flush()` へ進む。取りこぼす
/// のは「猶予内に送り切れなかった batch」だけで、OTLP 的には ACK していないため
/// exporter 側が retry できる。逆にここで無制限に待つと JSONL の `sync_all` に
/// 到達せず、確定済みの telemetry まで失う。
///
/// 10s なのは launchd の既定停止猶予 (20s) に対して `sync_all` 用の余白を半分残すため。
/// Kubernetes の既定 `terminationGracePeriodSeconds` (30s) でも十分間に合う。
/// flush 自体には timeout を掛けない (`spawn_blocking` の `sync_all` は future を
/// abort しても syscall が止まらず、「成功したことにする」方が危険)。
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// 1 接続あたりに許す同時 HTTP/2 ストリーム数 (= 並行 OTLP リクエスト数)。
///
/// 1 リクエストの上限 (`OTLP_MAX_REQUEST_BYTES`) だけでは総メモリを抑えられない。
/// axum (hyper) 側は既定 200 で頭打ちになるが、tonic は `None` を明示して既定を
/// 打ち消すため、設定しないと gRPC だけが真に無制限になる。
const MAX_CONCURRENT_STREAMS: u32 = 16;

/// `shutdown` が cancel されるまで、`addr` で OTLP/gRPC server を動かす。
pub async fn serve_grpc(addr: SocketAddr, sink: Sink, shutdown: CancellationToken) -> Result<()> {
    let (trace_srv, metrics_srv, logs_srv) = OtlpService::new(sink).into_servers();
    // 既定 (4MiB) では大きな batch が decode 前に拒否されるため、上限を明示的に引き上げる。
    // gzip も受け付ける。`OTEL_EXPORTER_OTLP_COMPRESSION=gzip` の exporter が gRPC で
    // 送ると、対応していなければ handler 到達前に `Unimplemented` が返る。これは
    // 非 retryable なので、OTLP/HTTP の 400 と同じく batch が恒久的に失われる。
    // size 上限は展開後に効くため 32MiB 制限は維持される。
    let gzip = tonic::codec::CompressionEncoding::Gzip;
    let trace_srv = trace_srv
        .accept_compressed(gzip)
        .max_decoding_message_size(OTLP_MAX_REQUEST_BYTES);
    let metrics_srv = metrics_srv
        .accept_compressed(gzip)
        .max_decoding_message_size(OTLP_MAX_REQUEST_BYTES);
    let logs_srv = logs_srv
        .accept_compressed(gzip)
        .max_decoding_message_size(OTLP_MAX_REQUEST_BYTES);
    tracing::info!(%addr, max_request_bytes = OTLP_MAX_REQUEST_BYTES, "OTLP/gRPC server listening");
    tonic::transport::Server::builder()
        // tonic は hyper の既定 (200 streams/connection) を `None` で打ち消すため、
        // 明示しないと 1 本の TCP 接続から無制限にストリームを開ける。1 リクエストの
        // 上限は 32MiB でも総量が抑えられず、`0.0.0.0` 公開 bind でメモリ枯渇する。
        // 実測 batch (約 0.6MiB) なら 16 並列でも十分な throughput が出る。
        .max_concurrent_streams(Some(MAX_CONCURRENT_STREAMS))
        .concurrency_limit_per_connection(MAX_CONCURRENT_STREAMS as usize)
        // 応答しない peer の接続を掴んだままにしない (shutdown を人質に取られる)。
        .http2_keepalive_interval(Some(std::time::Duration::from_secs(30)))
        .http2_keepalive_timeout(Some(std::time::Duration::from_secs(10)))
        // `timeout()` と `load_shed()` は付けない。前者は JSONL 保存後・ACK 前に
        // 発火すると exporter の retry で二重計上を招き、後者は `Overloaded` が
        // `Status` に変換されず接続レベルのエラーとして漏れる。
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
///
/// `sink` は既に proxy router を装着済みでも未装着でも受け付ける。ここでは受信 endpoint
/// と proxy worker の shutdown を同じ `CancellationToken` で束ねる。
pub async fn run(settings: Settings, sink: Sink) -> Result<()> {
    let shutdown = CancellationToken::new();

    // proxy worker が spawn 済みなら (Sink が router を持っていれば) 、shutdown 時に
    // join できるように handle を持っておく。ここでは proxy handle は main が持っており、
    // sink 側は router 参照のみ。worker join は main で行う。

    let mut grpc_handle = {
        let sink = sink.clone();
        let token = shutdown.clone();
        let addr = settings.grpc_addr;
        tokio::spawn(async move { serve_grpc(addr, sink, token).await })
    };
    let grpc_id = grpc_handle.id();
    let mut http_handle = {
        let sink = sink.clone();
        let token = shutdown.clone();
        let addr = settings.http_addr;
        tokio::spawn(async move { http::serve(addr, sink, token).await })
    };
    let http_id = http_handle.id();

    // 2 回目のシグナルで in-flight を諦めるための token。tokio の signal handler は
    // プロセスグローバルに既定動作を置き換えてしまうため、ここで受け直さないと
    // 2 回目の SIGTERM / SIGINT も無視され、SIGKILL 以外に落とす手段が無くなる。
    let hard_exit = CancellationToken::new();
    let signal_token = shutdown.clone();
    let hard_exit_signal = hard_exit.clone();
    let signal_handle = tokio::spawn(async move {
        shutdown_signal().await;
        signal_token.cancel();
        shutdown_signal().await;
        tracing::warn!("second shutdown signal received; abandoning in-flight connections");
        hard_exit_signal.cancel();
    });

    let grpc_abort = grpc_handle.abort_handle();
    let http_abort = http_handle.abort_handle();
    let mut grpc_done = false;
    let mut http_done = false;
    let mut result = tokio::select! {
        res = &mut grpc_handle => {
            grpc_done = true;
            shutdown.cancel();
            join_result(grpc_id, res).context("gRPC task")
        }
        res = &mut http_handle => {
            http_done = true;
            shutdown.cancel();
            join_result(http_id, res).context("HTTP task")
        }
        _ = shutdown.cancelled() => {
            shutdown.cancel();
            Ok(())
        },
    };

    // 残っている listener task の終了を待つ。axum / tonic の graceful shutdown は
    // in-flight request の完了を無制限に待つため、body を送り切らない接続が 1 本でも
    // 残ると SIGTERM で終了できない。既定 bind は `0.0.0.0` なので外部から誘発できる。
    // ここで止まると末尾の `sink.flush()` (= `--log-file` 経路の `sync_all`) に到達せず、
    // SIGKILL された時点で未確定分が失われる。
    let drain = async {
        let mut drained: Result<()> = Ok(());
        if !grpc_done {
            merge_task_result(
                &mut drained,
                wait_for(grpc_id, grpc_handle).await.context("gRPC task"),
            );
        }
        if !http_done {
            merge_task_result(
                &mut drained,
                wait_for(http_id, http_handle).await.context("HTTP task"),
            );
        }
        drained
    };
    tokio::select! {
        drained = drain => merge_task_result(&mut result, drained),
        _ = hard_exit.cancelled() => {
            grpc_abort.abort();
            http_abort.abort();
        }
        _ = tokio::time::sleep(SHUTDOWN_GRACE) => {
            tracing::warn!(
                grace_secs = SHUTDOWN_GRACE.as_secs(),
                "graceful shutdown timed out; aborting in-flight connections so the JSONL sink \
                 can still be flushed"
            );
            grpc_abort.abort();
            http_abort.abort();
        }
    }

    signal_handle.abort();
    // flush の失敗で `?` early return すると、gRPC / HTTP task 側のエラー (bind 失敗や
    // panic) が捨てられて診断の第一報が消える。他の合流と同じく merge する。
    merge_task_result(&mut result, sink.flush().await.context("flush JSONL sink"));
    result
}

async fn wait_for(id: tokio::task::Id, handle: tokio::task::JoinHandle<Result<()>>) -> Result<()> {
    join_result(id, handle.await)
}

fn join_result(
    id: tokio::task::Id,
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    match result {
        Ok(Ok(())) => {
            tracing::info!(?id, "task completed");
            Ok(())
        }
        Ok(Err(e)) => Err(e),
        Err(e) => Err(anyhow::anyhow!("task panicked: {e}")),
    }
}

fn merge_task_result(result: &mut Result<()>, next: Result<()>) {
    if result.is_ok() {
        *result = next;
    } else if let Err(e) = next {
        tracing::warn!(error = %e, "additional server task failed during shutdown");
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

/// gRPC / HTTP の両 listener を同時に bind できるか確認する。
/// 片方ずつ bind/drop すると同じ固定 port の衝突を見落とすため、両方を保持したまま検証する。
pub async fn probe_binds(
    grpc_addr: SocketAddr,
    http_addr: SocketAddr,
) -> Result<(SocketAddr, SocketAddr)> {
    let grpc_listener = TcpListener::bind(grpc_addr)
        .await
        .with_context(|| format!("bind gRPC {grpc_addr}"))?;
    let grpc = grpc_listener.local_addr()?;
    let http_listener = TcpListener::bind(http_addr)
        .await
        .with_context(|| format!("bind HTTP {http_addr}"))?;
    let http = http_listener.local_addr()?;
    drop(http_listener);
    drop(grpc_listener);
    Ok((grpc, http))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn probe_bind_returns_bound_addr() {
        let requested = "127.0.0.1:0".parse().unwrap();
        let bound = probe_bind(requested).await.unwrap();

        assert_ne!(bound.port(), 0, "ephemeral port が割り当てられること");
    }

    #[tokio::test]
    async fn probe_binds_rejects_same_fixed_port() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let result = probe_binds(addr, addr).await;

        assert!(result.is_err(), "同じ固定 port は dry-run でも失敗すること");
    }
}
