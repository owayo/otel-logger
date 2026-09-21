//! route ごとの worker task。ExportRequest を受け取り、retry しながら送信する。
//!
//! shutdown token が cancel されるか channel が close されたら終了する。

use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::client::RouteClient;
use super::{ExportRequest, RouteMetricsHandle};

/// shutdown 後に queue を送り切るために与える猶予。
///
/// 猶予なしで捨てると、上流の応答より ingest が速い通常運用でも、再起動のたびに
/// route あたり最大 queue 容量ぶんの batch が上流へ届かない。Phase B の
/// JSONL catch-up が入るまでは worker 再起動後の回収経路が無いため、そのまま
/// 上流から見た欠測になる。
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// worker 起動に必要な設定。
pub(super) struct WorkerConfig {
    pub route_name: String,
    pub client: RouteClient,
    pub receiver: mpsc::Receiver<ExportRequest>,
    pub metrics: RouteMetricsHandle,
    pub retry_max: u32,
    pub shutdown: CancellationToken,
}

pub(super) fn spawn(cfg: WorkerConfig) -> JoinHandle<()> {
    tokio::spawn(async move { run(cfg).await })
}

async fn run(mut cfg: WorkerConfig) {
    tracing::info!(route = %cfg.route_name, "proxy worker started");
    loop {
        tokio::select! {
            biased;
            _ = cfg.shutdown.cancelled() => {
                tracing::info!(route = %cfg.route_name, "proxy worker: shutdown requested");
                break;
            }
            maybe = cfg.receiver.recv() => {
                let Some(request) = maybe else {
                    tracing::info!(route = %cfg.route_name, "proxy worker: channel closed");
                    break;
                };
                if request.is_empty() {
                    continue;
                }
                match send_with_retry(
                    &cfg.route_name,
                    &cfg.client,
                    &request,
                    cfg.retry_max,
                    &cfg.shutdown,
                )
                .await
                {
                    Ok(()) => {
                        cfg.metrics.sent_total.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        cfg.metrics.failed_total.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(route = %cfg.route_name, error = %e, "proxy send failed after retries");
                    }
                }
            }
        }
    }

    // 新規 notify は Closed として drop 計上させ、既に queue にある分だけを対象にする。
    cfg.receiver.close();
    drain_remaining(&mut cfg).await;
    tracing::info!(route = %cfg.route_name, "proxy worker stopped");
}

/// shutdown 後に queue へ残った batch を `DRAIN_GRACE` の範囲で送り切る。
///
/// 期限を過ぎた分は `dropped_total` に計上する。計上しないと
/// `sent + failed + dropped` が notify 総数と合わなくなり、「何件を上流へ渡せなかったか」
/// を運用側から観測できなくなる。payload 自体は JSONL に残っている。
async fn drain_remaining(cfg: &mut WorkerConfig) {
    // shutdown token をそのまま送信側へ渡すと即 abort されて 1 件も送れない。
    // drain 専用の期限 token を使う。
    let deadline = CancellationToken::new();
    let timer = {
        let deadline = deadline.clone();
        tokio::spawn(async move {
            tokio::time::sleep(DRAIN_GRACE).await;
            deadline.cancel();
        })
    };

    let mut drained: u64 = 0;
    let mut abandoned: u64 = 0;
    while let Ok(request) = cfg.receiver.try_recv() {
        if request.is_empty() {
            continue;
        }
        if deadline.is_cancelled() {
            abandoned += 1;
            continue;
        }
        // 期限内に 1 件でも多く捌くため、drain 中は retry しない
        // (1 件の backoff で残り全部を落とす方が損失が大きい)。
        match send_with_retry(&cfg.route_name, &cfg.client, &request, 0, &deadline).await {
            Ok(()) => {
                cfg.metrics.sent_total.fetch_add(1, Ordering::Relaxed);
                drained += 1;
            }
            Err(e) => {
                cfg.metrics.failed_total.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    route = %cfg.route_name,
                    error = %e,
                    "proxy drain send failed"
                );
            }
        }
    }
    timer.abort();

    if abandoned > 0 {
        cfg.metrics
            .dropped_total
            .fetch_add(abandoned, Ordering::Relaxed);
        tracing::warn!(
            route = %cfg.route_name,
            abandoned,
            drained,
            "proxy worker: drain deadline exceeded; dropping queued batches"
        );
    } else if drained > 0 {
        tracing::info!(
            route = %cfg.route_name,
            drained,
            "proxy worker: drained queued batches at shutdown"
        );
    }
}

async fn send_with_retry(
    route_name: &str,
    client: &RouteClient,
    request: &ExportRequest,
    retry_max: u32,
    shutdown: &CancellationToken,
) -> anyhow::Result<()> {
    let mut delay = Duration::from_millis(200);
    let mut last_err: Option<anyhow::Error> = None;
    let attempts = retry_max.saturating_add(1); // 初回 + retry_max 回
    for attempt in 0..attempts {
        if shutdown.is_cancelled() {
            anyhow::bail!("cancelled during retry (route={route_name})");
        }
        let export_result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                anyhow::bail!("cancelled during send (route={route_name})");
            }
            result = client.export(request.clone()) => result,
        };
        match export_result {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::debug!(
                    route = %route_name,
                    attempt = attempt + 1,
                    max = attempts,
                    error = %e,
                    "proxy send attempt failed"
                );
                last_err = Some(e);
                if attempt + 1 >= attempts {
                    break;
                }
                // shutdown 中は無駄な sleep をせず即抜ける。
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        anyhow::bail!("cancelled during backoff (route={route_name})");
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
                // 単純 exponential (cap 30s)。jitter は運用中の同時多発を狙う場面は
                // 現状少ないので省略。
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("unknown send error")))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{Router, extract::State, http::StatusCode, routing::post};
    use tokio::{net::TcpListener, sync::Notify};

    use super::*;
    use crate::cli::ProxyRoute;
    use crate::config::{ProxySignal, ProxyTransport};
    use crate::forward::RouteMetrics;

    async fn stalled_upstream(State(started): State<Arc<Notify>>) {
        started.notify_one();
        std::future::pending::<()>().await;
    }

    async fn failing_upstream(State(attempts): State<Arc<AtomicUsize>>) -> StatusCode {
        attempts.fetch_add(1, Ordering::Relaxed);
        StatusCode::INTERNAL_SERVER_ERROR
    }

    /// `retry_max` は初回送信を含む総試行回数ではなく、初回失敗後の再試行回数として扱う。
    #[tokio::test]
    async fn retry_max_counts_retries_after_initial_attempt() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/v1/logs", post(failing_upstream))
            .with_state(Arc::clone(&attempts));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let route = ProxyRoute {
            name: "test".to_string(),
            service_names: vec!["test-service".to_string()],
            signals: ProxySignal::ALL.to_vec(),
            transport: ProxyTransport::HttpProtobuf,
            endpoint: format!("http://{addr}"),
            headers: vec![],
        };
        let client = RouteClient::build(&route, 3_000).unwrap();
        let request = ExportRequest::Logs(Box::default());
        let shutdown = CancellationToken::new();

        let result = send_with_retry("test", &client, &request, 1, &shutdown).await;

        assert!(result.is_err(), "再試行上限後は送信失敗を返す");
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            2,
            "初回送信に加えて retry_max 回だけ再試行する"
        );
        server.abort();
    }

    /// 上流が応答しない送信中でも、shutdown を受けたら timeout を待たずに中断する。
    #[tokio::test]
    async fn send_with_retry_cancels_in_flight_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let started = Arc::new(Notify::new());
        let app = Router::new()
            .route("/v1/logs", post(stalled_upstream))
            .with_state(Arc::clone(&started));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let route = ProxyRoute {
            name: "test".to_string(),
            service_names: vec!["test-service".to_string()],
            signals: ProxySignal::ALL.to_vec(),
            transport: ProxyTransport::HttpProtobuf,
            endpoint: format!("http://{addr}"),
            headers: vec![],
        };
        let client = RouteClient::build(&route, 30_000).unwrap();
        let shutdown = CancellationToken::new();
        let request = ExportRequest::Logs(Box::default());

        let send_shutdown = shutdown.clone();
        let send = tokio::spawn(async move {
            send_with_retry("test", &client, &request, 3, &send_shutdown).await
        });
        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("上流が 1 秒以内に送信を開始する");
        shutdown.cancel();

        let result = tokio::time::timeout(Duration::from_secs(1), send)
            .await
            .expect("shutdown 後は 1 秒以内に送信を中断する")
            .expect("送信タスクは panic しない");
        assert!(result.is_err(), "shutdown は送信をエラー終了させる");

        server.abort();
    }

    async fn ok_upstream(State(received): State<Arc<AtomicUsize>>) -> StatusCode {
        received.fetch_add(1, Ordering::Relaxed);
        StatusCode::OK
    }

    /// 回帰テスト: shutdown 時に queue へ残っている batch を捨てずに送り切る。
    ///
    /// 猶予ゼロで捨てると、上流の応答より ingest が速いだけの通常運用でも、再起動の
    /// たびに route あたり最大 queue 容量ぶんが上流へ届かない。Phase B の JSONL
    /// catch-up が入るまでは worker 再起動後の回収経路が無く、そのまま欠測になる。
    #[tokio::test]
    async fn shutdown_drains_queued_batches_before_stopping() {
        use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
        use opentelemetry_proto::tonic::logs::v1::ResourceLogs;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/v1/logs", post(ok_upstream))
            .with_state(Arc::clone(&received));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let route = ProxyRoute {
            name: "test".to_string(),
            service_names: vec!["test-service".to_string()],
            signals: ProxySignal::ALL.to_vec(),
            transport: ProxyTransport::HttpProtobuf,
            endpoint: format!("http://{addr}"),
            headers: vec![],
        };
        let client = RouteClient::build(&route, 3_000).unwrap();
        let metrics: RouteMetricsHandle = Arc::new(RouteMetrics::default());
        let (tx, rx) = mpsc::channel::<ExportRequest>(16);

        // worker を起こす前に queue へ積み、その状態で shutdown 済みにしておく。
        // これで「shutdown 後に残っていた batch」だけを対象にできる。
        for _ in 0..4 {
            tx.send(ExportRequest::Logs(Box::new(ExportLogsServiceRequest {
                resource_logs: vec![ResourceLogs::default()],
            })))
            .await
            .unwrap();
        }
        drop(tx);
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let handle = spawn(WorkerConfig {
            route_name: "test".to_string(),
            client,
            receiver: rx,
            metrics: Arc::clone(&metrics),
            retry_max: 0,
            shutdown,
        });
        tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("drain は猶予内に終わる")
            .expect("worker は panic しない");

        assert_eq!(
            received.load(Ordering::Relaxed),
            4,
            "queue に残っていた batch をすべて上流へ送る"
        );
        assert_eq!(metrics.sent_total.load(Ordering::Relaxed), 4);
        assert_eq!(
            metrics.dropped_total.load(Ordering::Relaxed),
            0,
            "猶予内に送れた batch を drop として数えない"
        );

        server.abort();
    }
}
