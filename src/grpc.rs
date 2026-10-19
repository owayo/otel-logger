use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use http_body_util::BodyExt as _;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_server::{
    LogsService, LogsServiceServer,
};
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_server::{
    MetricsService, MetricsServiceServer,
};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
    TraceService, TraceServiceServer,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use tonic::{Request, Response, Status};
use tower_http::timeout::{DeadlineBody, TimeoutError};

use crate::admission::{ReceiveLimits, ReceivePermit};
use crate::sink::{Sink, TelemetryRecord};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// OTLP/gRPC の受信を `ReceiveLimits` で制限する tower layer。tonic の `Server` に挟む。
///
/// 生成された service は handler を呼ぶ前に body を読んで decode するので、handler の中で
/// 枠を取っても遅い。ここで枠を取り、body には受け付けからの期限を掛ける。枠は request
/// extension で handler へ渡し、handler は保存の task まで持たせる。
#[derive(Clone)]
pub struct AdmissionLayer {
    limits: ReceiveLimits,
}

impl AdmissionLayer {
    pub fn new(limits: ReceiveLimits) -> Self {
        Self { limits }
    }
}

impl<S> tower::Layer<S> for AdmissionLayer {
    type Service = Admission<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Admission {
            inner,
            limits: self.limits.clone(),
        }
    }
}

/// `AdmissionLayer` が包んだ service。
#[derive(Clone)]
pub struct Admission<S> {
    inner: S,
    limits: ReceiveLimits,
}

impl<S> tower::Service<http::Request<tonic::body::Body>> for Admission<S>
where
    S: tower::Service<
            http::Request<tonic::body::Body>,
            Response = http::Response<tonic::body::Body>,
        > + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        // ready になった inner をこの request に使い、次の request には clone を残す
        // (tower の middleware の慣例。tonic の router は clone しても常に ready)。
        let ready = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, ready);
        let limits = self.limits.clone();
        Box::pin(async move {
            let deadline = limits.deadline();
            let permit = match limits.admit(deadline).await {
                Ok(permit) => permit,
                // OTLP では Unavailable が retryable。保存前なので exporter に再送させる。
                Err(rejected) => return Ok(Status::unavailable(rejected.message()).into_http()),
            };
            let (mut parts, body) = request.into_parts();
            parts.extensions.insert(permit.clone());
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let body = tonic::body::Body::new(
                DeadlineBody::new(remaining, body).map_err(deadline_exceeded_as_unavailable),
            );
            let response = inner.call(http::Request::from_parts(parts, body)).await;
            // handler が保存の task へ clone を渡しているので、保存が終わるまで枠は戻らない。
            drop(permit);
            response
        })
    }
}

/// body の期限切れを retryable な `Unavailable` にする。tonic は body のエラーが `Status`
/// でなければ `Unknown` (OTLP では retry できない) を返し、exporter は batch を捨てる。
fn deadline_exceeded_as_unavailable(error: BoxError) -> BoxError {
    if error.is::<TimeoutError>() {
        Box::new(Status::unavailable(
            "request body was not received before the deadline; retry later",
        ))
    } else {
        error
    }
}

/// handler が受け取った request の枠 (`AdmissionLayer` を通っていなければ `None`)。
fn receive_permit<T>(request: &Request<T>) -> Option<ReceivePermit> {
    request.extensions().get::<ReceivePermit>().cloned()
}

#[derive(Clone)]
pub struct OtlpService {
    sink: Sink,
}

impl OtlpService {
    pub fn new(sink: Sink) -> Self {
        Self { sink }
    }

    pub fn into_servers(
        self,
    ) -> (
        TraceServiceServer<Self>,
        MetricsServiceServer<Self>,
        LogsServiceServer<Self>,
    ) {
        (
            TraceServiceServer::new(self.clone()),
            MetricsServiceServer::new(self.clone()),
            LogsServiceServer::new(self),
        )
    }
}

/// JSONL 永続化失敗を gRPC client へ `Status::unavailable` として返す。
/// OTLP は retryable なエラーを `Unavailable` で示すよう定めており、`Internal` は
/// non-retryable (client は payload を drop する) 扱いになる。受信した payload を
/// 欠落させないために、必ず retryable な code で返して exporter に再送させる。
fn persistence_status(e: anyhow::Error) -> Status {
    Status::unavailable(format!("failed to persist telemetry: {e}"))
}

#[tonic::async_trait]
impl TraceService for OtlpService {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        let permit = receive_permit(&request);
        let payload = request.into_inner();
        self.sink
            .record_with_permit(TelemetryRecord::Traces(Box::new(payload)), permit)
            .await
            .map_err(persistence_status)?;
        Ok(Response::new(ExportTraceServiceResponse {
            partial_success: None,
        }))
    }
}

#[tonic::async_trait]
impl MetricsService for OtlpService {
    async fn export(
        &self,
        request: Request<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        let permit = receive_permit(&request);
        let payload = request.into_inner();
        self.sink
            .record_with_permit(TelemetryRecord::Metrics(Box::new(payload)), permit)
            .await
            .map_err(persistence_status)?;
        Ok(Response::new(ExportMetricsServiceResponse {
            partial_success: None,
        }))
    }
}

#[tonic::async_trait]
impl LogsService for OtlpService {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        let permit = receive_permit(&request);
        let payload = request.into_inner();
        self.sink
            .record_with_permit(TelemetryRecord::Logs(Box::new(payload)), permit)
            .await
            .map_err(persistence_status)?;
        Ok(Response::new(ExportLogsServiceResponse {
            partial_success: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with_log_file(path: std::path::PathBuf) -> crate::cli::Settings {
        use crate::cli::{ColorMode, LogSink};
        crate::cli::Settings {
            grpc_addr: "127.0.0.1:0".parse().unwrap(),
            http_addr: "127.0.0.1:0".parse().unwrap(),
            max_inflight_requests: crate::admission::DEFAULT_MAX_INFLIGHT_REQUESTS,
            receive_timeout: crate::admission::DEFAULT_RECEIVE_TIMEOUT,
            log_sink: Some(LogSink::File(path)),
            no_stdout: true,
            pretty_log_dir: None,
            summary: false,
            color: ColorMode::Never,
            dry_run: false,
            proxy: None,
        }
    }

    /// 応答の `grpc-status` header (`Status::into_http` が書く値)。
    fn grpc_status(response: &http::Response<tonic::body::Body>) -> Option<i32> {
        response
            .headers()
            .get("grpc-status")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
    }

    /// body を最後まで読み、読めなければその `Status` で応答する inner service。
    /// `tonic::body::Body::new` は包んだ body のエラーを `Status::from_error` で変換するので、
    /// ここで受け取るのは tonic の decoder が受け取るのと同じ `Status`。枠が request
    /// extension で届いたかも記録する。
    fn reading_service(
        saw_permit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> impl tower::Service<
        http::Request<tonic::body::Body>,
        Response = http::Response<tonic::body::Body>,
        Error = std::convert::Infallible,
        Future = impl Send,
    > + Clone
    + Send
    + 'static {
        tower::service_fn(move |request: http::Request<tonic::body::Body>| {
            let saw_permit = std::sync::Arc::clone(&saw_permit);
            async move {
                if request.extensions().get::<ReceivePermit>().is_some() {
                    saw_permit.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                let status = match request.into_body().collect().await {
                    Ok(_) => Status::new(tonic::Code::Ok, ""),
                    Err(status) => status,
                };
                Ok::<_, std::convert::Infallible>(status.into_http())
            }
        })
    }

    /// 回帰テスト: gRPC も body の受信に受け付けからの期限を掛け、期限切れは retryable な
    /// `Unavailable` (14) にする。`Status` 以外の body エラーは tonic が `Unknown` にするため、
    /// 変換しないと exporter は batch を捨てる。枠は handler へ request extension で渡す。
    #[tokio::test(start_paused = true)]
    async fn admission_layer_turns_a_stalled_body_into_unavailable() {
        use tonic::codegen::tokio_stream as stream;
        use tower::{Layer as _, Service as _, ServiceExt as _};

        let saw_permit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let limits = ReceiveLimits::new(1, std::time::Duration::from_millis(200));
        let mut service =
            AdmissionLayer::new(limits).layer(reading_service(std::sync::Arc::clone(&saw_permit)));
        let stalled = axum::body::Body::from_stream(stream::pending::<
            Result<bytes::Bytes, std::io::Error>,
        >());
        let request = http::Request::new(tonic::body::Body::new(stalled));

        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(
            grpc_status(&response),
            Some(tonic::Code::Unavailable as i32)
        );
        assert!(saw_permit.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// 処理中の上限に達したまま期限が来たら、inner (decode と handler) を呼ばずに
    /// retryable な `Unavailable` を返す。
    #[tokio::test(start_paused = true)]
    async fn admission_layer_rejects_with_unavailable_when_no_slot_frees() {
        use tower::{Layer as _, Service as _, ServiceExt as _};

        let saw_permit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let limits = ReceiveLimits::new(1, std::time::Duration::from_millis(200));
        let _held = limits.admit(limits.deadline()).await.unwrap();
        let mut service = AdmissionLayer::new(limits.clone())
            .layer(reading_service(std::sync::Arc::clone(&saw_permit)));
        let request = http::Request::new(tonic::body::Body::empty());

        let response = service.ready().await.unwrap().call(request).await.unwrap();
        assert_eq!(
            grpc_status(&response),
            Some(tonic::Code::Unavailable as i32)
        );
        assert!(
            !saw_permit.load(std::sync::atomic::Ordering::SeqCst),
            "枠を取れない request は decode しない"
        );
    }

    /// 応答を返し終えたら枠を返し、同時数 1 でも次の request を通す。CLI で渡せる最大の期限
    /// (u64 のミリ秒の最大値) でも、body に掛ける期限の計算を溢れさせない。
    #[tokio::test(start_paused = true)]
    async fn admission_layer_returns_the_slot_after_each_response() {
        use tower::{Layer as _, Service as _, ServiceExt as _};

        let saw_permit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let limits = ReceiveLimits::new(1, std::time::Duration::from_millis(u64::MAX));
        let mut service =
            AdmissionLayer::new(limits).layer(reading_service(std::sync::Arc::clone(&saw_permit)));
        for _ in 0..3 {
            let started = tokio::time::Instant::now();
            let request =
                http::Request::new(tonic::body::Body::new(axum::body::Body::from("payload")));
            let response = service.ready().await.unwrap().call(request).await.unwrap();
            assert_eq!(grpc_status(&response), Some(tonic::Code::Ok as i32));
            assert_eq!(
                started.elapsed(),
                std::time::Duration::ZERO,
                "前の request の枠を待たない"
            );
        }
        assert!(saw_permit.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// JSONL 永続化失敗は gRPC で必ず retryable な `Unavailable` を返し、exporter 側に
    /// retry させる契約を固定する。`Internal` は OTLP 上 non-retryable で、client が
    /// payload を drop してしまうため使わない。
    #[test]
    fn persistence_status_maps_to_retryable_code() {
        let status = persistence_status(anyhow::anyhow!("disk full"));
        assert_eq!(status.code(), tonic::Code::Unavailable);
        assert!(
            status.message().contains("disk full"),
            "原因を message に含める: {}",
            status.message()
        );
    }

    /// Traces / Metrics / Logs の 3 ハンドラがいずれも `Ok` を返し、payload を
    /// JSONL へ永続化することを確認する (各 record の種別取り違え防止も兼ねる)。
    #[tokio::test]
    async fn export_handlers_persist_payload_and_return_ok() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("otel-logger.jsonl");
        let sink = Sink::from_settings(&settings_with_log_file(log_path.clone()))
            .await
            .unwrap();
        let service = OtlpService::new(sink.clone());

        TraceService::export(
            &service,
            Request::new(ExportTraceServiceRequest {
                resource_spans: vec![],
            }),
        )
        .await
        .expect("traces export は Ok");
        MetricsService::export(
            &service,
            Request::new(ExportMetricsServiceRequest {
                resource_metrics: vec![],
            }),
        )
        .await
        .expect("metrics export は Ok");
        let logs_resp = LogsService::export(
            &service,
            Request::new(ExportLogsServiceRequest {
                resource_logs: vec![],
            }),
        )
        .await
        .expect("logs export は Ok");
        assert!(
            logs_resp.into_inner().partial_success.is_none(),
            "全件成功なので partial_success は None"
        );

        sink.flush().await.unwrap();
        let body = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            body.contains("\"kind\":\"traces\""),
            "traces 行が永続化される"
        );
        assert!(
            body.contains("\"kind\":\"metrics\""),
            "metrics 行が永続化される"
        );
        assert!(body.contains("\"kind\":\"logs\""), "logs 行が永続化される");
    }
}
