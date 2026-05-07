use std::fmt::Write as _;

use opentelemetry_proto::tonic::common::v1::any_value::Value as OtlpValue;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;
use opentelemetry_proto::tonic::metrics::v1::metric::Data as MetricData;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::aggregator::UsageSnapshot;
use crate::sink::TelemetryRecord;

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BLUE: &str = "\x1b[34m";
const MAGENTA: &str = "\x1b[35m";
const CYAN: &str = "\x1b[36m";

pub fn render(record: &TelemetryRecord, color: bool) -> String {
    let mut out = String::new();
    let p = Painter { color };
    match record {
        TelemetryRecord::Traces(req) => render_traces(&mut out, &p, req),
        TelemetryRecord::Metrics(req) => render_metrics(&mut out, &p, req),
        TelemetryRecord::Logs(req) => render_logs(&mut out, &p, req),
    }
    out
}

/// Cumulative usage summary, formatted for the human-readable stdout stream.
/// One block per agent (claude-code / codex), with per-(provider, model, effort)
/// breakdowns indented underneath.
pub fn render_summary(snapshot: &UsageSnapshot, color: bool) -> String {
    let p = Painter { color };
    let mut out = String::new();
    if snapshot.agents.is_empty() {
        return out;
    }
    for (agent, stats) in &snapshot.agents {
        let total = &stats.total;
        let _ = writeln!(
            out,
            "{tag} requests={requests} input={input} output={output} cache_read={cr} cache_create={cc} reasoning={r} cost={cost} duration={dur} since={since}",
            tag = p.paint(CYAN, &format!("[stats:{agent}]")),
            requests = p.bold(&total.request_count.to_string()),
            input = total.input_tokens,
            output = total.output_tokens,
            cr = total.cache_read_tokens,
            cc = total.cache_creation_tokens,
            r = total.reasoning_output_tokens,
            cost = format_cost(total.cost_usd),
            dur = format_duration_ns(total.duration_ms.saturating_mul(1_000_000)),
            since = p.dim(&snapshot.started_at),
        );
        for bucket in stats.buckets.values() {
            let _ = writeln!(
                out,
                "        {prefix} provider={provider} model={model} effort={effort}: requests={requests} input={input} output={output} cache_read={cr} cache_create={cc} reasoning={r} cost={cost}",
                prefix = p.dim("breakdown"),
                provider = p.bold(&bucket.provider),
                model = p.bold(&bucket.model),
                effort = p.bold(&bucket.effort),
                requests = bucket.stats.request_count,
                input = bucket.stats.input_tokens,
                output = bucket.stats.output_tokens,
                cr = bucket.stats.cache_read_tokens,
                cc = bucket.stats.cache_creation_tokens,
                r = bucket.stats.reasoning_output_tokens,
                cost = format_cost(bucket.stats.cost_usd),
            );
        }
    }
    out
}

fn format_cost(cost: f64) -> String {
    if cost == 0.0 {
        "$0".to_string()
    } else if cost < 0.01 {
        format!("${cost:.6}")
    } else {
        format!("${cost:.4}")
    }
}

struct Painter {
    color: bool,
}

impl Painter {
    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("{code}{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    fn dim(&self, text: &str) -> String {
        self.paint(DIM, text)
    }
    fn bold(&self, text: &str) -> String {
        self.paint(BOLD, text)
    }
}

fn render_traces(
    out: &mut String,
    p: &Painter,
    req: &opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest,
) {
    for resource_spans in &req.resource_spans {
        let service = service_name(resource_spans.resource.as_ref().map(|r| &r.attributes));
        for scope_spans in &resource_spans.scope_spans {
            let scope = scope_spans
                .scope
                .as_ref()
                .map(|s| s.name.as_str())
                .unwrap_or("");
            for span in &scope_spans.spans {
                let dur_ns = span
                    .end_time_unix_nano
                    .saturating_sub(span.start_time_unix_nano);
                let status_code = span.status.as_ref().map(|s| s.code).unwrap_or(0);
                let status = match status_code {
                    1 => "OK",
                    2 => "ERROR",
                    _ => "UNSET",
                };
                let status_painted = match status {
                    "OK" => p.paint(GREEN, status),
                    "ERROR" => p.paint(RED, status),
                    _ => p.dim(status),
                };
                let trace_id = hex_id(&span.trace_id);
                let span_id = hex_id(&span.span_id);
                let _ = writeln!(
                    out,
                    "{tag} {ts} service={service} scope={scope} span={name} dur={dur} status={status} trace={trace} span_id={span_id}",
                    tag = p.paint(MAGENTA, "[trace]"),
                    ts = p.dim(&format_unix_nanos(span.start_time_unix_nano)),
                    service = p.bold(&service),
                    scope = scope,
                    name = p.bold(&span.name),
                    dur = format_duration_ns(dur_ns),
                    status = status_painted,
                    trace = trace_id,
                    span_id = span_id,
                );
                if !span.attributes.is_empty() {
                    let attrs = format_attrs(&span.attributes);
                    let _ = writeln!(out, "        {} {}", p.dim("attrs:"), attrs);
                }
                for ev in &span.events {
                    let _ = writeln!(
                        out,
                        "        {} {} {}",
                        p.dim(&format!("event @{}", format_unix_nanos(ev.time_unix_nano))),
                        p.bold(&ev.name),
                        format_attrs(&ev.attributes),
                    );
                }
            }
        }
    }
}

fn render_metrics(
    out: &mut String,
    p: &Painter,
    req: &opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest,
) {
    for resource_metrics in &req.resource_metrics {
        let service = service_name(resource_metrics.resource.as_ref().map(|r| &r.attributes));
        for scope_metrics in &resource_metrics.scope_metrics {
            let scope = scope_metrics
                .scope
                .as_ref()
                .map(|s| s.name.as_str())
                .unwrap_or("");
            for metric in &scope_metrics.metrics {
                let unit = if metric.unit.is_empty() {
                    String::new()
                } else {
                    format!(" unit={}", metric.unit)
                };
                let summary = match &metric.data {
                    Some(MetricData::Sum(sum)) => {
                        let points: Vec<String> =
                            sum.data_points.iter().map(format_number_point).collect();
                        format!("sum=[{}]", points.join(", "))
                    }
                    Some(MetricData::Gauge(gauge)) => {
                        let points: Vec<String> =
                            gauge.data_points.iter().map(format_number_point).collect();
                        format!("gauge=[{}]", points.join(", "))
                    }
                    Some(MetricData::Histogram(hist)) => {
                        let points: Vec<String> = hist
                            .data_points
                            .iter()
                            .map(|dp| {
                                format!(
                                    "count={} sum={}",
                                    dp.count,
                                    dp.sum.map(|v| format!("{v}")).unwrap_or_default()
                                )
                            })
                            .collect();
                        format!("hist=[{}]", points.join(", "))
                    }
                    Some(MetricData::ExponentialHistogram(eh)) => {
                        format!("exp_hist=points={}", eh.data_points.len())
                    }
                    Some(MetricData::Summary(s)) => {
                        format!("summary=points={}", s.data_points.len())
                    }
                    None => "no_data".to_string(),
                };
                let _ = writeln!(
                    out,
                    "{tag} service={service} scope={scope} name={name}{unit} {summary} {desc}",
                    tag = p.paint(CYAN, "[metric]"),
                    service = p.bold(&service),
                    scope = scope,
                    name = p.bold(&metric.name),
                    unit = unit,
                    summary = summary,
                    desc = if metric.description.is_empty() {
                        String::new()
                    } else {
                        p.dim(&format!("({})", metric.description))
                    },
                );
            }
        }
    }
}

fn render_logs(
    out: &mut String,
    p: &Painter,
    req: &opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest,
) {
    for resource_logs in &req.resource_logs {
        let service = service_name(resource_logs.resource.as_ref().map(|r| &r.attributes));
        for scope_logs in &resource_logs.scope_logs {
            let scope = scope_logs
                .scope
                .as_ref()
                .map(|s| s.name.as_str())
                .unwrap_or("");
            for log in &scope_logs.log_records {
                let severity = severity_label(log.severity_number, &log.severity_text);
                let severity_painted = paint_severity(p, &severity);
                let body = log
                    .body
                    .as_ref()
                    .map(any_value_to_string)
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "{tag} {ts} service={service} scope={scope} severity={severity} body={body}",
                    tag = p.paint(YELLOW, "[log]"),
                    ts = p.dim(&format_unix_nanos(log.time_unix_nano)),
                    service = p.bold(&service),
                    scope = scope,
                    severity = severity_painted,
                    body = quote_for_pretty(&body),
                );
                if !log.attributes.is_empty() {
                    let attrs = format_attrs(&log.attributes);
                    let _ = writeln!(out, "        {} {}", p.dim("attrs:"), attrs);
                }
            }
        }
    }
}

fn paint_severity(p: &Painter, severity: &str) -> String {
    match severity {
        "ERROR" | "FATAL" => p.paint(RED, severity),
        "WARN" => p.paint(YELLOW, severity),
        "INFO" => p.paint(GREEN, severity),
        "DEBUG" => p.paint(BLUE, severity),
        "TRACE" => p.paint(MAGENTA, severity),
        _ => severity.to_string(),
    }
}

fn severity_label(number: i32, text: &str) -> String {
    if !text.is_empty() {
        return text.to_uppercase();
    }
    let severity = SeverityNumber::try_from(number).unwrap_or(SeverityNumber::Unspecified);
    match severity {
        SeverityNumber::Unspecified => "UNSPECIFIED".into(),
        SeverityNumber::Trace
        | SeverityNumber::Trace2
        | SeverityNumber::Trace3
        | SeverityNumber::Trace4 => "TRACE".into(),
        SeverityNumber::Debug
        | SeverityNumber::Debug2
        | SeverityNumber::Debug3
        | SeverityNumber::Debug4 => "DEBUG".into(),
        SeverityNumber::Info
        | SeverityNumber::Info2
        | SeverityNumber::Info3
        | SeverityNumber::Info4 => "INFO".into(),
        SeverityNumber::Warn
        | SeverityNumber::Warn2
        | SeverityNumber::Warn3
        | SeverityNumber::Warn4 => "WARN".into(),
        SeverityNumber::Error
        | SeverityNumber::Error2
        | SeverityNumber::Error3
        | SeverityNumber::Error4 => "ERROR".into(),
        SeverityNumber::Fatal
        | SeverityNumber::Fatal2
        | SeverityNumber::Fatal3
        | SeverityNumber::Fatal4 => "FATAL".into(),
    }
}

fn service_name(attrs: Option<&Vec<KeyValue>>) -> String {
    attrs
        .and_then(|kvs| {
            kvs.iter()
                .find(|kv| kv.key == "service.name")
                .and_then(|kv| kv.value.as_ref())
                .map(any_value_to_string)
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn any_value_to_string(v: &AnyValue) -> String {
    match v.value.as_ref() {
        Some(OtlpValue::StringValue(s)) => s.clone(),
        Some(OtlpValue::BoolValue(b)) => b.to_string(),
        Some(OtlpValue::IntValue(i)) => i.to_string(),
        Some(OtlpValue::DoubleValue(d)) => format!("{d}"),
        Some(OtlpValue::BytesValue(b)) => format!("0x{}", hex(b)),
        Some(OtlpValue::ArrayValue(arr)) => {
            let parts: Vec<String> = arr.values.iter().map(any_value_to_string).collect();
            format!("[{}]", parts.join(", "))
        }
        Some(OtlpValue::KvlistValue(kv)) => format_attrs(&kv.values),
        None => String::new(),
    }
}

fn format_attrs(attrs: &[KeyValue]) -> String {
    let parts: Vec<String> = attrs
        .iter()
        .map(|kv| {
            let value = kv
                .value
                .as_ref()
                .map(any_value_to_string)
                .unwrap_or_default();
            format!("{}={}", kv.key, quote_for_pretty(&value))
        })
        .collect();
    format!("{{{}}}", parts.join(", "))
}

fn format_number_point(dp: &opentelemetry_proto::tonic::metrics::v1::NumberDataPoint) -> String {
    use opentelemetry_proto::tonic::metrics::v1::number_data_point::Value;
    let v = match dp.value.as_ref() {
        Some(Value::AsDouble(d)) => format!("{d}"),
        Some(Value::AsInt(i)) => format!("{i}"),
        None => "?".into(),
    };
    if dp.attributes.is_empty() {
        v
    } else {
        format!("{v} {}", format_attrs(&dp.attributes))
    }
}

fn format_unix_nanos(nanos: u64) -> String {
    if nanos == 0 {
        return "-".into();
    }
    let secs = (nanos / 1_000_000_000) as i64;
    let sub = (nanos % 1_000_000_000) as i32;
    OffsetDateTime::from_unix_timestamp(secs)
        .ok()
        .and_then(|t| t.replace_nanosecond(sub as u32).ok())
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_else(|| nanos.to_string())
}

fn format_duration_ns(ns: u64) -> String {
    if ns == 0 {
        return "0ns".into();
    }
    if ns < 1_000 {
        return format!("{ns}ns");
    }
    if ns < 1_000_000 {
        return format!("{:.2}µs", ns as f64 / 1_000.0);
    }
    if ns < 1_000_000_000 {
        return format!("{:.2}ms", ns as f64 / 1_000_000.0);
    }
    format!("{:.3}s", ns as f64 / 1_000_000_000.0)
}

fn hex_id(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        "-".into()
    } else {
        hex(bytes)
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn quote_for_pretty(s: &str) -> String {
    if s.contains(' ') || s.contains('"') || s.contains('=') {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}
