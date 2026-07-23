//! OTLP proxy forwarder。
//!
//! 受信した OTLP payload を service.name で振り分け、Anthropic/OpenAI などの
//! 上流 collector へ transparently に forward する。JSONL 保存 (受信 payload の
//! 欠落防止) が最優先で、proxy 転送はその後で `try_send` で route worker に渡す。
//!
//! ## 現行 (Phase A) の delivery セマンティクス
//!
//! - 受信 → JSONL 書き込み → route worker への `try_send` (bounded channel)
//! - route worker は tokio task で backoff + retry しつつ送信
//! - endpoint down が長引いても queue capacity までは保持し、溢れたら
//!   drop-newest + drop counter で警告 (`/stats` で観測できる)
//! - process crash 時は in-flight (JSONL には残っているが未送信のもの) が失われる
//!   可能性がある。JSONL byte offset ベースの checkpoint 追跡は Phase B で対応する
//!
//! ## Notify チャネルの overflow
//!
//! bounded queue は route ごとに独立で、上流の一時的な遅延から他 route を守る。
//! 溢れたら drop-newest で counter を上げる。**欠測 0 を厳密にするには Phase B で
//! JSONL を outbox として扱い、checkpoint 側から catch-up 走査するのが必須**。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::cli::ProxySettings;
use crate::config::ProxySignal;

pub mod client;
mod router;
mod worker;

pub use router::ProxyRouter;

use router::RouteLane;

/// proxy worker 一式を保持し、shutdown 時に join できるようにするハンドル。
pub struct ProxyHandle {
    router: ProxyRouter,
    workers: Vec<JoinHandle<()>>,
}

impl ProxyHandle {
    /// 呼び出し側は Router を Sink に渡して notify させる。
    pub fn router(&self) -> ProxyRouter {
        self.router.clone()
    }

    /// worker が全て停止するのを待つ。shutdown の後で server::run から呼ぶ。
    pub async fn join(self) {
        for w in self.workers {
            let _ = w.await;
        }
    }
}

/// `ProxySettings` から Router + 各 route の worker を起動する。
pub async fn spawn_router(
    settings: &ProxySettings,
    shutdown: CancellationToken,
) -> Result<ProxyHandle> {
    let mut lanes: Vec<Arc<RouteLane>> = Vec::with_capacity(settings.routes.len());
    let mut workers: Vec<JoinHandle<()>> = Vec::with_capacity(settings.routes.len());

    for route in &settings.routes {
        let (tx, rx) = mpsc::channel::<ExportRequest>(settings.queue_capacity.max(1));
        let metrics: RouteMetricsHandle = Arc::new(RouteMetrics::default());
        let client = client::RouteClient::build(route, settings.timeout_ms)?;
        let lane = Arc::new(RouteLane {
            name: route.name.clone(),
            route: route.clone(),
            sender: tx,
            metrics: Arc::clone(&metrics),
        });
        lanes.push(lane);
        let cfg = worker::WorkerConfig {
            route_name: route.name.clone(),
            client,
            receiver: rx,
            metrics,
            retry_max: settings.retry_max,
            shutdown: shutdown.clone(),
        };
        workers.push(worker::spawn(cfg));
    }
    let router = ProxyRouter::new(lanes);
    Ok(ProxyHandle { router, workers })
}

/// route worker が処理する OTLP request。resource は route にマッチする分だけに絞られている。
#[derive(Debug, Clone)]
pub enum ExportRequest {
    Logs(Box<ExportLogsServiceRequest>),
    Traces(Box<ExportTraceServiceRequest>),
    Metrics(Box<ExportMetricsServiceRequest>),
}

impl ExportRequest {
    pub fn signal(&self) -> ProxySignal {
        match self {
            Self::Logs(_) => ProxySignal::Logs,
            Self::Traces(_) => ProxySignal::Traces,
            Self::Metrics(_) => ProxySignal::Metrics,
        }
    }

    /// マッチした resource がゼロなら送信不要。
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Logs(r) => r.resource_logs.is_empty(),
            Self::Traces(r) => r.resource_spans.is_empty(),
            Self::Metrics(r) => r.resource_metrics.is_empty(),
        }
    }
}

/// route ごとの累計メトリクス。`/stats` に露出させる。
#[derive(Debug, Default)]
pub struct RouteMetrics {
    /// forward 成功 (2xx / gRPC OK) 累計。
    pub sent_total: AtomicU64,
    /// forward 失敗 (retry 上限に達したもの) 累計。
    pub failed_total: AtomicU64,
    /// notify channel overflow で drop した累計。
    pub dropped_total: AtomicU64,
    /// 現在 queue に残っているおおよその件数 (snapshot 時に読む)。
    pub queue_depth: AtomicU64,
}

impl RouteMetrics {
    pub fn snapshot(&self) -> RouteMetricsSnapshot {
        RouteMetricsSnapshot {
            sent: self.sent_total.load(Ordering::Relaxed),
            failed: self.failed_total.load(Ordering::Relaxed),
            dropped: self.dropped_total.load(Ordering::Relaxed),
            queue_depth: self.queue_depth.load(Ordering::Relaxed),
        }
    }
}

/// `/stats` に載せる route ごとの snapshot。
#[derive(Debug, Serialize, Clone, Copy)]
pub struct RouteMetricsSnapshot {
    pub sent: u64,
    pub failed: u64,
    pub dropped: u64,
    pub queue_depth: u64,
}

/// route metrics の共有ハンドル。router / worker / stats で共有する。
pub type RouteMetricsHandle = Arc<RouteMetrics>;
