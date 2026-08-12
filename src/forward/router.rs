//! service.name → route の振り分けと、ResourceLogs 等の分割ロジック。
//!
//! Sink はここの `notify` を呼ぶだけでよい。Router 内部で route ごとの channel に
//! `try_send` し、overflow は drop counter に加算する (drop-newest)。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value as OtlpValue;
use opentelemetry_proto::tonic::resource::v1::Resource;
use tokio::sync::mpsc;

use crate::cli::ProxyRoute;
use crate::config::ProxySignal;
use crate::sink::TelemetryRecord;

use super::{ExportRequest, RouteMetricsHandle};

/// route ごとに 1 個作られる送信レーン。
pub(super) struct RouteLane {
    pub name: String,
    pub route: ProxyRoute,
    pub sender: mpsc::Sender<ExportRequest>,
    pub metrics: RouteMetricsHandle,
}

/// Sink からの notify を受けて service.name で振り分ける公開 API。
#[derive(Clone)]
pub struct ProxyRouter {
    inner: Arc<RouterInner>,
}

struct RouterInner {
    /// route index → RouteLane。
    lanes: Vec<Arc<RouteLane>>,
    /// service.name（受信値をそのまま使用）→ route index。大文字・小文字を区別する。
    /// (OTLP service.name は case-sensitive)。
    matcher: HashMap<String, usize>,
}

impl ProxyRouter {
    pub(super) fn new(lanes: Vec<Arc<RouteLane>>) -> Self {
        let mut matcher = HashMap::new();
        for (i, lane) in lanes.iter().enumerate() {
            for sn in &lane.route.service_names {
                matcher.insert(sn.clone(), i);
            }
        }
        Self {
            inner: Arc::new(RouterInner { lanes, matcher }),
        }
    }

    /// route ごとの累計 metrics snapshot を返す。`/stats` から呼ぶ。
    pub fn snapshot(&self) -> Vec<(String, super::RouteMetricsSnapshot)> {
        self.inner
            .lanes
            .iter()
            .map(|lane| {
                // 手動の Atomic カウンタは、送信成功後の加算より先に worker が受信して
                // 減算すると underflow する。channel 自身の空き容量から、snapshot 時点で
                // 実際に queue に残っている件数を算出する。
                let depth = lane
                    .sender
                    .max_capacity()
                    .saturating_sub(lane.sender.capacity());
                let depth = u64::try_from(depth).unwrap_or(u64::MAX);
                (lane.name.clone(), lane.metrics.snapshot(depth))
            })
            .collect()
    }

    /// JSONL に保存済みの record を各 route に振り分けて `try_send` する。
    ///
    /// - service.name にマッチしない resource は skip
    /// - matched resource が 0 の route は send しない
    /// - channel overflow (bounded queue full) / closed は drop + counter 加算
    pub fn notify(&self, record: &TelemetryRecord) {
        for lane in &self.inner.lanes {
            if !lane.route.accepts_signal(record.signal()) {
                continue;
            }
            let filtered = match record {
                TelemetryRecord::Logs(req) => filter_logs(req, &self.inner.matcher, lane),
                TelemetryRecord::Traces(req) => filter_traces(req, &self.inner.matcher, lane),
                TelemetryRecord::Metrics(req) => filter_metrics(req, &self.inner.matcher, lane),
            };
            let Some(request) = filtered else {
                continue;
            };
            match lane.sender.try_send(request) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    let dropped = lane.metrics.dropped_total.fetch_add(1, Ordering::Relaxed) + 1;
                    // 大量ログの洪水を避けるため 2^n ごとに warn。
                    if dropped.is_power_of_two() {
                        tracing::warn!(
                            route = %lane.name,
                            dropped_total = dropped,
                            "proxy queue full: dropping OTLP batch"
                        );
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    let dropped = lane.metrics.dropped_total.fetch_add(1, Ordering::Relaxed) + 1;
                    // shutdown 済みの route への payload も、実際に転送されないため drop として数える。
                    tracing::debug!(
                        route = %lane.name,
                        dropped_total = dropped,
                        "proxy queue closed; dropping notify"
                    );
                }
            }
        }
    }
}

impl TelemetryRecord {
    /// signal 種別を返す。router の filter 分岐に使う。
    pub fn signal(&self) -> ProxySignal {
        match self {
            TelemetryRecord::Logs(_) => ProxySignal::Logs,
            TelemetryRecord::Traces(_) => ProxySignal::Traces,
            TelemetryRecord::Metrics(_) => ProxySignal::Metrics,
        }
    }
}

fn filter_logs(
    req: &ExportLogsServiceRequest,
    matcher: &HashMap<String, usize>,
    lane: &RouteLane,
) -> Option<ExportRequest> {
    let target_idx = *matcher
        .get(lane.route.service_names.first()?)
        .unwrap_or(&usize::MAX);
    let resource_logs: Vec<_> = req
        .resource_logs
        .iter()
        .filter(|rl| service_name_matches_lane(rl.resource.as_ref(), matcher, target_idx, lane))
        .cloned()
        .collect();
    if resource_logs.is_empty() {
        return None;
    }
    Some(ExportRequest::Logs(Box::new(ExportLogsServiceRequest {
        resource_logs,
    })))
}

fn filter_traces(
    req: &ExportTraceServiceRequest,
    matcher: &HashMap<String, usize>,
    lane: &RouteLane,
) -> Option<ExportRequest> {
    let target_idx = *matcher
        .get(lane.route.service_names.first()?)
        .unwrap_or(&usize::MAX);
    let resource_spans: Vec<_> = req
        .resource_spans
        .iter()
        .filter(|rs| service_name_matches_lane(rs.resource.as_ref(), matcher, target_idx, lane))
        .cloned()
        .collect();
    if resource_spans.is_empty() {
        return None;
    }
    Some(ExportRequest::Traces(Box::new(ExportTraceServiceRequest {
        resource_spans,
    })))
}

fn filter_metrics(
    req: &ExportMetricsServiceRequest,
    matcher: &HashMap<String, usize>,
    lane: &RouteLane,
) -> Option<ExportRequest> {
    let target_idx = *matcher
        .get(lane.route.service_names.first()?)
        .unwrap_or(&usize::MAX);
    let resource_metrics: Vec<_> = req
        .resource_metrics
        .iter()
        .filter(|rm| service_name_matches_lane(rm.resource.as_ref(), matcher, target_idx, lane))
        .cloned()
        .collect();
    if resource_metrics.is_empty() {
        return None;
    }
    Some(ExportRequest::Metrics(Box::new(
        ExportMetricsServiceRequest { resource_metrics },
    )))
}

/// resource の service.name を見て、この lane にマッチするか判定する。
/// - service.name が matcher に載っていて、それが自分の route index なら OK
/// - unknown service.name (どこにもマッチしない) は default route に載せない (settings で決めた
///   route だけに配る。将来的に fallback route の設定を足す余地は残す)
fn service_name_matches_lane(
    resource: Option<&Resource>,
    matcher: &HashMap<String, usize>,
    _first_target_idx: usize,
    lane: &RouteLane,
) -> bool {
    let service = extract_service_name(resource);
    match matcher.get(service) {
        Some(idx) => {
            // route index の照合は「route 名」で行うほうが安全 (別 route と service.name を共有しない前提)。
            // ここでは matcher は unique index なので、name で照合する。
            let lane_idx = lane_index(matcher, &lane.route);
            *idx == lane_idx
        }
        None => false,
    }
}

fn lane_index(matcher: &HashMap<String, usize>, route: &ProxyRoute) -> usize {
    // 自分の route.service_names のうち matcher にある最初の 1 個を取り、その index を返す。
    // 呼び出し側が既に service_names を持っているので O(1)。
    route
        .service_names
        .iter()
        .filter_map(|s| matcher.get(s).copied())
        .next()
        .unwrap_or(usize::MAX)
}

fn extract_service_name(resource: Option<&Resource>) -> &str {
    let Some(r) = resource else { return "" };
    r.attributes
        .iter()
        .find(|kv| kv.key == "service.name")
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| v.value.as_ref())
        .and_then(|v| match v {
            OtlpValue::StringValue(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ProxyRoute;
    use crate::config::ProxyTransport;
    use opentelemetry_proto::tonic::common::v1::any_value::Value as OtlpValue;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue};
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use std::sync::Arc;

    fn kv_str(k: &str, v: &str) -> KeyValue {
        KeyValue {
            key: k.to_string(),
            value: Some(AnyValue {
                value: Some(OtlpValue::StringValue(v.to_string())),
            }),
            key_strindex: 0,
        }
    }

    fn resource_for(service: &str) -> Resource {
        Resource {
            attributes: vec![kv_str("service.name", service)],
            dropped_attributes_count: 0,
            entity_refs: vec![],
        }
    }

    fn resource_logs_for(service: &str) -> ResourceLogs {
        ResourceLogs {
            resource: Some(resource_for(service)),
            scope_logs: vec![ScopeLogs {
                scope: Some(InstrumentationScope::default()),
                log_records: vec![LogRecord::default()],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }
    }

    fn make_lane(
        name: &str,
        service_names: Vec<&str>,
    ) -> (Arc<RouteLane>, mpsc::Receiver<ExportRequest>) {
        let (tx, rx) = mpsc::channel(4);
        (
            Arc::new(RouteLane {
                name: name.to_string(),
                route: ProxyRoute {
                    name: name.to_string(),
                    service_names: service_names.into_iter().map(|s| s.to_string()).collect(),
                    signals: ProxySignal::ALL.to_vec(),
                    transport: ProxyTransport::Grpc,
                    endpoint: "https://example.invalid".to_string(),
                    headers: Vec::new(),
                },
                sender: tx,
                metrics: Arc::new(super::super::RouteMetrics::default()),
            }),
            rx,
        )
    }

    #[test]
    fn splits_resources_by_service_name() {
        let (anthropic, _a_rx) = make_lane("anthropic", vec!["claude-code"]);
        let (openai, _o_rx) = make_lane("openai", vec!["codex_cli_rs", "codex-app-server"]);
        let router = ProxyRouter::new(vec![anthropic.clone(), openai.clone()]);

        let req = ExportLogsServiceRequest {
            resource_logs: vec![
                resource_logs_for("claude-code"),
                resource_logs_for("codex_cli_rs"),
                resource_logs_for("unknown-service"),
            ],
        };
        router.notify(&TelemetryRecord::Logs(Box::new(req)));

        let snapshot = router.snapshot().into_iter().collect::<HashMap<_, _>>();
        assert_eq!(snapshot["anthropic"].queue_depth, 1);
        assert_eq!(snapshot["openai"].queue_depth, 1);
        // unknown-service は振り分け対象外なので dropped は上がらない。
        assert_eq!(anthropic.metrics.dropped_total.load(Ordering::Relaxed), 0);
        assert_eq!(openai.metrics.dropped_total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn skips_resource_without_service_name() {
        let (openai, _rx) = make_lane("openai", vec!["codex_cli_rs"]);
        let router = ProxyRouter::new(vec![openai.clone()]);
        let mut resource_logs = resource_logs_for("codex_cli_rs");
        resource_logs.resource = None;

        router.notify(&TelemetryRecord::Logs(Box::new(ExportLogsServiceRequest {
            resource_logs: vec![resource_logs],
        })));

        assert_eq!(router.snapshot()[0].1.queue_depth, 0);
        assert_eq!(openai.metrics.dropped_total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn respects_signal_types() {
        let (mut anthropic, _rx) = make_lane("anthropic", vec!["claude-code"]);
        // logs だけを転送する route にする。
        Arc::get_mut(&mut anthropic).unwrap().route.signals = vec![ProxySignal::Logs];
        let router = ProxyRouter::new(vec![anthropic.clone()]);

        let req = ExportMetricsServiceRequest {
            resource_metrics: vec![opentelemetry_proto::tonic::metrics::v1::ResourceMetrics {
                resource: Some(Resource {
                    attributes: vec![kv_str("service.name", "claude-code")],
                    dropped_attributes_count: 0,
                    entity_refs: vec![],
                }),
                scope_metrics: vec![],
                schema_url: String::new(),
            }],
        };
        router.notify(&TelemetryRecord::Metrics(Box::new(req)));

        assert_eq!(router.snapshot()[0].1.queue_depth, 0);
    }

    #[test]
    fn routes_traces_and_metrics_by_service_name() {
        let (openai, mut rx) = make_lane("openai", vec!["codex_cli_rs"]);
        let router = ProxyRouter::new(vec![openai]);

        router.notify(&TelemetryRecord::Traces(Box::new(
            ExportTraceServiceRequest {
                resource_spans: vec![
                    opentelemetry_proto::tonic::trace::v1::ResourceSpans {
                        resource: Some(resource_for("codex_cli_rs")),
                        ..Default::default()
                    },
                    opentelemetry_proto::tonic::trace::v1::ResourceSpans {
                        resource: Some(resource_for("unknown-service")),
                        ..Default::default()
                    },
                ],
            },
        )));
        match rx.try_recv().expect("trace が route に配送される") {
            ExportRequest::Traces(request) => {
                assert_eq!(request.resource_spans.len(), 1);
                assert_eq!(
                    extract_service_name(request.resource_spans[0].resource.as_ref()),
                    "codex_cli_rs"
                );
            }
            _ => panic!("trace 以外の signal が配送された"),
        }

        router.notify(&TelemetryRecord::Metrics(Box::new(
            ExportMetricsServiceRequest {
                resource_metrics: vec![
                    opentelemetry_proto::tonic::metrics::v1::ResourceMetrics {
                        resource: Some(resource_for("codex_cli_rs")),
                        ..Default::default()
                    },
                    opentelemetry_proto::tonic::metrics::v1::ResourceMetrics {
                        resource: Some(resource_for("unknown-service")),
                        ..Default::default()
                    },
                ],
            },
        )));
        match rx.try_recv().expect("metrics が route に配送される") {
            ExportRequest::Metrics(request) => {
                assert_eq!(request.resource_metrics.len(), 1);
                assert_eq!(
                    extract_service_name(request.resource_metrics[0].resource.as_ref()),
                    "codex_cli_rs"
                );
            }
            _ => panic!("metrics 以外の signal が配送された"),
        }
    }

    #[test]
    fn drops_when_channel_is_full() {
        let (tx, _rx) = mpsc::channel::<ExportRequest>(1);
        // 意図的に _rx を保持 (drop で channel が closed にならないよう)。
        let lane = Arc::new(RouteLane {
            name: "anthropic".to_string(),
            route: ProxyRoute {
                name: "anthropic".to_string(),
                service_names: vec!["claude-code".to_string()],
                signals: ProxySignal::ALL.to_vec(),
                transport: ProxyTransport::Grpc,
                endpoint: "https://example.invalid".to_string(),
                headers: Vec::new(),
            },
            sender: tx,
            metrics: Arc::new(super::super::RouteMetrics::default()),
        });
        let router = ProxyRouter::new(vec![lane.clone()]);

        let make_req = || {
            TelemetryRecord::Logs(Box::new(ExportLogsServiceRequest {
                resource_logs: vec![resource_logs_for("claude-code")],
            }))
        };

        router.notify(&make_req()); // queue=1
        router.notify(&make_req()); // dropped=1
        router.notify(&make_req()); // dropped=2

        assert_eq!(router.snapshot()[0].1.queue_depth, 1);
        assert_eq!(lane.metrics.dropped_total.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn queue_depth_uses_channel_state_after_receive() {
        let (lane, mut rx) = make_lane("anthropic", vec!["claude-code"]);
        let router = ProxyRouter::new(vec![lane]);
        let request = TelemetryRecord::Logs(Box::new(ExportLogsServiceRequest {
            resource_logs: vec![resource_logs_for("claude-code")],
        }));

        router.notify(&request);
        assert_eq!(router.snapshot()[0].1.queue_depth, 1);

        rx.try_recv().expect("queue から request を受信できること");
        assert_eq!(router.snapshot()[0].1.queue_depth, 0);
    }

    #[test]
    fn drops_when_channel_is_closed() {
        let (lane, rx) = make_lane("anthropic", vec!["claude-code"]);
        let router = ProxyRouter::new(vec![lane]);
        drop(rx);

        let request = TelemetryRecord::Logs(Box::new(ExportLogsServiceRequest {
            resource_logs: vec![resource_logs_for("claude-code")],
        }));
        router.notify(&request);

        let snapshot = router.snapshot();
        assert_eq!(snapshot[0].1.queue_depth, 0);
        assert_eq!(snapshot[0].1.dropped, 1);
    }
}
