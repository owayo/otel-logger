//! Per-route worker task。ExportRequest を受け取り、retry しながら送信する。
//!
//! shutdown token が cancel されるか channel が close されたら終了する。

use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::client::RouteClient;
use super::{ExportRequest, RouteMetricsHandle};

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
                cfg.metrics.queue_depth.fetch_sub(1, Ordering::Relaxed);
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
    tracing::info!(route = %cfg.route_name, "proxy worker stopped");
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
    use std::sync::Arc;

    use axum::{Router, extract::State, routing::post};
    use tokio::{net::TcpListener, sync::Notify};

    use super::*;
    use crate::cli::ProxyRoute;
    use crate::config::{ProxySignal, ProxyTransport};

    async fn stalled_upstream(State(started): State<Arc<Notify>>) {
        started.notify_one();
        std::future::pending::<()>().await;
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
}
