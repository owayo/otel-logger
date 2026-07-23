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
        match client.export(request.clone()).await {
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
