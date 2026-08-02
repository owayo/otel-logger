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
        let endpoint = build_grpc_endpoint(&route.endpoint, timeout)
            .with_context(|| format!("build gRPC endpoint for route `{}`", route.name))?;
        // `connect_lazy` は最初の RPC まで実際の TCP を確立せず、初回の endpoint down
        // だけで startup を潰さない。
        let channel = endpoint.connect_lazy();
        let mut metadata = MetadataMap::new();
        for (key, value) in &route.headers {
            let name =
                AsciiMetadataKey::from_str(&key.to_ascii_lowercase()).with_context(|| {
                    format!("invalid metadata key `{key}` for route `{}`", route.name)
                })?;
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

fn build_grpc_endpoint(url: &str, timeout: Duration) -> Result<Endpoint> {
    let ep = Endpoint::from_shared(url.to_string())
        .with_context(|| format!("parse gRPC endpoint `{url}`"))?
        .timeout(timeout)
        .connect_timeout(timeout)
        .tcp_keepalive(Some(Duration::from_secs(60)));
    // HTTPS ならデフォルト TLS 設定 (webpki-roots ベース) を有効化する。
    let uri = http::Uri::try_from(url).with_context(|| format!("parse URI `{url}`"))?;
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
            reason = %ps.error_message,
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
            reason = %ps.error_message,
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
            reason = %ps.error_message,
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
            .build()
            .context("build reqwest client")?;
        let endpoint = reqwest::Url::parse(&route.endpoint).with_context(|| {
            format!(
                "parse HTTP endpoint `{}` for route `{}`",
                route.endpoint, route.name
            )
        })?;
        if !matches!(endpoint.scheme(), "http" | "https") || !endpoint.has_host() {
            bail!(
                "HTTP endpoint `{}` for route `{}` must be an absolute HTTP(S) URL",
                route.endpoint,
                route.name
            );
        }
        if endpoint.query().is_some() || endpoint.fragment().is_some() {
            bail!(
                "HTTP endpoint `{}` for route `{}` must not contain a query or fragment",
                route.endpoint,
                route.name
            );
        }

        // 検証済みのベース URL に OTLP の signal 別パスを追加する。
        let base = route.endpoint.trim_end_matches('/');
        let signal_url = |path: &str| {
            reqwest::Url::parse(&format!("{base}{path}"))
                .with_context(|| format!("build OTLP/HTTP URL `{path}` for route `{}`", route.name))
        };
        let logs_url = signal_url("/v1/logs")?;
        let traces_url = signal_url("/v1/traces")?;
        let metrics_url = signal_url("/v1/metrics")?;

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static(OTLP_HTTP_PROTOBUF),
        );
        for (key, value) in &route.headers {
            let name =
                reqwest::header::HeaderName::from_bytes(key.as_bytes()).with_context(|| {
                    format!("invalid HTTP header key `{key}` on route `{}`", route.name)
                })?;
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
        let (url, body) = match request {
            ExportRequest::Logs(req) => (&self.urls.logs_url, encode_pb(req.as_ref())),
            ExportRequest::Traces(req) => (&self.urls.traces_url, encode_pb(req.as_ref())),
            ExportRequest::Metrics(req) => (&self.urls.metrics_url, encode_pb(req.as_ref())),
        };
        let response = self
            .client
            .post(url.clone())
            .headers(self.headers.clone())
            .body(Bytes::from(body))
            .send()
            .await
            .with_context(|| format!("HTTP POST {url}"))?;
        let status = response.status();
        if !status.is_success() {
            let body_snippet = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(400)
                .collect::<String>();
            bail!(
                "upstream returned HTTP {status} for {url}: {snippet}",
                snippet = body_snippet
            );
        }
        // 2xx かつ body 全体を discard。partial success の解析は今のところ省略 (Phase B で追加)。
        drop(response);
        Ok(())
    }
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

    #[test]
    fn http_client_rejects_invalid_endpoint_at_startup() {
        for endpoint in [
            "",
            "collector.example.com",
            "ftp://collector.example.com",
            "https://collector.example.com?tenant=example",
            "https://collector.example.com/#fragment",
        ] {
            let route = ProxyRoute {
                name: "openai".to_string(),
                service_names: vec!["codex_cli_rs".to_string()],
                signals: ProxySignal::ALL.to_vec(),
                transport: ProxyTransport::HttpProtobuf,
                endpoint: endpoint.to_string(),
                headers: vec![],
            };

            let error = match RouteClient::build(&route, 3_000) {
                Ok(_) => panic!("不正な endpoint `{endpoint}` が受理された"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains("openai"),
                "route 名を含むエラーを返す必要がある: {error:#}"
            );
        }
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
