use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value as OtlpValue;
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NumberValue;
use opentelemetry_proto::tonic::metrics::v1::{
    AggregationTemporality, HistogramDataPoint, Metric, NumberDataPoint, metric::Data as MetricData,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const AGENT_CLAUDE: &str = "claude-code";
const AGENT_CODEX: &str = "codex";
const SERVICE_CLAUDE: &str = "claude-code";
const SERVICE_CODEX_TUI: &str = "codex_cli_rs";
const SERVICE_CODEX_EXEC: &str = "codex_exec";
const PROVIDER_ANTHROPIC: &str = "anthropic";
const PROVIDER_OPENAI: &str = "OpenAI";
const UNKNOWN: &str = "unknown";

/// Per-bucket running totals. Fields outside the agent's vocabulary stay 0
/// (e.g. `cost_usd` for Codex, `reasoning_output_tokens` for Claude).
#[derive(Debug, Default, Clone, Serialize)]
pub struct ModelStats {
    pub request_count: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub cost_usd: f64,
    pub duration_ms: u64,
}

impl ModelStats {
    fn add(&mut self, sample: &ModelStats) {
        self.request_count += sample.request_count;
        self.input_tokens += sample.input_tokens;
        self.output_tokens += sample.output_tokens;
        self.cache_read_tokens += sample.cache_read_tokens;
        self.cache_creation_tokens += sample.cache_creation_tokens;
        self.reasoning_output_tokens += sample.reasoning_output_tokens;
        self.cost_usd += sample.cost_usd;
        self.duration_ms += sample.duration_ms;
    }
}

/// Internal provider × model × effort triple used as a `BTreeMap` key for
/// breakdowns. Not exposed: `BucketStats` (the value type) carries the same
/// fields for serialization.
#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Bucket {
    provider: String,
    model: String,
    effort: String,
}

impl Bucket {
    fn from_parts(
        provider: impl Into<String>,
        model: impl Into<String>,
        effort: impl Into<String>,
    ) -> Self {
        let provider = non_empty(provider.into());
        let model = non_empty(model.into());
        let effort = non_empty(effort.into());
        Self {
            provider,
            model,
            effort,
        }
    }

    /// Stable string key for serialization output: `provider/model/effort`.
    fn key(&self) -> String {
        format!("{}/{}/{}", self.provider, self.model, self.effort)
    }
}

fn non_empty(s: String) -> String {
    if s.is_empty() { UNKNOWN.to_string() } else { s }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct AgentStats {
    pub total: ModelStats,
    /// Per-(provider, model, effort) bucket. Map key is `Bucket::key()`.
    pub buckets: BTreeMap<String, BucketStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BucketStats {
    pub provider: String,
    pub model: String,
    pub effort: String,
    #[serde(flatten)]
    pub stats: ModelStats,
}

impl BucketStats {
    fn from_bucket(bucket: &Bucket) -> Self {
        Self {
            provider: bucket.provider.clone(),
            model: bucket.model.clone(),
            effort: bucket.effort.clone(),
            stats: ModelStats::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageSnapshot {
    pub started_at: String,
    pub last_updated: Option<String>,
    pub agents: BTreeMap<String, AgentStats>,
}

#[derive(Debug)]
pub struct Aggregator {
    inner: RwLock<AggregatorInner>,
}

#[derive(Debug)]
struct AggregatorInner {
    started_at: OffsetDateTime,
    last_updated: Option<OffsetDateTime>,
    agents: BTreeMap<String, AgentStats>,

    /// Claude model name canonicalization. Anthropic logs strip variant
    /// suffixes from `model` (e.g. `claude-opus-4-7`), but the matching
    /// metrics + spans carry the full name (`claude-opus-4-7[1m]`). We track
    /// every full name we have seen on a metric and use it to upgrade later
    /// log-side bucket keys so 1M and standard variants don't fragment.
    claude_canonical_models: HashMap<String, String>,

    /// Latest Codex session metadata observed via `codex.conversation_starts`
    /// log/event. Codex metrics do not carry `reasoning_effort`, so we
    /// fall back to the most recent session-level value.
    codex_last_session: Option<CodexSession>,
}

#[derive(Debug, Clone)]
struct CodexSession {
    provider: String,
    #[allow(dead_code)] // kept for future correlation
    model: String,
    effort: String,
}

impl Aggregator {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(AggregatorInner {
                started_at: OffsetDateTime::now_utc(),
                last_updated: None,
                agents: BTreeMap::new(),
                claude_canonical_models: HashMap::new(),
                codex_last_session: None,
            }),
        }
    }

    /// Ingest log events. Token/cost are no longer derived from logs (metrics
    /// own those); logs only contribute `request_count` and `duration_ms` for
    /// Claude, and Codex session metadata used to fill in `effort`.
    pub fn ingest_logs(&self, req: &ExportLogsServiceRequest) -> usize {
        let mut count = 0;
        let mut g = self.inner.write().expect("aggregator lock poisoned");
        for resource_logs in &req.resource_logs {
            let service = service_name(resource_logs.resource.as_ref());
            for scope_logs in &resource_logs.scope_logs {
                for log in &scope_logs.log_records {
                    if service == SERVICE_CLAUDE
                        && let Some((bucket, stats)) = extract_claude_api_request_meta(&mut g, log)
                    {
                        record_into(&mut g, AGENT_CLAUDE, &bucket, &stats);
                        count += 1;
                        continue;
                    }
                    if (service == SERVICE_CODEX_TUI || service == SERVICE_CODEX_EXEC)
                        && let Some(session) = extract_codex_conversation_starts(log)
                    {
                        g.codex_last_session = Some(session);
                    }
                }
            }
        }
        count
    }

    /// Ingest trace spans. Codex session info still arrives as span events
    /// (`codex.conversation_starts`) on `session_init`, so we read those to
    /// capture `reasoning_effort`. All token/cost accounting now lives in
    /// metrics ingestion.
    pub fn ingest_traces(&self, req: &ExportTraceServiceRequest) -> usize {
        let mut g = self.inner.write().expect("aggregator lock poisoned");
        let mut session_updates = 0;
        for resource_spans in &req.resource_spans {
            let service = service_name(resource_spans.resource.as_ref());
            if service != SERVICE_CODEX_TUI && service != SERVICE_CODEX_EXEC {
                continue;
            }
            for scope_spans in &resource_spans.scope_spans {
                for span in &scope_spans.spans {
                    for ev in &span.events {
                        if ev.name == "codex.conversation_starts"
                            && let Some(session) = extract_session_from_attrs(&ev.attributes)
                        {
                            g.codex_last_session = Some(session);
                            session_updates += 1;
                        }
                    }
                    // Fallback: when `codex.conversation_starts` is dropped
                    // (e.g. codex CLI's OTLP exporter fails to flush before
                    // the second invocation in the same job), the per-request
                    // `handle_responses` span still carries the effort.
                    if span.name == "handle_responses"
                        && update_codex_effort_from_request_attrs(&mut g, &span.attributes)
                    {
                        session_updates += 1;
                    }
                }
            }
        }
        // Spans are no longer sample-bearing for usage stats; only return >0
        // when we learned something so the caller can still emit a summary.
        session_updates
    }

    /// Ingest metric data. This is now the source of truth for tokens/cost
    /// (Claude) and tokens/duration/request count (Codex). Only DELTA
    /// temporality is honored; CUMULATIVE points are dropped with a warning
    /// because correctly carrying state across restarts would require
    /// persistence we don't have today.
    pub fn ingest_metrics(&self, req: &ExportMetricsServiceRequest) -> usize {
        let mut count = 0;
        let mut g = self.inner.write().expect("aggregator lock poisoned");
        for resource_metrics in &req.resource_metrics {
            let service = service_name(resource_metrics.resource.as_ref()).to_string();
            for scope_metrics in &resource_metrics.scope_metrics {
                for metric in &scope_metrics.metrics {
                    count += match (service.as_str(), metric.name.as_str()) {
                        (SERVICE_CLAUDE, "claude_code.token.usage") => {
                            ingest_claude_token(&mut g, metric)
                        }
                        (SERVICE_CLAUDE, "claude_code.cost.usage") => {
                            ingest_claude_cost(&mut g, metric)
                        }
                        (SERVICE_CODEX_TUI | SERVICE_CODEX_EXEC, "codex.turn.token_usage") => {
                            ingest_codex_token(&mut g, metric)
                        }
                        (
                            SERVICE_CODEX_TUI | SERVICE_CODEX_EXEC,
                            "codex.conversation.turn.count",
                        ) => ingest_codex_turn_count(&mut g, metric),
                        (SERVICE_CODEX_TUI | SERVICE_CODEX_EXEC, "codex.turn.e2e_duration_ms") => {
                            ingest_codex_duration(&mut g, metric)
                        }
                        _ => 0,
                    };
                }
            }
        }
        count
    }

    pub fn snapshot(&self) -> UsageSnapshot {
        let g = self.inner.read().expect("aggregator lock poisoned");
        UsageSnapshot {
            started_at: format_rfc3339(g.started_at),
            last_updated: g.last_updated.map(format_rfc3339),
            agents: g.agents.clone(),
        }
    }
}

impl Default for Aggregator {
    fn default() -> Self {
        Self::new()
    }
}

fn record_into(g: &mut AggregatorInner, agent: &str, bucket: &Bucket, stats: &ModelStats) {
    g.last_updated = Some(OffsetDateTime::now_utc());
    let agent_stats = g.agents.entry(agent.to_string()).or_default();
    agent_stats.total.add(stats);
    let entry = agent_stats
        .buckets
        .entry(bucket.key())
        .or_insert_with(|| BucketStats::from_bucket(bucket));
    entry.stats.add(stats);
}

/// `claude_code.api_request` carries `request_count` (1) and `duration_ms`,
/// but its `model` attribute drops variant suffixes like `[1m]`. We map the
/// bare name back to a previously-seen full name so it merges into the same
/// bucket as the metrics.
fn extract_claude_api_request_meta(
    g: &mut AggregatorInner,
    log: &LogRecord,
) -> Option<(Bucket, ModelStats)> {
    let body = log.body.as_ref()?.value.as_ref()?;
    let body_str = match body {
        OtlpValue::StringValue(s) => s.as_str(),
        _ => return None,
    };
    if body_str != "claude_code.api_request" {
        return None;
    }
    let attrs = &log.attributes;
    let raw_model = string_attr(attrs, "model").unwrap_or_default();
    let model = canonical_claude_model(g, raw_model);
    let effort = string_attr(attrs, "effort")
        .map(str::to_string)
        .unwrap_or_default();
    let stats = ModelStats {
        request_count: 1,
        duration_ms: int_attr(attrs, "duration_ms").unwrap_or(0).max(0) as u64,
        ..Default::default()
    };
    Some((Bucket::from_parts(PROVIDER_ANTHROPIC, model, effort), stats))
}

fn extract_codex_conversation_starts(log: &LogRecord) -> Option<CodexSession> {
    // Codex emits this either as a top-level log event or as a span event;
    // here we only handle the log-record form. We accept either body or
    // event.name carrying the marker so we're robust to both shapes.
    let body_matches = log
        .body
        .as_ref()
        .and_then(|b| b.value.as_ref())
        .map(|v| matches!(v, OtlpValue::StringValue(s) if s == "codex.conversation_starts"))
        .unwrap_or(false);
    let event_name_matches = log
        .attributes
        .iter()
        .find(|kv| kv.key == "event.name")
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| v.value.as_ref())
        .map(|v| matches!(v, OtlpValue::StringValue(s) if s == "codex.conversation_starts"))
        .unwrap_or(false);
    if !body_matches && !event_name_matches {
        return None;
    }
    extract_session_from_attrs(&log.attributes)
}

fn extract_session_from_attrs(attrs: &[KeyValue]) -> Option<CodexSession> {
    let provider = string_attr(attrs, "provider_name")
        .map(str::to_string)
        .unwrap_or_else(|| PROVIDER_OPENAI.to_string());
    let model = string_attr(attrs, "model")
        .map(str::to_string)
        .unwrap_or_default();
    let effort = string_attr(attrs, "reasoning_effort")
        .map(str::to_string)
        .unwrap_or_default();
    Some(CodexSession {
        provider,
        model,
        effort,
    })
}

fn ingest_claude_token(g: &mut AggregatorInner, metric: &Metric) -> usize {
    let Some(MetricData::Sum(sum)) = metric.data.as_ref() else {
        return 0;
    };
    if !is_delta_temporality(sum.aggregation_temporality, &metric.name) {
        return 0;
    }
    let mut hits = 0;
    for dp in &sum.data_points {
        let model = string_attr(&dp.attributes, "model").unwrap_or_default();
        if !model.is_empty() {
            register_canonical_claude_model(g, model);
        }
        let effort = string_attr(&dp.attributes, "effort")
            .map(str::to_string)
            .unwrap_or_default();
        let typ = string_attr(&dp.attributes, "type").unwrap_or("");
        let value = number_value_as_u64(dp);
        let mut stats = ModelStats::default();
        match typ {
            "input" => stats.input_tokens = value,
            "output" => stats.output_tokens = value,
            "cacheRead" => stats.cache_read_tokens = value,
            "cacheCreation" => stats.cache_creation_tokens = value,
            _ => continue,
        }
        let bucket = Bucket::from_parts(PROVIDER_ANTHROPIC, model, effort);
        record_into(g, AGENT_CLAUDE, &bucket, &stats);
        hits += 1;
    }
    hits
}

fn ingest_claude_cost(g: &mut AggregatorInner, metric: &Metric) -> usize {
    let Some(MetricData::Sum(sum)) = metric.data.as_ref() else {
        return 0;
    };
    if !is_delta_temporality(sum.aggregation_temporality, &metric.name) {
        return 0;
    }
    let mut hits = 0;
    for dp in &sum.data_points {
        let model = string_attr(&dp.attributes, "model").unwrap_or_default();
        if !model.is_empty() {
            register_canonical_claude_model(g, model);
        }
        let effort = string_attr(&dp.attributes, "effort")
            .map(str::to_string)
            .unwrap_or_default();
        let stats = ModelStats {
            cost_usd: number_value_as_f64(dp),
            ..Default::default()
        };
        let bucket = Bucket::from_parts(PROVIDER_ANTHROPIC, model, effort);
        record_into(g, AGENT_CLAUDE, &bucket, &stats);
        hits += 1;
    }
    hits
}

fn ingest_codex_token(g: &mut AggregatorInner, metric: &Metric) -> usize {
    let Some(MetricData::Histogram(hist)) = metric.data.as_ref() else {
        return 0;
    };
    if !is_delta_temporality(hist.aggregation_temporality, &metric.name) {
        return 0;
    }
    let mut hits = 0;
    for dp in &hist.data_points {
        let model = string_attr(&dp.attributes, "model")
            .map(str::to_string)
            .unwrap_or_default();
        let token_type = string_attr(&dp.attributes, "token_type").unwrap_or("");
        let provider = codex_provider(g);
        let effort = codex_effort(g);
        let value = histogram_sum_as_u64(dp);
        let mut stats = ModelStats::default();
        match token_type {
            "input" => stats.input_tokens = value,
            "output" => stats.output_tokens = value,
            "cached_input" => stats.cache_read_tokens = value,
            "reasoning_output" => stats.reasoning_output_tokens = value,
            // `total` double-counts the others; ignore.
            _ => continue,
        }
        let bucket = Bucket::from_parts(provider, model, effort);
        record_into(g, AGENT_CODEX, &bucket, &stats);
        hits += 1;
    }
    hits
}

fn ingest_codex_turn_count(g: &mut AggregatorInner, metric: &Metric) -> usize {
    let Some(MetricData::Sum(sum)) = metric.data.as_ref() else {
        return 0;
    };
    if !is_delta_temporality(sum.aggregation_temporality, &metric.name) {
        return 0;
    }
    let mut hits = 0;
    for dp in &sum.data_points {
        let model = string_attr(&dp.attributes, "model")
            .map(str::to_string)
            .unwrap_or_default();
        let provider = codex_provider(g);
        let effort = codex_effort(g);
        let stats = ModelStats {
            request_count: number_value_as_u64(dp),
            ..Default::default()
        };
        let bucket = Bucket::from_parts(provider, model, effort);
        record_into(g, AGENT_CODEX, &bucket, &stats);
        hits += 1;
    }
    hits
}

fn ingest_codex_duration(g: &mut AggregatorInner, metric: &Metric) -> usize {
    let Some(MetricData::Histogram(hist)) = metric.data.as_ref() else {
        return 0;
    };
    if !is_delta_temporality(hist.aggregation_temporality, &metric.name) {
        return 0;
    }
    let mut hits = 0;
    for dp in &hist.data_points {
        let model = string_attr(&dp.attributes, "model")
            .map(str::to_string)
            .unwrap_or_default();
        let provider = codex_provider(g);
        let effort = codex_effort(g);
        let stats = ModelStats {
            duration_ms: histogram_sum_as_u64(dp),
            ..Default::default()
        };
        let bucket = Bucket::from_parts(provider, model, effort);
        record_into(g, AGENT_CODEX, &bucket, &stats);
        hits += 1;
    }
    hits
}

fn codex_provider(g: &AggregatorInner) -> String {
    g.codex_last_session
        .as_ref()
        .map(|s| s.provider.clone())
        .unwrap_or_else(|| PROVIDER_OPENAI.to_string())
}

fn codex_effort(g: &AggregatorInner) -> String {
    g.codex_last_session
        .as_ref()
        .map(|s| s.effort.clone())
        .unwrap_or_default()
}

/// Fallback effort source when `codex.conversation_starts` is missing.
/// `codex.request.*` is internal Codex CLI telemetry rather than an OTel
/// semantic convention, so we scope this to `handle_responses` spans only and
/// only touch `effort` (provider/model are still owned by `conversation_starts`
/// or the metric data point itself).
fn update_codex_effort_from_request_attrs(g: &mut AggregatorInner, attrs: &[KeyValue]) -> bool {
    let Some(effort) = string_attr(attrs, "codex.request.reasoning_effort") else {
        return false;
    };
    if effort.is_empty() {
        return false;
    }
    match g.codex_last_session.as_mut() {
        Some(session) => session.effort = effort.to_string(),
        None => {
            g.codex_last_session = Some(CodexSession {
                provider: PROVIDER_OPENAI.to_string(),
                model: String::new(),
                effort: effort.to_string(),
            });
        }
    }
    true
}

/// Anthropic logs report `claude-opus-4-7`, while metrics/spans report
/// `claude-opus-4-7[1m]`. Treat the first segment before `[` as the bare
/// name and remember the full name so subsequent log records merge into the
/// same bucket as the metrics.
fn register_canonical_claude_model(g: &mut AggregatorInner, full: &str) {
    let bare = match full.find('[') {
        Some(i) => &full[..i],
        None => full,
    };
    if bare.is_empty() || bare == full {
        // No suffix to canonicalize; nothing to remember.
        if !full.is_empty() {
            g.claude_canonical_models
                .entry(full.to_string())
                .or_insert_with(|| full.to_string());
        }
        return;
    }
    let bare = bare.to_string();
    let already = g.claude_canonical_models.get(&bare).cloned();
    if already.as_deref() == Some(full) {
        return;
    }
    g.claude_canonical_models
        .insert(bare.clone(), full.to_string());
    // If a bucket was already created under the bare name, fold its stats
    // into the full-name bucket so the snapshot doesn't show duplicates.
    if let Some(agent_stats) = g.agents.get_mut(AGENT_CLAUDE) {
        merge_bare_into_full(agent_stats, &bare, full);
    }
}

fn merge_bare_into_full(agent_stats: &mut AgentStats, bare: &str, full: &str) {
    let bare_keys: Vec<String> = agent_stats
        .buckets
        .keys()
        .filter(|k| {
            let parts: Vec<&str> = k.splitn(3, '/').collect();
            parts.len() == 3 && parts[1] == bare
        })
        .cloned()
        .collect();
    for bare_key in bare_keys {
        let Some(bare_stats) = agent_stats.buckets.remove(&bare_key) else {
            continue;
        };
        let full_key = bare_key.replacen(&format!("/{bare}/"), &format!("/{full}/"), 1);
        let entry = agent_stats
            .buckets
            .entry(full_key)
            .or_insert_with(|| BucketStats {
                provider: bare_stats.provider.clone(),
                model: full.to_string(),
                effort: bare_stats.effort.clone(),
                stats: ModelStats::default(),
            });
        entry.stats.add(&bare_stats.stats);
    }
}

fn canonical_claude_model(g: &AggregatorInner, raw: &str) -> String {
    g.claude_canonical_models
        .get(raw)
        .cloned()
        .unwrap_or_else(|| raw.to_string())
}

fn is_delta_temporality(temporality: i32, metric_name: &str) -> bool {
    let Ok(t) = AggregationTemporality::try_from(temporality) else {
        tracing::debug!(metric = metric_name, temporality, "unknown temporality");
        return false;
    };
    match t {
        AggregationTemporality::Delta => true,
        AggregationTemporality::Cumulative => {
            tracing::warn!(
                metric = metric_name,
                "cumulative aggregation temporality is not supported; dropping data point"
            );
            false
        }
        AggregationTemporality::Unspecified => false,
    }
}

fn number_value_as_u64(dp: &NumberDataPoint) -> u64 {
    match dp.value {
        Some(NumberValue::AsInt(i)) => i.max(0) as u64,
        Some(NumberValue::AsDouble(d)) if d.is_finite() && d >= 0.0 => d as u64,
        _ => 0,
    }
}

fn number_value_as_f64(dp: &NumberDataPoint) -> f64 {
    match dp.value {
        Some(NumberValue::AsDouble(d)) if d.is_finite() => d.max(0.0),
        Some(NumberValue::AsInt(i)) => i.max(0) as f64,
        _ => 0.0,
    }
}

fn histogram_sum_as_u64(dp: &HistogramDataPoint) -> u64 {
    match dp.sum {
        Some(s) if s.is_finite() && s >= 0.0 => s as u64,
        _ => 0,
    }
}

fn service_name(resource: Option<&Resource>) -> &str {
    let Some(r) = resource else {
        return "";
    };
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

fn string_attr<'a>(attrs: &'a [KeyValue], key: &str) -> Option<&'a str> {
    let v = attrs
        .iter()
        .find(|kv| kv.key == key)?
        .value
        .as_ref()?
        .value
        .as_ref()?;
    match v {
        OtlpValue::StringValue(s) => Some(s.as_str()),
        _ => None,
    }
}

fn int_attr(attrs: &[KeyValue], key: &str) -> Option<i64> {
    let v = attrs
        .iter()
        .find(|kv| kv.key == key)?
        .value
        .as_ref()?
        .value
        .as_ref()?;
    match v {
        OtlpValue::IntValue(i) => Some(*i),
        OtlpValue::DoubleValue(d) => Some(*d as i64),
        OtlpValue::StringValue(s) => s.parse().ok(),
        _ => None,
    }
}

fn format_rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339)
        .unwrap_or_else(|_| t.unix_timestamp().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
    use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value as NumberValue;
    use opentelemetry_proto::tonic::metrics::v1::{
        Histogram, HistogramDataPoint, Metric, NumberDataPoint, Sum, metric::Data as MetricData,
    };

    fn kv(key: &str, value: AnyValue) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(value),
        }
    }
    fn kv_str(key: &str, value: &str) -> KeyValue {
        kv(
            key,
            AnyValue {
                value: Some(OtlpValue::StringValue(value.to_string())),
            },
        )
    }
    fn make_int_dp(attrs: Vec<KeyValue>, value: i64) -> NumberDataPoint {
        NumberDataPoint {
            attributes: attrs,
            start_time_unix_nano: 0,
            time_unix_nano: 0,
            exemplars: vec![],
            flags: 0,
            value: Some(NumberValue::AsInt(value)),
        }
    }
    fn make_double_dp(attrs: Vec<KeyValue>, value: f64) -> NumberDataPoint {
        NumberDataPoint {
            attributes: attrs,
            start_time_unix_nano: 0,
            time_unix_nano: 0,
            exemplars: vec![],
            flags: 0,
            value: Some(NumberValue::AsDouble(value)),
        }
    }
    fn make_hist_dp(attrs: Vec<KeyValue>, sum: f64) -> HistogramDataPoint {
        HistogramDataPoint {
            attributes: attrs,
            start_time_unix_nano: 0,
            time_unix_nano: 0,
            count: 1,
            sum: Some(sum),
            bucket_counts: vec![],
            explicit_bounds: vec![],
            exemplars: vec![],
            flags: 0,
            min: None,
            max: None,
        }
    }
    fn delta_sum(points: Vec<NumberDataPoint>) -> MetricData {
        MetricData::Sum(Sum {
            data_points: points,
            aggregation_temporality: AggregationTemporality::Delta as i32,
            is_monotonic: true,
        })
    }
    fn delta_hist(points: Vec<HistogramDataPoint>) -> MetricData {
        MetricData::Histogram(Histogram {
            data_points: points,
            aggregation_temporality: AggregationTemporality::Delta as i32,
        })
    }

    fn claude_token_metric(model: &str, effort: &str, typ: &str, value: i64) -> Metric {
        Metric {
            name: "claude_code.token.usage".into(),
            description: String::new(),
            unit: "tokens".into(),
            metadata: vec![],
            data: Some(delta_sum(vec![make_int_dp(
                vec![
                    kv_str("model", model),
                    kv_str("effort", effort),
                    kv_str("type", typ),
                ],
                value,
            )])),
        }
    }

    fn claude_cost_metric(model: &str, effort: &str, value: f64) -> Metric {
        Metric {
            name: "claude_code.cost.usage".into(),
            description: String::new(),
            unit: "USD".into(),
            metadata: vec![],
            data: Some(delta_sum(vec![make_double_dp(
                vec![kv_str("model", model), kv_str("effort", effort)],
                value,
            )])),
        }
    }

    fn codex_token_metric(model: &str, token_type: &str, sum: f64) -> Metric {
        Metric {
            name: "codex.turn.token_usage".into(),
            description: String::new(),
            unit: String::new(),
            metadata: vec![],
            data: Some(delta_hist(vec![make_hist_dp(
                vec![kv_str("model", model), kv_str("token_type", token_type)],
                sum,
            )])),
        }
    }

    fn codex_turn_count(model: &str, value: i64) -> Metric {
        Metric {
            name: "codex.conversation.turn.count".into(),
            description: String::new(),
            unit: String::new(),
            metadata: vec![],
            data: Some(delta_sum(vec![make_int_dp(
                vec![kv_str("model", model)],
                value,
            )])),
        }
    }

    fn codex_duration_metric(model: &str, sum: f64) -> Metric {
        Metric {
            name: "codex.turn.e2e_duration_ms".into(),
            description: String::new(),
            unit: String::new(),
            metadata: vec![],
            data: Some(delta_hist(vec![make_hist_dp(
                vec![kv_str("model", model)],
                sum,
            )])),
        }
    }

    fn make_metric_req(service: &str, metrics: Vec<Metric>) -> ExportMetricsServiceRequest {
        use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
        use opentelemetry_proto::tonic::metrics::v1::{ResourceMetrics, ScopeMetrics};
        use opentelemetry_proto::tonic::resource::v1::Resource;

        ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: Some(Resource {
                    attributes: vec![kv_str("service.name", service)],
                    dropped_attributes_count: 0,
                    entity_refs: vec![],
                }),
                scope_metrics: vec![ScopeMetrics {
                    scope: Some(InstrumentationScope::default()),
                    metrics,
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        }
    }

    fn make_trace_req(
        service: &str,
        spans: Vec<opentelemetry_proto::tonic::trace::v1::Span>,
    ) -> ExportTraceServiceRequest {
        use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
        use opentelemetry_proto::tonic::resource::v1::Resource;
        use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans};

        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(Resource {
                    attributes: vec![kv_str("service.name", service)],
                    dropped_attributes_count: 0,
                    entity_refs: vec![],
                }),
                scope_spans: vec![ScopeSpans {
                    scope: Some(InstrumentationScope::default()),
                    spans,
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        }
    }

    fn handle_responses_span(effort: &str) -> opentelemetry_proto::tonic::trace::v1::Span {
        opentelemetry_proto::tonic::trace::v1::Span {
            trace_id: vec![1; 16],
            span_id: vec![2; 8],
            trace_state: String::new(),
            parent_span_id: vec![],
            flags: 0,
            name: "handle_responses".into(),
            kind: 0,
            start_time_unix_nano: 1,
            end_time_unix_nano: 2,
            attributes: vec![kv_str("codex.request.reasoning_effort", effort)],
            dropped_attributes_count: 0,
            events: vec![],
            dropped_events_count: 0,
            links: vec![],
            dropped_links_count: 0,
            status: None,
        }
    }

    fn make_log_req(service: &str, body: &str, attrs: Vec<KeyValue>) -> ExportLogsServiceRequest {
        use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
        use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
        use opentelemetry_proto::tonic::resource::v1::Resource;

        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![kv_str("service.name", service)],
                    dropped_attributes_count: 0,
                    entity_refs: vec![],
                }),
                scope_logs: vec![ScopeLogs {
                    scope: Some(InstrumentationScope::default()),
                    log_records: vec![LogRecord {
                        time_unix_nano: 0,
                        observed_time_unix_nano: 0,
                        severity_number: 0,
                        severity_text: String::new(),
                        body: Some(AnyValue {
                            value: Some(OtlpValue::StringValue(body.to_string())),
                        }),
                        attributes: attrs,
                        dropped_attributes_count: 0,
                        flags: 0,
                        trace_id: vec![],
                        span_id: vec![],
                        event_name: String::new(),
                    }],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        }
    }

    #[test]
    fn claude_metrics_aggregate_per_bucket() {
        let agg = Aggregator::new();
        let req = make_metric_req(
            SERVICE_CLAUDE,
            vec![
                claude_token_metric("claude-opus-4-7[1m]", "max", "input", 100),
                claude_token_metric("claude-opus-4-7[1m]", "max", "output", 50),
                claude_token_metric("claude-opus-4-7[1m]", "max", "cacheRead", 1000),
                claude_token_metric("claude-opus-4-7[1m]", "max", "cacheCreation", 200),
                claude_cost_metric("claude-opus-4-7[1m]", "max", 1.25),
            ],
        );
        agg.ingest_metrics(&req);
        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CLAUDE).unwrap();
        let bucket = agent
            .buckets
            .get("anthropic/claude-opus-4-7[1m]/max")
            .unwrap();
        assert_eq!(bucket.stats.input_tokens, 100);
        assert_eq!(bucket.stats.output_tokens, 50);
        assert_eq!(bucket.stats.cache_read_tokens, 1000);
        assert_eq!(bucket.stats.cache_creation_tokens, 200);
        assert!((bucket.stats.cost_usd - 1.25).abs() < 1e-9);
    }

    #[test]
    fn claude_log_request_count_merges_into_metric_bucket() {
        let agg = Aggregator::new();
        // Metrics arrive first and establish the canonical full-name bucket.
        let metric_req = make_metric_req(
            SERVICE_CLAUDE,
            vec![
                claude_token_metric("claude-opus-4-7[1m]", "max", "input", 10),
                claude_cost_metric("claude-opus-4-7[1m]", "max", 0.5),
            ],
        );
        agg.ingest_metrics(&metric_req);
        // Log arrives with the bare model name; should merge into [1m] bucket.
        let log_req = make_log_req(
            SERVICE_CLAUDE,
            "claude_code.api_request",
            vec![
                kv_str("model", "claude-opus-4-7"),
                kv_str("effort", "max"),
                kv(
                    "duration_ms",
                    AnyValue {
                        value: Some(OtlpValue::IntValue(2500)),
                    },
                ),
            ],
        );
        agg.ingest_logs(&log_req);
        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CLAUDE).unwrap();
        assert!(
            !agent.buckets.contains_key("anthropic/claude-opus-4-7/max"),
            "bare bucket should not exist after canonicalization"
        );
        let bucket = agent
            .buckets
            .get("anthropic/claude-opus-4-7[1m]/max")
            .unwrap();
        assert_eq!(bucket.stats.request_count, 1);
        assert_eq!(bucket.stats.duration_ms, 2500);
        assert_eq!(bucket.stats.input_tokens, 10);
        assert!((bucket.stats.cost_usd - 0.5).abs() < 1e-9);
    }

    #[test]
    fn claude_log_then_metric_folds_bare_bucket_into_full() {
        let agg = Aggregator::new();
        // Log arrives first under the bare name.
        let log_req = make_log_req(
            SERVICE_CLAUDE,
            "claude_code.api_request",
            vec![
                kv_str("model", "claude-opus-4-7"),
                kv_str("effort", "max"),
                kv(
                    "duration_ms",
                    AnyValue {
                        value: Some(OtlpValue::IntValue(1000)),
                    },
                ),
            ],
        );
        agg.ingest_logs(&log_req);
        // Then metrics teach us the full name; the bare bucket must fold in.
        let metric_req = make_metric_req(
            SERVICE_CLAUDE,
            vec![claude_token_metric(
                "claude-opus-4-7[1m]",
                "max",
                "input",
                7,
            )],
        );
        agg.ingest_metrics(&metric_req);
        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CLAUDE).unwrap();
        assert!(!agent.buckets.contains_key("anthropic/claude-opus-4-7/max"));
        let bucket = agent
            .buckets
            .get("anthropic/claude-opus-4-7[1m]/max")
            .unwrap();
        assert_eq!(bucket.stats.request_count, 1);
        assert_eq!(bucket.stats.duration_ms, 1000);
        assert_eq!(bucket.stats.input_tokens, 7);
    }

    #[test]
    fn codex_token_metric_skips_total_and_maps_others() {
        let agg = Aggregator::new();
        let req = make_metric_req(
            SERVICE_CODEX_EXEC,
            vec![
                codex_token_metric("gpt-5.5", "input", 100.0),
                codex_token_metric("gpt-5.5", "output", 20.0),
                codex_token_metric("gpt-5.5", "cached_input", 50.0),
                codex_token_metric("gpt-5.5", "reasoning_output", 30.0),
                codex_token_metric("gpt-5.5", "total", 9999.0), // must be ignored
            ],
        );
        agg.ingest_metrics(&req);
        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CODEX).unwrap();
        let bucket_key = format!("{PROVIDER_OPENAI}/gpt-5.5/{UNKNOWN}");
        let bucket = agent.buckets.get(&bucket_key).unwrap();
        assert_eq!(bucket.stats.input_tokens, 100);
        assert_eq!(bucket.stats.output_tokens, 20);
        assert_eq!(bucket.stats.cache_read_tokens, 50);
        assert_eq!(bucket.stats.reasoning_output_tokens, 30);
    }

    #[test]
    fn codex_request_count_and_duration_from_metrics() {
        let agg = Aggregator::new();
        let req = make_metric_req(
            SERVICE_CODEX_EXEC,
            vec![
                codex_turn_count("gpt-5.4-mini", 1),
                codex_turn_count("gpt-5.4-mini", 1),
                codex_duration_metric("gpt-5.4-mini", 5000.0),
            ],
        );
        agg.ingest_metrics(&req);
        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CODEX).unwrap();
        let bucket_key = format!("{PROVIDER_OPENAI}/gpt-5.4-mini/{UNKNOWN}");
        let bucket = agent.buckets.get(&bucket_key).unwrap();
        assert_eq!(bucket.stats.request_count, 2);
        assert_eq!(bucket.stats.duration_ms, 5000);
    }

    #[test]
    fn codex_handle_responses_span_updates_effort_for_later_metrics() {
        let agg = Aggregator::new();
        // 1回目: conversation_starts が high で届く。
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "codex.conversation_starts",
            vec![
                kv_str("provider_name", PROVIDER_OPENAI),
                kv_str("model", "gpt-5.5"),
                kv_str("reasoning_effort", "high"),
            ],
        ));
        // 2回目: conversation_starts は届かず、handle_responses span だけが xhigh を運んでくる。
        agg.ingest_traces(&make_trace_req(
            SERVICE_CODEX_EXEC,
            vec![handle_responses_span("xhigh")],
        ));
        // その後に届くメトリクスは xhigh バケットに計上されるべき。
        agg.ingest_metrics(&make_metric_req(
            SERVICE_CODEX_EXEC,
            vec![codex_token_metric("gpt-5.5", "input", 42.0)],
        ));

        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CODEX).unwrap();
        let bucket = agent
            .buckets
            .get(&format!("{PROVIDER_OPENAI}/gpt-5.5/xhigh"))
            .expect("xhigh bucket should be populated via span fallback");
        assert_eq!(bucket.stats.input_tokens, 42);
    }

    #[test]
    fn codex_handle_responses_span_without_conversation_starts_seeds_session() {
        let agg = Aggregator::new();
        // conversation_starts を一度も観測しないまま handle_responses span が先着しても、
        // effort が回収できることを確認する (provider は OpenAI 既定にフォールバック)。
        agg.ingest_traces(&make_trace_req(
            SERVICE_CODEX_EXEC,
            vec![handle_responses_span("xhigh")],
        ));
        agg.ingest_metrics(&make_metric_req(
            SERVICE_CODEX_EXEC,
            vec![codex_token_metric("gpt-5.5", "input", 7.0)],
        ));

        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CODEX).unwrap();
        let bucket = agent
            .buckets
            .get(&format!("{PROVIDER_OPENAI}/gpt-5.5/xhigh"))
            .expect("xhigh bucket should be populated via span fallback");
        assert_eq!(bucket.stats.input_tokens, 7);
    }

    #[test]
    fn cumulative_temporality_is_dropped() {
        let agg = Aggregator::new();
        // Build a Cumulative Sum manually.
        let mut metric = claude_token_metric("claude-opus-4-7[1m]", "max", "input", 999);
        if let Some(MetricData::Sum(ref mut sum)) = metric.data {
            sum.aggregation_temporality = AggregationTemporality::Cumulative as i32;
        }
        let req = make_metric_req(SERVICE_CLAUDE, vec![metric]);
        agg.ingest_metrics(&req);
        let snap = agg.snapshot();
        assert!(snap.agents.is_empty());
    }
}
