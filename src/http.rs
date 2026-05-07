use std::net::SocketAddr;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use prost::Message;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::sink::{Sink, TelemetryRecord};

const PROTOBUF_CT: &str = "application/x-protobuf";
const JSON_CT: &str = "application/json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Protobuf,
    Json,
}

#[derive(Debug, Error)]
enum HttpError {
    #[error("unsupported content type: {0}")]
    UnsupportedContentType(String),
    #[error("failed to decode protobuf body: {0}")]
    BadProtobuf(#[from] prost::DecodeError),
    #[error("failed to decode JSON body: {0}")]
    BadJson(#[from] serde_json::Error),
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let status = match &self {
            HttpError::UnsupportedContentType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            HttpError::BadProtobuf(_) | HttpError::BadJson(_) => StatusCode::BAD_REQUEST,
        };
        (status, self.to_string()).into_response()
    }
}

fn detect_encoding(headers: &HeaderMap) -> Result<Encoding, HttpError> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(PROTOBUF_CT);
    let primary = ct.split(';').next().unwrap_or(ct).trim();
    match primary {
        PROTOBUF_CT => Ok(Encoding::Protobuf),
        JSON_CT => Ok(Encoding::Json),
        other => Err(HttpError::UnsupportedContentType(other.to_string())),
    }
}

fn encode_response<M>(encoding: Encoding, msg: &M) -> Response
where
    M: Message + serde::Serialize,
{
    match encoding {
        Encoding::Protobuf => {
            let mut buf = Vec::with_capacity(msg.encoded_len());
            msg.encode(&mut buf).expect("encode response into Vec");
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, HeaderValue::from_static(PROTOBUF_CT))],
                buf,
            )
                .into_response()
        }
        Encoding::Json => {
            let body = serde_json::to_vec(msg).expect("serialize response to JSON");
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, HeaderValue::from_static(JSON_CT))],
                body,
            )
                .into_response()
        }
    }
}

fn decode_request<M>(encoding: Encoding, body: &[u8]) -> Result<M, HttpError>
where
    M: Message + Default + serde::de::DeserializeOwned,
{
    match encoding {
        Encoding::Protobuf => Ok(M::decode(body)?),
        Encoding::Json => Ok(serde_json::from_slice(body)?),
    }
}

async fn handle_traces(
    State(sink): State<Sink>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HttpError> {
    let encoding = detect_encoding(&headers)?;
    let req: ExportTraceServiceRequest = decode_request(encoding, &body)?;
    sink.record(TelemetryRecord::Traces(Box::new(req))).await;
    Ok(encode_response(
        encoding,
        &ExportTraceServiceResponse {
            partial_success: None,
        },
    ))
}

async fn handle_metrics(
    State(sink): State<Sink>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HttpError> {
    let encoding = detect_encoding(&headers)?;
    let req: ExportMetricsServiceRequest = decode_request(encoding, &body)?;
    sink.record(TelemetryRecord::Metrics(Box::new(req))).await;
    Ok(encode_response(
        encoding,
        &ExportMetricsServiceResponse {
            partial_success: None,
        },
    ))
}

async fn handle_logs(
    State(sink): State<Sink>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HttpError> {
    let encoding = detect_encoding(&headers)?;
    let req: ExportLogsServiceRequest = decode_request(encoding, &body)?;
    sink.record(TelemetryRecord::Logs(Box::new(req))).await;
    Ok(encode_response(
        encoding,
        &ExportLogsServiceResponse {
            partial_success: None,
        },
    ))
}

async fn health() -> &'static str {
    "ok"
}

async fn handle_stats(State(sink): State<Sink>) -> Json<crate::aggregator::UsageSnapshot> {
    Json(sink.aggregator().snapshot())
}

pub fn router(sink: Sink) -> Router {
    Router::new()
        .route("/v1/traces", post(handle_traces))
        .route("/v1/metrics", post(handle_metrics))
        .route("/v1/logs", post(handle_logs))
        .route("/healthz", get(health))
        .route("/stats", get(handle_stats))
        .with_state(sink)
}

pub async fn serve(
    addr: SocketAddr,
    sink: Sink,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    tracing::info!(addr = %local, "OTLP/HTTP server listening");
    let app = router(sink);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await?;
    Ok(())
}
