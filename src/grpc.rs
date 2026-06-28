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

use crate::sink::{Sink, TelemetryRecord};

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

/// JSONL 永続化失敗を gRPC client へ `Status::internal` として返す。
/// 受信した payload を欠落させないために、exporter 側で retry を促す。
fn persistence_status(e: anyhow::Error) -> Status {
    Status::internal(format!("failed to persist telemetry: {e}"))
}

#[tonic::async_trait]
impl TraceService for OtlpService {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        let payload = request.into_inner();
        self.sink
            .record(TelemetryRecord::Traces(Box::new(payload)))
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
        let payload = request.into_inner();
        self.sink
            .record(TelemetryRecord::Metrics(Box::new(payload)))
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
        let payload = request.into_inner();
        self.sink
            .record(TelemetryRecord::Logs(Box::new(payload)))
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
            log_sink: Some(LogSink::File(path)),
            no_stdout: true,
            summary: false,
            color: ColorMode::Never,
            dry_run: false,
        }
    }

    /// JSONL 永続化失敗は gRPC で必ず `Internal` を返し、exporter 側に retry させる契約を固定する。
    #[test]
    fn persistence_status_maps_to_internal_code() {
        let status = persistence_status(anyhow::anyhow!("disk full"));
        assert_eq!(status.code(), tonic::Code::Internal);
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
