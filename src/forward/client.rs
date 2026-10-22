//! 転送クライアントの抽象化と gRPC / HTTP protobuf 実装。
//!
//! 各 client は 1 route に対応し、`ExportRequest` を上流 collector へ送る。
//! 認証 header や TLS 設定は route の構築時に一度だけ組み立てる。

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::logs::v1::logs_service_client::LogsServiceClient;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_client::MetricsServiceClient;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_client::TraceServiceClient;
use prost::Message;
use tonic::Request;
use tonic::metadata::{AsciiMetadataKey, AsciiMetadataValue, MetadataMap};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::cli::ProxyRoute;
use crate::config::ProxyTransport;

use super::ExportRequest;

const OTLP_HTTP_PROTOBUF: &str = "application/x-protobuf";

/// OTLP/HTTP の signal 別パス。endpoint の末尾に足して送信先 URL を作る。
const OTLP_HTTP_LOGS_PATH: &str = "/v1/logs";
const OTLP_HTTP_TRACES_PATH: &str = "/v1/traces";
const OTLP_HTTP_METRICS_PATH: &str = "/v1/metrics";

/// gRPC の `grpc-timeout` header で表せる timeout の上限 (99,999,999 時間)。
///
/// header の値は 8 桁までの整数と単位で、最大の単位は時間 (`H`) なので、これより長い
/// timeout は正しく送れない。tonic は 1 億時間以上の値を送信時に
/// `expect("duration is unrealistically large")` で panic させ、その route の worker が止まる。
const GRPC_TIMEOUT_MAX: Duration = Duration::from_secs(99_999_999 * 60 * 60);

/// route ごとに 1 個作られる送信クライアント。中は Arc で安く clone できる。
#[derive(Clone)]
pub enum RouteClient {
    Grpc(GrpcClient),
    Http(HttpClient),
}

impl RouteClient {
    pub fn build(route: &ProxyRoute, timeout_ms: u64) -> Result<Self> {
        let timeout = Duration::from_millis(timeout_ms);
        match route.transport {
            ProxyTransport::Grpc => Ok(RouteClient::Grpc(GrpcClient::connect(route, timeout)?)),
            ProxyTransport::HttpProtobuf => {
                Ok(RouteClient::Http(HttpClient::build(route, timeout)?))
            }
        }
    }

    pub async fn export(&self, request: ExportRequest) -> Result<()> {
        match self {
            RouteClient::Grpc(c) => c.export(request).await,
            RouteClient::Http(c) => c.export(request).await,
        }
    }
}

/// gRPC (tonic) client。lazy connect。retry はワーカー側で行う。
#[derive(Clone)]
pub struct GrpcClient {
    channel: Channel,
    metadata: MetadataMap,
    timeout: Duration,
}

impl GrpcClient {
    fn connect(route: &ProxyRoute, timeout: Duration) -> Result<Self> {
        let endpoint = build_grpc_endpoint(&route.endpoint, &route.name, timeout)?;
        // `connect_lazy` は最初の RPC まで実際の TCP を確立せず、初回の endpoint down
        // だけで startup を潰さない。
        let channel = endpoint.connect_lazy();
        let mut metadata = MetadataMap::new();
        for (key, value) in &route.headers {
            let name = AsciiMetadataKey::from_str(&key.to_ascii_lowercase())
                .with_context(|| format!("invalid metadata key for route `{}`", route.name))?;
            let val = AsciiMetadataValue::try_from(value.as_str()).with_context(|| {
                format!(
                    "invalid metadata value for header `{key}` on route `{}`",
                    route.name
                )
            })?;
            metadata.insert(name, val);
        }
        Ok(Self {
            channel,
            metadata,
            timeout,
        })
    }

    fn attach_metadata<T>(&self, mut req: Request<T>) -> Request<T> {
        // MetadataMap を per-request に clone して挿入する。
        for kv in self.metadata.iter() {
            match kv {
                tonic::metadata::KeyAndValueRef::Ascii(k, v) => {
                    req.metadata_mut().insert(k.clone(), v.clone());
                }
                tonic::metadata::KeyAndValueRef::Binary(k, v) => {
                    req.metadata_mut().insert_bin(k.clone(), v.clone());
                }
            }
        }
        req.set_timeout(self.timeout);
        req
    }

    async fn export(&self, request: ExportRequest) -> Result<()> {
        match request {
            ExportRequest::Logs(req) => {
                let mut client = LogsServiceClient::new(self.channel.clone());
                let mut r: Request<ExportLogsServiceRequest> = Request::new(*req);
                r = self.attach_metadata(r);
                let resp = client.export(r).await.context("gRPC logs export")?;
                warn_partial_logs(resp.get_ref().partial_success.as_ref());
                Ok(())
            }
            ExportRequest::Traces(req) => {
                let mut client = TraceServiceClient::new(self.channel.clone());
                let mut r: Request<ExportTraceServiceRequest> = Request::new(*req);
                r = self.attach_metadata(r);
                let resp = client.export(r).await.context("gRPC traces export")?;
                warn_partial_traces(resp.get_ref().partial_success.as_ref());
                Ok(())
            }
            ExportRequest::Metrics(req) => {
                let mut client = MetricsServiceClient::new(self.channel.clone());
                let mut r: Request<ExportMetricsServiceRequest> = Request::new(*req);
                r = self.attach_metadata(r);
                let resp = client.export(r).await.context("gRPC metrics export")?;
                warn_partial_metrics(resp.get_ref().partial_success.as_ref());
                Ok(())
            }
        }
    }
}

/// proxy endpoint を transport 非依存で検証する。
///
/// エラーには endpoint 自体を載せない。userinfo (`https://user:pass@host`) や query に
/// 資格情報を書かれていると、起動失敗のメッセージそのものが漏洩経路になるため、
/// 診断には route 名だけを使う。
fn validate_proxy_endpoint(raw: &str, route: &str) -> Result<()> {
    let url =
        reqwest::Url::parse(raw).with_context(|| format!("parse endpoint for route `{route}`"))?;
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
        bail!("endpoint for route `{route}` must be an absolute HTTP(S) URL");
    }
    // OTLP の signal 別パスを endpoint 末尾に足す実装なので、query / fragment 付きは
    // 組み立てた URL が壊れる。起動時に弾く。
    if url.query().is_some() || url.fragment().is_some() {
        bail!("endpoint for route `{route}` must not contain a query or fragment");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!(
            "endpoint for route `{route}` must not embed credentials; use a proxy header with `env:VAR` instead"
        );
    }
    Ok(())
}

fn build_grpc_endpoint(url: &str, route: &str, timeout: Duration) -> Result<Endpoint> {
    // gRPC は既定の transport なので、HTTP protobuf と同じ検証を必ず通す。
    // 検証を欠くと userinfo が HTTP/2 の `:authority` に載り (RFC 9113 §8.3.1 違反)、
    // scheme 無しの endpoint は起動に成功したうえで全送信が恒久的に失敗する。
    validate_proxy_endpoint(url, route)?;
    // `grpc-timeout` に書けない値は、最初の送信 (`attach_metadata` の `set_timeout`) で
    // tonic が panic させる。worker が止まると以後その route の batch はすべて捨てられる
    // ため、queue 容量と同じく起動時の設定エラーにする。
    if timeout > GRPC_TIMEOUT_MAX {
        bail!(
            "proxy timeout-ms {} for route `{route}` exceeds the gRPC maximum of {} ms",
            timeout.as_millis(),
            GRPC_TIMEOUT_MAX.as_millis()
        );
    }
    let ep = Endpoint::from_shared(url.to_string())
        .with_context(|| format!("build gRPC endpoint for route `{route}`"))?
        .timeout(timeout)
        .connect_timeout(timeout)
        .tcp_keepalive(Some(Duration::from_secs(60)));
    // HTTPS ならデフォルト TLS 設定 (webpki-roots ベース) を有効化する。
    let uri =
        http::Uri::try_from(url).with_context(|| format!("build gRPC URI for route `{route}`"))?;
    let ep = match uri.scheme_str() {
        Some("https") => ep.tls_config(ClientTlsConfig::new().with_webpki_roots())?,
        _ => ep,
    };
    Ok(ep)
}

fn warn_partial_logs(
    ps: Option<&opentelemetry_proto::tonic::collector::logs::v1::ExportLogsPartialSuccess>,
) {
    if let Some(ps) = ps
        && (ps.rejected_log_records != 0 || !ps.error_message.is_empty())
    {
        tracing::warn!(
            rejected = ps.rejected_log_records,
            // 上流が自由に設定できる文字列なので、Debug で制御文字を escape して出す。
            reason = ?ps.error_message,
            "proxy: upstream partially rejected logs"
        );
    }
}

fn warn_partial_traces(
    ps: Option<&opentelemetry_proto::tonic::collector::trace::v1::ExportTracePartialSuccess>,
) {
    if let Some(ps) = ps
        && (ps.rejected_spans != 0 || !ps.error_message.is_empty())
    {
        tracing::warn!(
            rejected = ps.rejected_spans,
            // 上流が自由に設定できる文字列なので、Debug で制御文字を escape して出す。
            reason = ?ps.error_message,
            "proxy: upstream partially rejected spans"
        );
    }
}

fn warn_partial_metrics(
    ps: Option<&opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsPartialSuccess>,
) {
    if let Some(ps) = ps
        && (ps.rejected_data_points != 0 || !ps.error_message.is_empty())
    {
        tracing::warn!(
            rejected = ps.rejected_data_points,
            // 上流が自由に設定できる文字列なので、Debug で制御文字を escape して出す。
            reason = ?ps.error_message,
            "proxy: upstream partially rejected data points"
        );
    }
}

/// OTLP/HTTP protobuf client (reqwest ベース)。
#[derive(Clone)]
pub struct HttpClient {
    client: reqwest::Client,
    urls: std::sync::Arc<HttpUrls>,
    headers: reqwest::header::HeaderMap,
}

/// clone 時に signal 別 URL の内部バッファを共有する。
struct HttpUrls {
    logs_url: reqwest::Url,
    traces_url: reqwest::Url,
    metrics_url: reqwest::Url,
}

impl HttpClient {
    fn build(route: &ProxyRoute, timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .tcp_keepalive(Some(Duration::from_secs(60)))
            // OTLP endpoint は明示指定なので redirect を追う正当性がない。既定 (最大 10 回)
            // のままだと、`api-key` のようなカスタム認証ヘッダは cross-host でも除去されず、
            // 307/308 では body ごと別 origin へ再送される。3xx は恒久的な送信失敗とする。
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build reqwest client")?;
        // gRPC 経路と同じ検証を通す。エラーに endpoint を載せないのも共通。
        validate_proxy_endpoint(&route.endpoint, &route.name)?;

        // 検証済みのベース URL に OTLP の signal 別パスを追加する。
        let base = route.endpoint.trim_end_matches('/');
        let signal_url = |path: &str| {
            reqwest::Url::parse(&format!("{base}{path}"))
                .with_context(|| format!("build OTLP/HTTP URL `{path}` for route `{}`", route.name))
        };
        let logs_url = signal_url(OTLP_HTTP_LOGS_PATH)?;
        let traces_url = signal_url(OTLP_HTTP_TRACES_PATH)?;
        let metrics_url = signal_url(OTLP_HTTP_METRICS_PATH)?;

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static(OTLP_HTTP_PROTOBUF),
        );
        for (key, value) in &route.headers {
            let name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
                .with_context(|| format!("invalid HTTP header key on route `{}`", route.name))?;
            let val = reqwest::header::HeaderValue::from_str(value).with_context(|| {
                format!(
                    "invalid HTTP header value for `{key}` on route `{}`",
                    route.name
                )
            })?;
            headers.insert(name, val);
        }
        Ok(Self {
            client,
            urls: std::sync::Arc::new(HttpUrls {
                logs_url,
                traces_url,
                metrics_url,
            }),
            headers,
        })
    }

    async fn export(&self, request: ExportRequest) -> Result<()> {
        let (url, signal_path, body) = match request {
            ExportRequest::Logs(req) => (
                &self.urls.logs_url,
                OTLP_HTTP_LOGS_PATH,
                encode_pb(req.as_ref()),
            ),
            ExportRequest::Traces(req) => (
                &self.urls.traces_url,
                OTLP_HTTP_TRACES_PATH,
                encode_pb(req.as_ref()),
            ),
            ExportRequest::Metrics(req) => (
                &self.urls.metrics_url,
                OTLP_HTTP_METRICS_PATH,
                encode_pb(req.as_ref()),
            ),
        };
        let response = self
            .client
            .post(url.clone())
            .headers(self.headers.clone())
            .body(Bytes::from(body))
            .send()
            .await
            // URL は出さない。reqwest の Display は URL 全体を含み、worker は原因まで
            // chain ごとログに残す。endpoint の path に秘密を埋める送信先もあるため、
            // 診断には signal path だけを使う。
            .map_err(reqwest::Error::without_url)
            .with_context(|| format!("HTTP POST to upstream (signal path {signal_path})"))?;
        let status = response.status();
        if !status.is_success() {
            // 本文は読まない。`text()` は全量をメモリに展開する (gzip なら展開後のサイズ) ため、
            // 読んでから切り詰めても上限にならず、中身も上流が自由に決められる文字列なので
            // そのまま stderr に出すと terminal escape injection の経路になる。
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after);
            return Err(HttpExportFailure {
                status,
                retry_after,
            }
            .into());
        }
        // 2xx かつ body 全体を discard。partial success の解析は今のところ省略 (Phase B で追加)。
        drop(response);
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("upstream returned HTTP {status}")]
struct HttpExportFailure {
    status: reqwest::StatusCode,
    retry_after: Option<Duration>,
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    let delay = value
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            httpdate::parse_http_date(value).ok().map(|date| {
                date.duration_since(std::time::SystemTime::now())
                    .unwrap_or_default()
            })
        })?;
    // 外部入力が巨大でも tokio の deadline 計算を overflow させない。
    std::time::Instant::now().checked_add(delay).map(|_| delay)
}

/// `Some` は retry 可能な失敗、値は上流が指定した最小待機時間。
/// OTLP が retry を禁止する status は即座に worker へ返す。
pub(super) fn retry_delay(error: &anyhow::Error) -> Option<Duration> {
    if let Some(error) = error.downcast_ref::<HttpExportFailure>() {
        return matches!(error.status.as_u16(), 429 | 502 | 503 | 504)
            .then(|| error.retry_after.unwrap_or_default());
    }
    if let Some(status) = error.downcast_ref::<tonic::Status>() {
        return match status.code() {
            tonic::Code::Cancelled
            | tonic::Code::DeadlineExceeded
            | tonic::Code::Aborted
            | tonic::Code::OutOfRange
            | tonic::Code::Unavailable
            | tonic::Code::DataLoss => Some(grpc_retry_info(status).unwrap_or_default()),
            tonic::Code::ResourceExhausted => grpc_retry_info(status),
            // 上流から status を受け取る前に接続が切れても (接続直後の FIN / RST、GOAWAY)、
            // tonic は `Unknown` / `Internal` を作る。code だけで判定すると、HTTP の接続断と
            // 違って 1 回で failed になる。
            tonic::Code::Unknown | tonic::Code::Internal if is_grpc_transport_failure(status) => {
                Some(Duration::ZERO)
            }
            _ => None,
        };
    }
    // 接続断など、上流から status を受け取っていない transport 障害は再送できる。
    error
        .downcast_ref::<reqwest::Error>()
        .map(|_| Duration::ZERO)
}

/// 上流が返した status ではなく、tonic が channel の transport 障害から作った status か判定する。
///
/// 応答ヘッダーより前の失敗は、`Channel` が返す `tonic::transport::Error` が source に付く。
/// 上流が返した status は source を持たない。応答ヘッダーを受け取った後の body の失敗は
/// source が hyper の error になるが、unary RPC の応答ヘッダーは上流が処理を終えてから
/// 届くため、再送すると二重に取り込ませるおそれがあり対象にしない (HTTP 経路も
/// 応答ヘッダーを受け取ったら status code だけで判定する)。
fn is_grpc_transport_failure(status: &tonic::Status) -> bool {
    std::error::Error::source(status).is_some_and(|source| source.is::<tonic::transport::Error>())
}

/// google.rpc.Status / Any / RetryInfo の必要な field だけを読む。
/// ResourceExhausted は有効な RetryInfo を伴う場合だけ retry できる。
#[derive(prost::Message)]
struct RpcStatus {
    #[prost(message, repeated, tag = "3")]
    details: Vec<RpcAny>,
}

#[derive(prost::Message)]
struct RpcAny {
    #[prost(string, tag = "1")]
    type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    value: Vec<u8>,
}

#[derive(prost::Message)]
struct RpcRetryInfo {
    #[prost(message, optional, tag = "1")]
    retry_delay: Option<RpcDuration>,
}

#[derive(prost::Message)]
struct RpcDuration {
    #[prost(int64, tag = "1")]
    seconds: i64,
    #[prost(int32, tag = "2")]
    nanos: i32,
}

fn grpc_retry_info(status: &tonic::Status) -> Option<Duration> {
    let decoded = RpcStatus::decode(status.details()).ok()?;
    decoded.details.iter().find_map(|detail| {
        if detail.type_url.rsplit('/').next() != Some("google.rpc.RetryInfo") {
            return None;
        }
        let info = RpcRetryInfo::decode(detail.value.as_slice()).ok()?;
        let Some(duration) = info.retry_delay else {
            return Some(Duration::ZERO);
        };
        if !(0..=315_576_000_000).contains(&duration.seconds)
            || !(0..1_000_000_000).contains(&duration.nanos)
        {
            return None;
        }
        let delay = Duration::new(duration.seconds as u64, duration.nanos as u32);
        std::time::Instant::now().checked_add(delay).map(|_| delay)
    })
}

fn encode_pb<M: Message>(msg: &M) -> Vec<u8> {
    let mut buf = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut buf)
        .expect("prost encode never fails on Vec");
    buf
}

#[allow(dead_code)]
fn from_shared_uri(input: &str) -> Result<http::Uri> {
    http::Uri::from_str(input).map_err(|e| anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ProxyRoute;
    use crate::config::ProxySignal;
    use axum::Router;
    use axum::body::Bytes as AxumBytes;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::http::StatusCode;
    use axum::routing::post;
    use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceResponse;
    use opentelemetry_proto::tonic::logs::v1::ResourceLogs;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    #[test]
    fn http_retry_policy_accepts_only_otlp_retryable_statuses() {
        for code in 300..600 {
            let error = HttpExportFailure {
                status: reqwest::StatusCode::from_u16(code).unwrap(),
                retry_after: Some(Duration::from_secs(3)),
            };
            assert_eq!(
                retry_delay(&error.into()),
                matches!(code, 429 | 502 | 503 | 504).then_some(Duration::from_secs(3)),
                "HTTP {code}"
            );
        }
    }

    #[test]
    fn grpc_retry_policy_requires_retry_info_for_resource_exhausted() {
        for code in 1..=16 {
            let code = tonic::Code::from_i32(code);
            let error =
                anyhow::Error::new(tonic::Status::new(code, "untrusted message")).context("export");
            assert_eq!(
                retry_delay(&error).is_some(),
                matches!(
                    code,
                    tonic::Code::Cancelled
                        | tonic::Code::DeadlineExceeded
                        | tonic::Code::Aborted
                        | tonic::Code::OutOfRange
                        | tonic::Code::Unavailable
                        | tonic::Code::DataLoss
                )
            );
        }
        let retry = RpcRetryInfo {
            retry_delay: Some(RpcDuration {
                seconds: 2,
                nanos: 500_000_000,
            }),
        };
        let details = RpcStatus {
            details: vec![RpcAny {
                type_url: "type.googleapis.com/google.rpc.RetryInfo".to_string(),
                value: retry.encode_to_vec(),
            }],
        }
        .encode_to_vec();
        let status =
            tonic::Status::with_details(tonic::Code::ResourceExhausted, "busy", details.into());
        assert_eq!(
            retry_delay(&status.into()),
            Some(Duration::from_millis(2500))
        );
        let invalid = tonic::Status::with_details(
            tonic::Code::ResourceExhausted,
            "busy",
            Bytes::from_static(b"bad"),
        );
        assert_eq!(retry_delay(&invalid.into()), None);
        for (delay, expected) in [
            (None, Some(Duration::ZERO)),
            (
                Some(RpcDuration {
                    seconds: -1,
                    nanos: 0,
                }),
                None,
            ),
            (
                Some(RpcDuration {
                    seconds: 0,
                    nanos: 1_000_000_000,
                }),
                None,
            ),
            (
                Some(RpcDuration {
                    seconds: 315_576_000_001,
                    nanos: 0,
                }),
                None,
            ),
        ] {
            let details = RpcStatus {
                details: vec![RpcAny {
                    type_url: "type.googleapis.com/google.rpc.RetryInfo".into(),
                    value: RpcRetryInfo { retry_delay: delay }.encode_to_vec(),
                }],
            }
            .encode_to_vec();
            let status =
                tonic::Status::with_details(tonic::Code::ResourceExhausted, "busy", details.into());
            assert_eq!(retry_delay(&status.into()), expected);
        }
    }

    /// tonic は上流から status を受け取る前の transport 障害も `Unknown` / `Internal` にする。
    /// その場合は source に `tonic::transport::Error` が付くので、それだけを再送する。
    /// source を持たない status (上流が返したもの) は上の test で code だけで判定している。
    #[test]
    fn grpc_retry_policy_retries_unknown_and_internal_only_for_transport_failures() {
        let with_source = |code, source: Arc<dyn std::error::Error + Send + Sync>| {
            let mut status = tonic::Status::new(code, "untrusted message");
            status.set_source(source);
            anyhow::Error::new(status).context("export")
        };
        for code in 1..=16 {
            let code = tonic::Code::from_i32(code);
            let retryable = matches!(
                code,
                tonic::Code::Cancelled
                    | tonic::Code::DeadlineExceeded
                    | tonic::Code::Aborted
                    | tonic::Code::OutOfRange
                    | tonic::Code::Unavailable
                    | tonic::Code::DataLoss
            );
            let transport = Endpoint::from_shared("not a uri").expect_err("空白を含む URI は不正");
            assert_eq!(
                retry_delay(&with_source(code, Arc::new(transport))),
                (retryable || matches!(code, tonic::Code::Unknown | tonic::Code::Internal))
                    .then_some(Duration::ZERO),
                "{code:?} (transport 障害)"
            );
            let other = std::io::Error::other("not a transport failure");
            assert_eq!(
                retry_delay(&with_source(code, Arc::new(other))).is_some(),
                retryable,
                "{code:?} (transport 以外の source)"
            );
        }
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        assert_eq!(parse_retry_after("7"), Some(Duration::from_secs(7)));
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(Duration::ZERO)
        );
        let date = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(60));
        let delay = parse_retry_after(&date).unwrap();
        assert!(delay > Duration::from_secs(58) && delay <= Duration::from_secs(60));
        assert_eq!(parse_retry_after("invalid"), None);
        assert_eq!(parse_retry_after("18446744073709551615"), None);
    }

    #[tokio::test]
    async fn client_header_validation_does_not_echo_an_invalid_key() {
        for transport in [ProxyTransport::Grpc, ProxyTransport::HttpProtobuf] {
            let route = ProxyRoute {
                name: "test".to_string(),
                service_names: vec![],
                signals: ProxySignal::ALL.to_vec(),
                transport,
                endpoint: "http://localhost:4317".to_string(),
                headers: vec![(
                    "Authorization: Basic example-secret".to_string(),
                    "".to_string(),
                )],
            };
            let error = match RouteClient::build(&route, 3000) {
                Ok(_) => panic!("不正な header を受理した"),
                Err(error) => format!("{error:#}"),
            };
            assert!(!error.contains("example-secret"));
        }
    }

    #[derive(Default)]
    struct MockState {
        body: Vec<u8>,
        content_type: Option<String>,
        auth: Option<String>,
    }

    async fn logs_handler(
        State(state): State<Arc<Mutex<MockState>>>,
        headers: HeaderMap,
        body: AxumBytes,
    ) -> (
        StatusCode,
        [(axum::http::HeaderName, axum::http::HeaderValue); 1],
        Vec<u8>,
    ) {
        let mut g = state.lock().await;
        g.body = body.to_vec();
        g.content_type = headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok().map(str::to_owned));
        g.auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok().map(str::to_owned));

        let resp = ExportLogsServiceResponse {
            partial_success: None,
        };
        let mut buf = Vec::new();
        resp.encode(&mut buf).unwrap();
        (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static(OTLP_HTTP_PROTOBUF),
            )],
            buf,
        )
    }

    async fn spawn_mock_http() -> (String, Arc<Mutex<MockState>>) {
        let state = Arc::new(Mutex::new(MockState::default()));
        let router = Router::new()
            .route("/v1/logs", post(logs_handler))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{}", addr), state)
    }

    fn logs_request_with_service(service: &str) -> ExportLogsServiceRequest {
        use opentelemetry_proto::tonic::common::v1::any_value::Value as OtlpValue;
        use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue};
        use opentelemetry_proto::tonic::logs::v1::ScopeLogs;
        use opentelemetry_proto::tonic::resource::v1::Resource;

        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "service.name".to_string(),
                        value: Some(AnyValue {
                            value: Some(OtlpValue::StringValue(service.to_string())),
                        }),
                        key_strindex: 0,
                    }],
                    dropped_attributes_count: 0,
                    entity_refs: vec![],
                }),
                scope_logs: vec![ScopeLogs {
                    scope: Some(InstrumentationScope::default()),
                    log_records: vec![],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        }
    }

    #[tokio::test]
    async fn http_client_sends_protobuf_body_and_auth_header() {
        let (endpoint, state) = spawn_mock_http().await;

        let route = ProxyRoute {
            name: "anthropic".to_string(),
            service_names: vec!["claude-code".to_string()],
            signals: ProxySignal::ALL.to_vec(),
            transport: ProxyTransport::HttpProtobuf,
            endpoint,
            headers: vec![("Authorization".to_string(), "Bearer abc123".to_string())],
        };
        let client = RouteClient::build(&route, 3000).unwrap();

        let req = logs_request_with_service("claude-code");
        client
            .export(ExportRequest::Logs(Box::new(req.clone())))
            .await
            .expect("HTTP send should succeed");

        let g = state.lock().await;
        assert_eq!(g.content_type.as_deref(), Some(OTLP_HTTP_PROTOBUF));
        assert_eq!(g.auth.as_deref(), Some("Bearer abc123"));

        // 送信された body が元の request と等価な protobuf であること。
        let decoded = ExportLogsServiceRequest::decode(g.body.as_slice()).unwrap();
        assert_eq!(decoded.resource_logs.len(), req.resource_logs.len());
    }

    #[tokio::test]
    async fn http_client_reports_non_2xx_as_error() {
        // 500 を返すだけの mock server。
        async fn always_500() -> StatusCode {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        let router = Router::new().route("/v1/logs", post(always_500));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let route = ProxyRoute {
            name: "openai".to_string(),
            service_names: vec!["codex_cli_rs".to_string()],
            signals: ProxySignal::ALL.to_vec(),
            transport: ProxyTransport::HttpProtobuf,
            endpoint: format!("http://{addr}"),
            headers: Vec::new(),
        };
        let client = RouteClient::build(&route, 3000).unwrap();

        let req = logs_request_with_service("codex_cli_rs");
        let result = client.export(ExportRequest::Logs(Box::new(req))).await;
        let err = result.unwrap_err();
        assert!(err.to_string().contains("500"), "got: {err}");
    }

    /// リダイレクト先へカスタム認証ヘッダと payload を送らない。
    #[tokio::test]
    async fn http_redirect_does_not_forward_credentials_or_payload() {
        for status in [
            StatusCode::TEMPORARY_REDIRECT,
            StatusCode::PERMANENT_REDIRECT,
        ] {
            let (destination, received) = spawn_mock_http().await;
            let location = format!("{destination}/v1/logs");
            let router = Router::new().route(
                "/v1/logs",
                post(move || {
                    let location = location.clone();
                    async move { (status, [(axum::http::header::LOCATION, location)]) }
                }),
            );
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let route = ProxyRoute {
                name: "redirect-test".into(),
                service_names: vec!["claude-code".into()],
                signals: ProxySignal::ALL.to_vec(),
                transport: ProxyTransport::HttpProtobuf,
                endpoint: format!("http://{addr}"),
                headers: vec![("api-key".into(), "test-secret".into())],
            };
            let client = RouteClient::build(&route, 3000).unwrap();
            let error = client
                .export(ExportRequest::Logs(Box::new(logs_request_with_service(
                    "claude-code",
                ))))
                .await
                .unwrap_err();
            server.abort();

            assert_eq!(
                error.downcast_ref::<HttpExportFailure>().unwrap().status,
                status
            );
            assert_eq!(retry_delay(&error), None, "3xx は再送しない");
            assert!(
                received.lock().await.body.is_empty(),
                "別 origin へ転送しない"
            );
        }
    }

    /// 非 2xx は本文を待たずに判定する。巨大な本文や止まった本文を読み込まない。
    #[tokio::test]
    async fn http_error_status_does_not_wait_for_the_response_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut buf = [0; 1024];
                let read = socket.read(&mut buf).await.unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buf[..read]);
            }
            socket
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 1000000000\r\nRetry-After: 7\r\n\r\n")
                .await
                .unwrap();
            // 本文を送らずに接続を保つ。client の全体 timeout より前に status が返る必要がある。
            std::future::pending::<()>().await;
            drop(socket);
        });
        let route = ProxyRoute {
            name: "stalled-body-test".into(),
            service_names: vec!["claude-code".into()],
            signals: ProxySignal::ALL.to_vec(),
            transport: ProxyTransport::HttpProtobuf,
            endpoint: format!("http://{addr}"),
            headers: Vec::new(),
        };
        let client = RouteClient::build(&route, 30_000).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.export(ExportRequest::Logs(Box::new(logs_request_with_service(
                "claude-code",
            )))),
        )
        .await;
        server.abort();
        let error = result
            .expect("本文の完了を待たずに status を返す")
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<HttpExportFailure>().unwrap().status,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(retry_delay(&error), Some(Duration::from_secs(7)));
    }

    /// 送信失敗の chain に原因を残しつつ、URL は載せない。
    ///
    /// reqwest の Display は URL 全体を含むため、そのまま包むと worker のログに endpoint が
    /// 残る。endpoint の path に秘密を埋める送信先もあるので、診断には signal path だけを使う。
    #[tokio::test]
    async fn http_send_error_keeps_the_cause_without_the_url() {
        // bind した listener をすぐ閉じ、接続を受け付けない port を作る。
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let route = ProxyRoute {
            name: "openai".to_string(),
            service_names: vec![],
            signals: ProxySignal::ALL.to_vec(),
            transport: ProxyTransport::HttpProtobuf,
            endpoint: format!("http://{addr}/ingest/path-secret/"),
            headers: vec![],
        };
        let client = RouteClient::build(&route, 3_000).unwrap();

        let error = client
            .export(ExportRequest::Logs(Box::default()))
            .await
            .expect_err("接続を受け付けない port へは送信できない");

        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("signal path /v1/logs") && rendered.contains("error sending request"),
            "signal path と送信失敗の原因を含む必要がある: {rendered}"
        );
        assert!(
            !rendered.contains("http://127.0.0.1:") && !rendered.contains("path-secret"),
            "URL や endpoint の path を載せてはいけない: {rendered}"
        );
        assert!(
            retry_delay(&error).is_some(),
            "上流の応答を受け取る前の失敗は再送できる"
        );
    }

    #[test]
    fn http_client_rejects_invalid_endpoint_at_startup() {
        for endpoint in INVALID_ENDPOINTS {
            assert_endpoint_rejected(ProxyTransport::HttpProtobuf, endpoint);
        }
    }

    /// gRPC は既定 transport なので、HTTP protobuf と同じ検証を必ず通す必要がある。
    /// 検証が無いと userinfo が HTTP/2 の `:authority` に載り (RFC 9113 §8.3.1 違反)、
    /// scheme 無しの endpoint は起動に成功したうえで全送信が恒久的に失敗する。
    #[test]
    fn grpc_client_rejects_invalid_endpoint_at_startup() {
        for endpoint in INVALID_ENDPOINTS {
            assert_endpoint_rejected(ProxyTransport::Grpc, endpoint);
        }
    }

    const INVALID_ENDPOINTS: &[&str] = &[
        "",
        "collector.example.com",
        "ftp://collector.example.com",
        "https://collector.example.com?tenant=example",
        "https://collector.example.com/#fragment",
        // userinfo は資格情報そのもの。エラーにも出してはいけない。
        "https://user:s3cr3tPASSWORD@collector.example.com:4317",
    ];

    fn assert_endpoint_rejected(transport: ProxyTransport, endpoint: &str) {
        let route = ProxyRoute {
            name: "openai".to_string(),
            service_names: vec!["codex_cli_rs".to_string()],
            signals: ProxySignal::ALL.to_vec(),
            transport,
            endpoint: endpoint.to_string(),
            headers: vec![],
        };

        let error = match RouteClient::build(&route, 3_000) {
            Ok(_) => panic!("不正な endpoint `{endpoint}` が {transport:?} で受理された"),
            Err(error) => error,
        };
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("openai"),
            "route 名を含むエラーを返す必要がある: {rendered}"
        );
        assert!(
            !rendered.contains("s3cr3tPASSWORD") && !rendered.contains("tenant=example"),
            "endpoint 由来の資格情報や query をエラーへ載せてはいけない: {rendered}"
        );
    }

    /// 回帰テスト: tonic は `grpc-timeout` header に書けない timeout を送信時に panic させ、
    /// その route の worker が止まっていた。起動時に route 名付きのエラーで拒否する。
    #[tokio::test]
    async fn grpc_timeout_beyond_the_header_limit_is_rejected_at_startup() {
        // bind した listener をすぐ閉じ、接続を受け付けない port を作る。
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let route = ProxyRoute {
            name: "openai".to_string(),
            service_names: vec![],
            signals: ProxySignal::ALL.to_vec(),
            transport: ProxyTransport::Grpc,
            endpoint: format!("http://{addr}"),
            headers: vec![],
        };
        let max_ms = u64::try_from(GRPC_TIMEOUT_MAX.as_millis()).unwrap();
        // 360,000,000,000,000 ms (1 億時間) は tonic が panic させる最小の値。
        for timeout_ms in [max_ms + 1, 360_000_000_000_000, u64::MAX] {
            let error = match RouteClient::build(&route, timeout_ms) {
                Ok(_) => panic!("gRPC で表せない timeout {timeout_ms} ms を受理した"),
                Err(error) => format!("{error:#}"),
            };
            assert!(
                error.contains("openai"),
                "route 名を含むエラーを返す必要がある: {error}"
            );
        }

        let client =
            RouteClient::build(&route, max_ms).expect("上限ちょうどの timeout は受け付ける");
        // 送信時に `grpc-timeout` header を組み立てても panic せず、接続の失敗として返る。
        assert!(
            client
                .export(ExportRequest::Logs(Box::default()))
                .await
                .is_err()
        );
    }

    #[test]
    fn http_client_appends_signal_paths_after_base_path() {
        let route = ProxyRoute {
            name: "openai".to_string(),
            service_names: vec!["codex_cli_rs".to_string()],
            signals: ProxySignal::ALL.to_vec(),
            transport: ProxyTransport::HttpProtobuf,
            endpoint: "https://collector.example.com/otlp/".to_string(),
            headers: vec![],
        };

        let client = match RouteClient::build(&route, 3_000).expect("正しい endpoint を受理する")
        {
            RouteClient::Http(client) => client,
            RouteClient::Grpc(_) => panic!("HTTP route が gRPC client になった"),
        };
        assert_eq!(
            client.urls.logs_url.as_str(),
            "https://collector.example.com/otlp/v1/logs"
        );
        assert_eq!(
            client.urls.traces_url.as_str(),
            "https://collector.example.com/otlp/v1/traces"
        );
        assert_eq!(
            client.urls.metrics_url.as_str(),
            "https://collector.example.com/otlp/v1/metrics"
        );
    }
}
