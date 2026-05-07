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

#[tonic::async_trait]
impl TraceService for OtlpService {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        let payload = request.into_inner();
        self.sink
            .record(TelemetryRecord::Traces(Box::new(payload)))
            .await;
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
            .await;
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
            .await;
        Ok(Response::new(ExportLogsServiceResponse {
            partial_success: None,
        }))
    }
}
