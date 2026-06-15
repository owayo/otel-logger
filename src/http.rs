use std::net::SocketAddr;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
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

use crate::server::OTLP_MAX_REQUEST_BYTES;
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
    /// JSONL や stdout への永続化に失敗した。受信した payload を欠落させたくないので
    /// クライアントに 5xx を返し、OTLP exporter 側で retry させる。
    #[error("failed to persist telemetry: {0}")]
    Persistence(#[source] anyhow::Error),
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let status = match &self {
            HttpError::UnsupportedContentType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            HttpError::BadProtobuf(_) | HttpError::BadJson(_) => StatusCode::BAD_REQUEST,
            HttpError::Persistence(_) => StatusCode::INTERNAL_SERVER_ERROR,
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
    // HTTP の media type (type/subtype) は大小文字を区別しない (RFC 9110)。
    // `Application/X-Protobuf` のような表記でも正当な OTLP/HTTP request として受け付け、
    // decode 前に 415 で拒否しない。
    if primary.eq_ignore_ascii_case(PROTOBUF_CT) {
        Ok(Encoding::Protobuf)
    } else if primary.eq_ignore_ascii_case(JSON_CT) {
        Ok(Encoding::Json)
    } else {
        Err(HttpError::UnsupportedContentType(primary.to_string()))
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
    sink.record(TelemetryRecord::Traces(Box::new(req)))
        .await
        .map_err(HttpError::Persistence)?;
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
    sink.record(TelemetryRecord::Metrics(Box::new(req)))
        .await
        .map_err(HttpError::Persistence)?;
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
    sink.record(TelemetryRecord::Logs(Box::new(req)))
        .await
        .map_err(HttpError::Persistence)?;
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
        // 既定 (2MiB) では大きな batch が抽出前に 413 で拒否されるため、上限を引き上げる。
        .layer(DefaultBodyLimit::max(OTLP_MAX_REQUEST_BYTES))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_ct(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn detect_encoding_defaults_to_protobuf_when_header_missing() {
        let headers = HeaderMap::new();
        assert_eq!(detect_encoding(&headers).unwrap(), Encoding::Protobuf);
    }

    #[test]
    fn detect_encoding_recognises_protobuf_and_json() {
        let h_proto = headers_with_ct(PROTOBUF_CT);
        let h_json = headers_with_ct(JSON_CT);
        assert_eq!(detect_encoding(&h_proto).unwrap(), Encoding::Protobuf);
        assert_eq!(detect_encoding(&h_json).unwrap(), Encoding::Json);
    }

    #[test]
    fn detect_encoding_ignores_parameters_in_content_type() {
        // OTLP spec では `application/x-protobuf; charset=utf-8` のような表記も許容される。
        let h = headers_with_ct("application/x-protobuf; charset=utf-8");
        assert_eq!(detect_encoding(&h).unwrap(), Encoding::Protobuf);
    }

    #[test]
    fn detect_encoding_rejects_unknown_content_type() {
        let h = headers_with_ct("text/plain");
        let err = detect_encoding(&h).unwrap_err();
        match err {
            HttpError::UnsupportedContentType(ct) => assert_eq!(ct, "text/plain"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn detect_encoding_treats_media_type_case_insensitively() {
        // HTTP の media type (type/subtype) は大小文字非区別 (RFC 9110)。
        // 大文字混じりの Content-Type でも正当な OTLP/HTTP request として受け付ける。
        let h = headers_with_ct("Application/X-Protobuf; charset=utf-8");
        assert_eq!(detect_encoding(&h).unwrap(), Encoding::Protobuf);
        let h = headers_with_ct("APPLICATION/JSON");
        assert_eq!(detect_encoding(&h).unwrap(), Encoding::Json);
    }

    #[test]
    fn decode_request_returns_error_for_malformed_protobuf() {
        // 0xFF だけの bytes は proto Message として decode できない。
        let result: Result<ExportTraceServiceRequest, _> =
            decode_request(Encoding::Protobuf, &[0xFF]);
        assert!(result.is_err());
    }

    /// axum 既定の 2MiB body limit を超える batch でも 413 で拒否されず、
    /// `OTLP_MAX_REQUEST_BYTES` まで受け付けることの回帰テスト。
    /// 受信した payload を欠落なく保存する方針上、大きな batch を size で弾かないことが重要。
    #[tokio::test]
    async fn router_accepts_body_larger_than_axum_default_limit() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        use crate::cli::{ColorMode, Settings};

        let settings = Settings {
            grpc_addr: "127.0.0.1:0".parse().unwrap(),
            http_addr: "127.0.0.1:0".parse().unwrap(),
            log_sink: None,
            no_stdout: true,
            summary: false,
            color: ColorMode::Never,
            dry_run: false,
        };
        let sink = Sink::from_settings(&settings).await.unwrap();
        let app = router(sink);

        // axum 既定の 2MiB を超えるが OTLP_MAX_REQUEST_BYTES (32MiB) には収まる body。
        // 中身は不正な protobuf なので、size 制限を通過できれば decode 失敗で 400 になる。
        // 制限が 2MiB のままなら body 抽出時点で 413 Payload Too Large になる。
        let oversized = vec![0xFF_u8; 3 * 1024 * 1024];
        let request = Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .header(header::CONTENT_TYPE, PROTOBUF_CT)
            .body(Body::from(oversized))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "2MiB 超の body が size 制限ではなく protobuf decode 失敗で弾かれること"
        );
    }
}
