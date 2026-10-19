use std::net::SocketAddr;

use anyhow::Context as _;
use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
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
use tower_http::decompression::RequestDecompressionLayer;

use crate::admission::{ReceiveLimits, ReceivePermit, Rejected};
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
    /// body を読めなかった (gzip の展開に失敗した、接続が切れたなど)。
    #[error("failed to read request body: {0}")]
    BadBody(#[source] axum::Error),
    /// 展開後の body が `OTLP_MAX_REQUEST_BYTES` を超えた。
    #[error("request body exceeds {OTLP_MAX_REQUEST_BYTES} bytes")]
    BodyTooLarge,
    /// 受信の枠が空かなかった (`admission`)。
    #[error("{}", .0.message())]
    Busy(Rejected),
    /// 受け付けからの期限までに body を読み終えなかった。
    #[error("request body was not received before the deadline; retry later")]
    BodyTimeout,
    /// JSONL への永続化に失敗した。受信した payload を欠落させたくないので
    /// クライアントに retry 可能な応答を返し、OTLP exporter 側で再送させる。
    #[error("failed to persist telemetry: {0}")]
    Persistence(#[source] anyhow::Error),
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let status = match &self {
            HttpError::UnsupportedContentType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            HttpError::BadProtobuf(_) | HttpError::BadJson(_) | HttpError::BadBody(_) => {
                StatusCode::BAD_REQUEST
            }
            HttpError::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            // OTLP/HTTP が retry を許すのは 429 / 502 / 503 / 504 だけで、それ以外の
            // 4xx / 5xx は "MUST NOT be retried" (client は payload を drop する)。
            // 500 を返すと再送されず欠測するため、503 で retry させる。受信の上限と期限で
            // 断った request も、保存前なので同じく 503 で再送させる (408 は retry できない)。
            HttpError::Busy(_) | HttpError::BodyTimeout | HttpError::Persistence(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
        };
        (status, self.to_string()).into_response()
    }
}

/// 受信の枠を取ってから、受け付けからの期限までに body を読み切る。
///
/// 枠は body を読む前に取る。展開後の body (最大 32MiB) を溜め始めてから上限を数えても
/// 総メモリは抑えられない。期限は request 全体ではなく受信だけに掛ける。保存の後で
/// 打ち切ると、保存済みの batch に失敗を返して exporter の再送で JSONL が重複する。
async fn receive_body(
    limits: &ReceiveLimits,
    body: Body,
) -> Result<(ReceivePermit, Bytes), HttpError> {
    let deadline = limits.deadline();
    let permit = limits.admit(deadline).await.map_err(HttpError::Busy)?;
    // body は `RequestDecompressionLayer` で展開されるので、上限は展開後のバイト数に効く
    // (仕様が要求する "including after decompression")。
    let bytes =
        match tokio::time::timeout_at(deadline, axum::body::to_bytes(body, OTLP_MAX_REQUEST_BYTES))
            .await
        {
            Err(_) => return Err(HttpError::BodyTimeout),
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) if exceeds_length_limit(&error) => return Err(HttpError::BodyTooLarge),
            Ok(Err(error)) => return Err(HttpError::BadBody(error)),
        };
    Ok((permit, bytes))
}

/// `to_bytes` の失敗が、上限を超えたためか判定する。
fn exceeds_length_limit(error: &axum::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if current.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        source = current.source();
    }
    false
}

/// HTTP の handler が共有する状態。
#[derive(Clone)]
struct HttpState {
    sink: Sink,
    limits: ReceiveLimits,
}

fn detect_encoding(headers: &HeaderMap) -> Result<Encoding, HttpError> {
    let ct = match headers.get(header::CONTENT_TYPE) {
        Some(value) => value
            .to_str()
            .map_err(|_| HttpError::UnsupportedContentType("<invalid header value>".to_string()))?,
        None => PROTOBUF_CT,
    };
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

// Content-Type の判定は枠を取る前に行う (415 で断る request に枠を使わない)。
// decode した request を保存し終えるまで枠を持ち、保存の task へも渡す。

async fn handle_traces(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, HttpError> {
    let encoding = detect_encoding(&headers)?;
    let (permit, body) = receive_body(&state.limits, body).await?;
    let req: ExportTraceServiceRequest = decode_request(encoding, &body)?;
    drop(body);
    state
        .sink
        .record_with_permit(TelemetryRecord::Traces(Box::new(req)), Some(permit))
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
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, HttpError> {
    let encoding = detect_encoding(&headers)?;
    let (permit, body) = receive_body(&state.limits, body).await?;
    let req: ExportMetricsServiceRequest = decode_request(encoding, &body)?;
    drop(body);
    state
        .sink
        .record_with_permit(TelemetryRecord::Metrics(Box::new(req)), Some(permit))
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
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, HttpError> {
    let encoding = detect_encoding(&headers)?;
    let (permit, body) = receive_body(&state.limits, body).await?;
    let req: ExportLogsServiceRequest = decode_request(encoding, &body)?;
    drop(body);
    state
        .sink
        .record_with_permit(TelemetryRecord::Logs(Box::new(req)), Some(permit))
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

async fn handle_stats(State(state): State<HttpState>) -> Json<StatsResponse> {
    let sink = &state.sink;
    let usage = sink.aggregator().snapshot();
    // proxy が設定されていれば route ごとの累計を含めて返す。
    let proxy = sink.proxy().map(|router| {
        router
            .snapshot()
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>()
    });
    Json(StatsResponse { usage, proxy })
}

#[derive(serde::Serialize)]
struct StatsResponse {
    #[serde(flatten)]
    usage: crate::aggregator::UsageSnapshot,
    /// route 名 → 送信累計。未設定なら省略される。
    #[serde(skip_serializing_if = "Option::is_none")]
    proxy: Option<std::collections::BTreeMap<String, crate::forward::RouteMetricsSnapshot>>,
}

/// OTLP/HTTP の router。`limits` は gRPC 側と共有する (受信の上限はプロセス全体で 1 つ)。
pub fn router(sink: Sink, limits: ReceiveLimits) -> Router {
    Router::new()
        .route("/v1/traces", post(handle_traces))
        .route("/v1/metrics", post(handle_metrics))
        .route("/v1/logs", post(handle_logs))
        .route("/healthz", get(health))
        .route("/stats", get(handle_stats))
        .with_state(HttpState { sink, limits })
        // `OTEL_EXPORTER_OTLP_COMPRESSION=gzip` の exporter は body を圧縮して送る。
        // 解凍しないと decode 失敗で 400 を返し、OTLP 仕様上 400 は retry 禁止なので
        // batch が恒久的に失われる。size 上限は handler の `to_bytes` が、ここで展開した
        // 後の body に対して掛ける (仕様が要求する "including after decompression")。
        .layer(RequestDecompressionLayer::new().gzip(true))
}

/// `addr` に OTLP/HTTP の listener を bind する。
///
/// 失敗にはアドレスを付ける (gRPC 側の `server::bind_grpc` と同じ)。付けないと
/// `Address already in use` だけが残り、どのアドレスで衝突したか分からない。
async fn bind(addr: SocketAddr) -> anyhow::Result<TcpListener> {
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind HTTP {addr}"))
}

pub async fn serve(
    addr: SocketAddr,
    sink: Sink,
    limits: ReceiveLimits,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let listener = bind(addr).await?;
    let local = listener.local_addr()?;
    tracing::info!(addr = %local, "OTLP/HTTP server listening");
    let app = router(sink, limits);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JSONL も stdout も出さない最小構成。router のテストで共有する。
    /// テスト用の受信上限 (既定値と同じ)。
    fn test_limits() -> ReceiveLimits {
        ReceiveLimits::new(
            crate::admission::DEFAULT_MAX_INFLIGHT_REQUESTS,
            crate::admission::DEFAULT_RECEIVE_TIMEOUT,
        )
    }

    fn no_output_settings() -> crate::cli::Settings {
        use crate::cli::{ColorMode, Settings};

        Settings {
            grpc_addr: "127.0.0.1:0".parse().unwrap(),
            http_addr: "127.0.0.1:0".parse().unwrap(),
            max_inflight_requests: crate::admission::DEFAULT_MAX_INFLIGHT_REQUESTS,
            receive_timeout: crate::admission::DEFAULT_RECEIVE_TIMEOUT,
            log_sink: None,
            no_stdout: true,
            pretty_log_dir: None,
            summary: false,
            color: ColorMode::Never,
            dry_run: false,
            proxy: None,
        }
    }

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
    fn detect_encoding_rejects_non_utf8_content_type() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_bytes(&[0xFF]).unwrap(),
        );

        let err = detect_encoding(&headers).unwrap_err();
        match err {
            HttpError::UnsupportedContentType(value) => {
                assert_eq!(value, "<invalid header value>")
            }
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
            max_inflight_requests: crate::admission::DEFAULT_MAX_INFLIGHT_REQUESTS,
            receive_timeout: crate::admission::DEFAULT_RECEIVE_TIMEOUT,
            log_sink: None,
            no_stdout: true,
            pretty_log_dir: None,
            summary: false,
            color: ColorMode::Never,
            dry_run: false,
            proxy: None,
        };
        let sink = Sink::from_settings(&settings).await.unwrap();
        let app = router(sink, test_limits());

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

    /// JSONL 永続化に失敗したときは retry 可能な応答を返す。OTLP/HTTP で retry される
    /// のは 429 / 502 / 503 / 504 だけで、500 は "MUST NOT be retried" のため
    /// exporter が batch を drop してしまう。
    #[test]
    fn persistence_error_maps_to_retryable_status() {
        let response = HttpError::Persistence(anyhow::anyhow!("disk full")).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// decode 不能な request は非 retryable で返す。415 / 400 を 5xx に化けさせると、
    /// exporter が壊れた payload を無限に再送し続ける。逆に永続化失敗 (503) を
    /// 400 に化けさせると batch が恒久的に失われるため、この対応は固定する。
    #[test]
    fn decode_errors_map_to_non_retryable_statuses() {
        assert_eq!(
            HttpError::UnsupportedContentType("text/plain".to_string())
                .into_response()
                .status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        let bad_protobuf =
            <ExportLogsServiceRequest as prost::Message>::decode(&[0xffu8, 0xff, 0xff][..])
                .expect_err("不正な protobuf は decode に失敗する");
        assert_eq!(
            HttpError::BadProtobuf(bad_protobuf)
                .into_response()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let bad_json = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert_eq!(
            HttpError::BadJson(bad_json).into_response().status(),
            StatusCode::BAD_REQUEST
        );
    }

    /// `OTLP_MAX_REQUEST_BYTES` を超える body は 413 で拒否する。上限側のテストが
    /// 無いと、定数を過大にしてもメモリ枯渇の防御が外れたことに気づけない。
    #[tokio::test]
    async fn router_rejects_body_larger_than_the_otlp_limit() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        let sink = Sink::from_settings(&no_output_settings()).await.unwrap();
        let app = router(sink, test_limits());
        let body = vec![0u8; crate::server::OTLP_MAX_REQUEST_BYTES + 1];
        let request = Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .header(header::CONTENT_TYPE, PROTOBUF_CT)
            .body(Body::from(body))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// 圧縮後のサイズが小さくても、解凍後に上限を超える body は拒否する。
    #[tokio::test]
    async fn router_rejects_gzip_body_larger_than_the_otlp_limit_after_decompression() {
        use std::io::Write as _;

        use axum::body::Body;
        use axum::http::Request;
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use tower::ServiceExt as _;

        let sink = Sink::from_settings(&no_output_settings()).await.unwrap();
        let app = router(sink, test_limits());
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&vec![0u8; crate::server::OTLP_MAX_REQUEST_BYTES + 1])
            .unwrap();
        let request = Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .header(header::CONTENT_TYPE, PROTOBUF_CT)
            .header(header::CONTENT_ENCODING, "gzip")
            .body(Body::from(encoder.finish().unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// 回帰テスト: gzip の body を送り切らない request は、受け付けからの期限で retryable な
    /// 503 を返し、受信の枠を返す。
    ///
    /// 期限が無かった頃は、終端を送らずに止めた接続が展開後の body (最大 32MiB) を抱えたまま
    /// 居座り、8 接続・合計 245KB の送信で RSS が 15MB から 264MB に増えて戻らなかった。
    #[tokio::test(start_paused = true)]
    async fn stalled_gzip_body_gets_a_retryable_status_at_the_deadline() {
        use std::io::Write as _;

        use axum::http::Request;
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use tonic::codegen::tokio_stream::{self as stream, StreamExt as _};
        use tower::ServiceExt as _;

        let sink = Sink::from_settings(&no_output_settings()).await.unwrap();
        let app = router(
            sink,
            ReceiveLimits::new(1, std::time::Duration::from_millis(200)),
        );
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&vec![0u8; 1024 * 1024]).unwrap();
        let compressed = encoder.finish().unwrap();
        // gzip の前半だけを送り、残りは送らずに止める。
        let head = Bytes::copy_from_slice(&compressed[..compressed.len() / 2]);
        let body = Body::from_stream(
            stream::iter([Ok::<_, std::io::Error>(head)]).chain(stream::pending()),
        );
        let request = Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .header(header::CONTENT_TYPE, PROTOBUF_CT)
            .header(header::CONTENT_ENCODING, "gzip")
            .body(body)
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        // 枠は戻っているので、次の request は通る。
        let request = Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .header(header::CONTENT_TYPE, JSON_CT)
            .body(Body::from(r#"{"resourceLogs":[]}"#))
            .unwrap();
        assert_eq!(app.oneshot(request).await.unwrap().status(), StatusCode::OK);
    }

    /// 処理中の上限に達したら、期限まで空きを待つ。間に合わなければ retryable な 503 を返し、
    /// 期限内に空けば通す (瞬間的な重なりで断ると、retry しない exporter では欠測する)。
    #[tokio::test(start_paused = true)]
    async fn requests_over_the_inflight_limit_wait_for_a_free_slot() {
        use axum::http::Request;
        use tower::ServiceExt as _;

        let sink = Sink::from_settings(&no_output_settings()).await.unwrap();
        let limits = ReceiveLimits::new(1, std::time::Duration::from_millis(200));
        let app = router(sink, limits.clone());
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/v1/logs")
                .header(header::CONTENT_TYPE, JSON_CT)
                .body(Body::from(r#"{"resourceLogs":[]}"#))
                .unwrap()
        };
        let held = limits.admit(limits.deadline()).await.unwrap();

        let response = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let waiting = tokio::spawn(app.oneshot(request()));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(held);
        let response = waiting.await.unwrap().unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Content-Type / Content-Encoding で断る request は、受信の枠を待たずにすぐ 415 を返す。
    ///
    /// 枠を取る前に判定するので、上限が埋まっていても期限まで待たせない。待たせてから
    /// 503 を返すと、retry しても通らない request を exporter に再送させ続ける。
    #[tokio::test(start_paused = true)]
    async fn unsupported_media_is_rejected_without_waiting_for_a_slot() {
        use axum::http::Request;
        use tower::ServiceExt as _;

        let sink = Sink::from_settings(&no_output_settings()).await.unwrap();
        let limits = ReceiveLimits::new(1, std::time::Duration::from_millis(200));
        let app = router(sink, limits.clone());
        let _held = limits.admit(limits.deadline()).await.unwrap();

        for (content_type, content_encoding) in [("text/plain", None), (PROTOBUF_CT, Some("br"))] {
            let mut request = Request::builder()
                .method("POST")
                .uri("/v1/logs")
                .header(header::CONTENT_TYPE, content_type);
            if let Some(encoding) = content_encoding {
                request = request.header(header::CONTENT_ENCODING, encoding);
            }
            let started = tokio::time::Instant::now();
            let response = app
                .clone()
                .oneshot(request.body(Body::from("x")).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{content_type} / {content_encoding:?}"
            );
            assert_eq!(started.elapsed(), std::time::Duration::ZERO, "枠を待たない");
        }
    }

    /// decode できない body・上限を超える body で断った request も、受信の枠を返す。
    ///
    /// 枠が戻らないと、壊れた request を上限の数だけ送るだけで、以後の正常な request が
    /// すべて期限切れの 503 になる。
    #[tokio::test(start_paused = true)]
    async fn rejected_bodies_return_their_slot() {
        use std::io::Write as _;

        use axum::http::Request;
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use tower::ServiceExt as _;

        let sink = Sink::from_settings(&no_output_settings()).await.unwrap();
        let app = router(
            sink,
            ReceiveLimits::new(1, std::time::Duration::from_millis(200)),
        );
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&vec![0u8; crate::server::OTLP_MAX_REQUEST_BYTES + 1])
            .unwrap();
        let too_large = encoder.finish().unwrap();
        let rejected = [
            (
                PROTOBUF_CT,
                None,
                Body::from(vec![0xFF_u8; 16]),
                StatusCode::BAD_REQUEST,
            ),
            (JSON_CT, None, Body::from("{"), StatusCode::BAD_REQUEST),
            (
                PROTOBUF_CT,
                Some("gzip"),
                Body::from(too_large),
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
        ];
        for (content_type, content_encoding, body, status) in rejected {
            let mut request = Request::builder()
                .method("POST")
                .uri("/v1/logs")
                .header(header::CONTENT_TYPE, content_type);
            if let Some(encoding) = content_encoding {
                request = request.header(header::CONTENT_ENCODING, encoding);
            }
            let response = app
                .clone()
                .oneshot(request.body(body).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), status);
        }

        let request = Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .header(header::CONTENT_TYPE, JSON_CT)
            .body(Body::from(r#"{"resourceLogs":[]}"#))
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::OK,
            "断った request の枠は戻っている"
        );
    }

    /// body の上限はちょうど `OTLP_MAX_REQUEST_BYTES` までを含む。1 byte 手前で断ると、
    /// 上限ちょうどの batch が retry できない 413 で失われる。
    #[tokio::test]
    async fn body_of_exactly_the_request_limit_is_not_rejected_by_size() {
        use axum::http::Request;
        use tower::ServiceExt as _;

        let sink = Sink::from_settings(&no_output_settings()).await.unwrap();
        let app = router(sink, test_limits());
        // 中身は不正な protobuf なので、size の上限を通れば decode 失敗の 400 になる。
        let request = Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .header(header::CONTENT_TYPE, PROTOBUF_CT)
            .body(Body::from(vec![
                0xFF_u8;
                crate::server::OTLP_MAX_REQUEST_BYTES
            ]))
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }

    /// `/v1/traces` と `/v1/metrics` も実際に配線されていること。logs だけを叩く
    /// テストしか無いと、handler を取り違えて配線しても気づけない。
    #[tokio::test]
    async fn router_accepts_every_otlp_signal_path() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        for (path, body) in [
            ("/v1/logs", r#"{"resourceLogs":[]}"#),
            ("/v1/traces", r#"{"resourceSpans":[]}"#),
            ("/v1/metrics", r#"{"resourceMetrics":[]}"#),
        ] {
            let sink = Sink::from_settings(&no_output_settings()).await.unwrap();
            let app = router(sink, test_limits());
            let request = Request::builder()
                .method("POST")
                .uri(path)
                .header(header::CONTENT_TYPE, JSON_CT)
                .body(Body::from(body))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{path} が 200 を返さない"
            );
        }
    }

    /// protobuf の既定値を省略した空の JSON request も全 signal で受理・保存する。
    /// 400 は exporter が再送しないため、依存更新でこの境界が退行すると batch が失われる。
    #[tokio::test]
    async fn router_persists_empty_json_requests_with_omitted_resource_fields() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telemetry.jsonl");
        let mut settings = no_output_settings();
        settings.log_sink = Some(crate::cli::LogSink::File(path.clone()));
        let sink = Sink::from_settings(&settings).await.unwrap();
        let app = router(sink, test_limits());
        for signal in ["logs", "traces", "metrics"] {
            let request = Request::builder()
                .method("POST")
                .uri(format!("/v1/{signal}"))
                .header(header::CONTENT_TYPE, JSON_CT)
                .body(Body::from("{}"))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{signal}");
        }

        // ACK の時点で 3 batch がファイルへ書き渡されていることも確認する。
        let saved = std::fs::read_to_string(path).unwrap();
        let records: Vec<serde_json::Value> = saved
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 3);
        for (record, (signal, field)) in records.iter().zip([
            ("logs", "resourceLogs"),
            ("traces", "resourceSpans"),
            ("metrics", "resourceMetrics"),
        ]) {
            assert_eq!(record["kind"], signal);
            assert_eq!(record[field], serde_json::json!([]));
        }
    }

    /// 空の AnyValue と null の oneof field は未設定値として保存し、batch 全体を拒否しない。
    #[tokio::test]
    async fn router_persists_empty_and_null_json_any_values() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telemetry.jsonl");
        let mut settings = no_output_settings();
        settings.log_sink = Some(crate::cli::LogSink::File(path.clone()));
        let sink = Sink::from_settings(&settings).await.unwrap();
        let payload = serde_json::json!({
            "resourceLogs": [{
                "scopeLogs": [{
                    "logRecords": [
                        {"body": {}, "attributes": [{"key": "empty", "value": {}}]},
                        {"body": {"stringValue": null, "intValue": null}}
                    ]
                }]
            }]
        });
        let request = Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .header(header::CONTENT_TYPE, JSON_CT)
            .body(Body::from(payload.to_string()))
            .unwrap();
        let response = router(sink, test_limits()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let saved = std::fs::read_to_string(path).unwrap();
        assert_eq!(saved.lines().count(), 1);
        let mut record: serde_json::Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(
            record.as_object_mut().unwrap().remove("kind").unwrap(),
            "logs"
        );
        let persisted: ExportLogsServiceRequest = serde_json::from_value(record).unwrap();
        let logs = &persisted.resource_logs[0].scope_logs[0].log_records;
        assert_eq!(logs.len(), 2);
        assert!(
            logs.iter()
                .all(|log| log.body.as_ref().unwrap().value.is_none())
        );
        assert_eq!(logs[0].attributes[0].key, "empty");
        assert!(
            logs[0].attributes[0]
                .value
                .as_ref()
                .unwrap()
                .value
                .is_none()
        );
    }

    /// `OTEL_EXPORTER_OTLP_COMPRESSION=gzip` の exporter は body を gzip して送る。
    /// 解凍せずに protobuf decode すると 400 になり、400 は retry 禁止なので
    /// batch が恒久的に失われる。
    #[tokio::test]
    async fn router_accepts_gzip_encoded_body() {
        use std::io::Write as _;

        use axum::body::Body;
        use axum::http::Request;
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use tower::ServiceExt as _;

        use crate::cli::{ColorMode, Settings};

        let settings = Settings {
            grpc_addr: "127.0.0.1:0".parse().unwrap(),
            http_addr: "127.0.0.1:0".parse().unwrap(),
            max_inflight_requests: crate::admission::DEFAULT_MAX_INFLIGHT_REQUESTS,
            receive_timeout: crate::admission::DEFAULT_RECEIVE_TIMEOUT,
            log_sink: None,
            no_stdout: true,
            pretty_log_dir: None,
            summary: false,
            color: ColorMode::Never,
            dry_run: false,
            proxy: None,
        };
        let sink = Sink::from_settings(&settings).await.unwrap();
        let app = router(sink, test_limits());

        // 解凍されなければ gzip のマジックバイトを protobuf として読もうとして失敗する。
        let payload = ExportLogsServiceRequest {
            resource_logs: vec![opentelemetry_proto::tonic::logs::v1::ResourceLogs {
                resource: None,
                scope_logs: vec![],
                schema_url: "https://example.invalid/schema".into(),
            }],
        };
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&payload.encode_to_vec()).unwrap();
        let gzipped = encoder.finish().unwrap();

        let request = Request::builder()
            .method("POST")
            .uri("/v1/logs")
            .header(header::CONTENT_TYPE, PROTOBUF_CT)
            .header(header::CONTENT_ENCODING, "gzip")
            .body(Body::from(gzipped))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "gzip した OTLP request を解凍して受け付けること"
        );
    }

    /// `:0` では割り当てられた port を返す (「listening」のログに出すのはこの値)。
    #[tokio::test]
    async fn bind_reports_the_assigned_port() {
        let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();

        assert_ne!(listener.local_addr().unwrap().port(), 0);
    }

    /// 回帰テスト: 使用中の port では、エラーの chain に衝突したアドレスを残す。
    /// 修正前は `Address already in use` だけで、どのアドレスか分からなかった。
    #[tokio::test]
    async fn bind_error_names_the_address_in_use() {
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = occupied.local_addr().unwrap();

        let error = bind(addr).await.unwrap_err();

        let rendered = format!("{error:#}");
        assert!(rendered.contains(&addr.to_string()), "{rendered}");
    }
}
