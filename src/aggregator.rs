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

/// バケットごとの累計値。エージェントが持たない語彙のフィールドは 0 のままにする
/// (例: Codex の `cost_usd`、Claude の `reasoning_output_tokens`)。
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

/// 内部集計で `BTreeMap` のキーに使う provider × model × effort の組。
/// 公開せず、シリアライズ用の値型 `BucketStats` 側にも同じ情報を持たせる。
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

    /// シリアライズ出力で使う安定したキー: `provider/model/effort`。
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
    /// provider/model/effort ごとのバケット。Map のキーは `Bucket::key()`。
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

    /// Claude モデル名の正規化。Anthropic のログでは `model` から
    /// `claude-opus-4-7` のようにバリアント接尾辞が落ちる一方、対応する
    /// metrics/spans には `claude-opus-4-7[1m]` のような完全名が入る。
    /// metric で観測した完全名を覚えておき、後続のログ側バケットを昇格して
    /// 1M 版と通常版が別集計にならないようにする。
    claude_canonical_models: HashMap<String, String>,

    /// `codex.conversation_starts` の log/event から最後に観測した Codex セッション情報。
    /// Codex metrics には `reasoning_effort` が載らないため、直近のセッション値に
    /// フォールバックする。
    codex_last_session: Option<CodexSession>,

    /// `conversation.id` ごとの Codex セッション情報。
    /// SSE 完了ログには `conversation.id` が載るため、同時進行のセッションが混ざっても
    /// 最後に見た別セッションの effort へ誤って寄せないようにする。
    codex_sessions: HashMap<String, CodexSession>,

    /// Codex は SSE 完了ログと turn metrics の両方で token usage を出す。
    /// metrics 側に conversation id が無いため、両方に存在する model 単位で
    /// 最初に観測したソースを採用し、二重計上を避ける。
    codex_token_sources: HashMap<String, CodexTokenSource>,
}

#[derive(Debug, Clone)]
struct CodexSession {
    conversation_id: String,
    provider: String,
    model: String,
    effort: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexTokenSource {
    Logs,
    Metrics,
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
                codex_sessions: HashMap::new(),
                codex_token_sources: HashMap::new(),
            }),
        }
    }

    /// log events を取り込む。Claude はログから `request_count` と `duration_ms` を補い、
    /// Codex は `effort` 補完用のセッション情報と、実ログで metrics より多く観測できる
    /// SSE 完了ログの token usage を取り込む。
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
                        update_codex_session(&mut g, session);
                    }
                    if (service == SERVICE_CODEX_TUI || service == SERVICE_CODEX_EXEC)
                        && let Some((bucket, stats)) = extract_codex_sse_response_completed(&g, log)
                    {
                        let source_key = codex_token_source_key(&bucket.model);
                        if g.codex_token_sources.get(&source_key)
                            != Some(&CodexTokenSource::Metrics)
                        {
                            record_into(&mut g, AGENT_CODEX, &bucket, &stats);
                            g.codex_token_sources
                                .insert(source_key, CodexTokenSource::Logs);
                            count += 1;
                        }
                    }
                }
            }
        }
        count
    }

    /// trace spans を取り込む。Codex のセッション情報は `session_init` 上の
    /// `codex.conversation_starts` span event としても届くため、ここから
    /// `reasoning_effort` を回収する。token/cost の計上は別経路に寄せる。
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
                            update_codex_session(&mut g, session);
                            session_updates += 1;
                        }
                    }
                    // `codex.conversation_starts` が落ちても、同じジョブの後続実行では
                    // `handle_responses` span に effort が残るため、そこから補完する。
                    if span.name == "handle_responses"
                        && update_codex_effort_from_request_attrs(&mut g, &span.attributes)
                    {
                        session_updates += 1;
                    }
                }
            }
        }
        // spans 自体は usage stats のサンプルではない。セッション情報を更新できた時だけ
        // >0 を返し、呼び出し側が必要に応じてサマリーを出せるようにする。
        session_updates
    }

    /// metric data を取り込む。Claude の token/cost と Codex の duration/request count
    /// はここで集計する。temporality は DELTA のみ扱い、CUMULATIVE は再起動をまたいだ
    /// 状態保持が必要になるため警告して破棄する。
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

/// `claude_code.api_request` は `request_count` (1) と `duration_ms` を持つが、
/// `model` 属性から `[1m]` のようなバリアント接尾辞が落ちる。過去に観測した完全名へ
/// 戻して、metrics と同じバケットに入るようにする。
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
    // Codex はこれを top-level log event または span event として送る。
    // ここでは log record 形式だけを扱い、body と event.name のどちらに目印が
    // 入っていても拾えるようにする。
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
    let conversation_id = string_attr(attrs, "conversation.id")
        .map(str::to_string)
        .unwrap_or_default();
    let provider = string_attr(attrs, "provider_name")
        .map(str::to_string)
        .unwrap_or_else(|| PROVIDER_OPENAI.to_string());
    let model = string_attr(attrs, "model")
        .map(str::to_string)
        .unwrap_or_default();
    let effort = string_attr(attrs, "reasoning_effort")
        .map(str::to_string)
        .unwrap_or_default();
    if conversation_id.is_empty() && model.is_empty() && effort.is_empty() {
        return None;
    }
    Some(CodexSession {
        conversation_id,
        provider,
        model,
        effort,
    })
}

fn extract_codex_sse_response_completed(
    g: &AggregatorInner,
    log: &LogRecord,
) -> Option<(Bucket, ModelStats)> {
    let attrs = &log.attributes;
    if string_attr(attrs, "event.name") != Some("codex.sse_event")
        || string_attr(attrs, "event.kind") != Some("response.completed")
    {
        return None;
    }

    let input_tokens = u64_attr(attrs, "input_token_count");
    let output_tokens = u64_attr(attrs, "output_token_count");
    let cache_read_tokens = u64_attr(attrs, "cached_token_count");
    let reasoning_output_tokens = u64_attr(attrs, "reasoning_token_count");
    let tool_tokens = u64_attr(attrs, "tool_token_count");
    // 実ログでは tool context だけの完了イベントが先に届く。turn metrics と
    // `handle_responses` span usage には含まれないため、usage としては数えない。
    if input_tokens != 0
        && input_tokens == tool_tokens
        && output_tokens == 0
        && cache_read_tokens == 0
        && reasoning_output_tokens == 0
    {
        return None;
    }
    if input_tokens == 0
        && output_tokens == 0
        && cache_read_tokens == 0
        && reasoning_output_tokens == 0
    {
        return None;
    }

    let session = codex_session_for_attrs(g, attrs).or(g.codex_last_session.as_ref());
    let model = string_attr(attrs, "model")
        .filter(|model| !model.is_empty())
        .or_else(|| string_attr(attrs, "slug").filter(|slug| !slug.is_empty()))
        .map(str::to_string)
        .or_else(|| session.map(|s| s.model.clone()))
        .unwrap_or_default();
    let stats = ModelStats {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        reasoning_output_tokens,
        ..Default::default()
    };
    Some((
        Bucket::from_parts(
            codex_provider_from_session(session),
            model,
            codex_effort_from_session(session),
        ),
        stats,
    ))
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
        let source_key = codex_token_source_key(&model);
        if g.codex_token_sources.get(&source_key) == Some(&CodexTokenSource::Logs) {
            continue;
        }
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
            // `total` は他の token 種別と重複するため無視する。
            _ => continue,
        }
        let bucket = Bucket::from_parts(provider, model, effort);
        record_into(g, AGENT_CODEX, &bucket, &stats);
        g.codex_token_sources
            .insert(source_key, CodexTokenSource::Metrics);
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
    codex_provider_from_session(g.codex_last_session.as_ref())
}

fn codex_effort(g: &AggregatorInner) -> String {
    codex_effort_from_session(g.codex_last_session.as_ref())
}

fn codex_provider_from_session(session: Option<&CodexSession>) -> String {
    session
        .map(|s| s.provider.clone())
        .unwrap_or_else(|| PROVIDER_OPENAI.to_string())
}

fn codex_effort_from_session(session: Option<&CodexSession>) -> String {
    session.map(|s| s.effort.clone()).unwrap_or_default()
}

fn codex_session_for_attrs<'a>(
    g: &'a AggregatorInner,
    attrs: &[KeyValue],
) -> Option<&'a CodexSession> {
    let conversation_id = string_attr(attrs, "conversation.id")?;
    if conversation_id.is_empty() {
        return None;
    }
    g.codex_sessions.get(conversation_id)
}

fn codex_token_source_key(model: &str) -> String {
    non_empty(model.to_string())
}

fn update_codex_session(g: &mut AggregatorInner, session: CodexSession) {
    let provider = session.provider.clone();
    let model = session.model.clone();
    let effort = session.effort.clone();
    if !session.conversation_id.is_empty() {
        g.codex_sessions
            .insert(session.conversation_id.clone(), session.clone());
    }
    g.codex_last_session = Some(session);
    merge_codex_unknown_effort(g, &provider, &model, &effort);
}

fn merge_codex_unknown_effort(g: &mut AggregatorInner, provider: &str, model: &str, effort: &str) {
    if model.is_empty() || effort.is_empty() || effort == UNKNOWN {
        return;
    }
    let from = Bucket::from_parts(provider.to_string(), model.to_string(), UNKNOWN.to_string());
    let to = Bucket::from_parts(provider.to_string(), model.to_string(), effort.to_string());
    if from == to {
        return;
    }
    let Some(agent_stats) = g.agents.get_mut(AGENT_CODEX) else {
        return;
    };
    let Some(from_stats) = agent_stats.buckets.remove(&from.key()) else {
        return;
    };
    let entry = agent_stats
        .buckets
        .entry(to.key())
        .or_insert_with(|| BucketStats {
            provider: to.provider,
            model: to.model,
            effort: to.effort,
            stats: ModelStats::default(),
        });
    entry.stats.add(&from_stats.stats);
}

/// `codex.conversation_starts` が欠けた時の effort 補完元。
/// `codex.request.*` は OTel semantic convention ではなく Codex CLI 内部の
/// telemetry なので、対象は `handle_responses` spans に限定し、`effort` だけを更新する
/// (provider/model は `conversation_starts` または metric data point 側に任せる)。
fn update_codex_effort_from_request_attrs(g: &mut AggregatorInner, attrs: &[KeyValue]) -> bool {
    let Some(effort) = string_attr(attrs, "codex.request.reasoning_effort") else {
        return false;
    };
    if effort.is_empty() {
        return false;
    }
    let session = match g.codex_last_session.clone() {
        Some(mut session) => {
            session.effort = effort.to_string();
            session
        }
        None => CodexSession {
            conversation_id: string_attr(attrs, "conversation.id")
                .map(str::to_string)
                .unwrap_or_default(),
            provider: PROVIDER_OPENAI.to_string(),
            model: String::new(),
            effort: effort.to_string(),
        },
    };
    update_codex_session(g, session);
    true
}

/// Anthropic logs では `claude-opus-4-7`、metrics/spans では
/// `claude-opus-4-7[1m]` のように報告される。`[` より前を bare name として扱い、
/// 完全名を覚えて後続ログを metrics と同じバケットへ寄せる。
fn register_canonical_claude_model(g: &mut AggregatorInner, full: &str) {
    let bare = match full.find('[') {
        Some(i) => &full[..i],
        None => full,
    };
    if bare.is_empty() || bare == full {
        // 正規化すべき接尾辞がないため、追加で覚えるものはない。
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
    // bare name のバケットが先に作られていた場合は、snapshot に重複表示されないよう
    // 完全名のバケットへ統合する。
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

fn u64_attr(attrs: &[KeyValue], key: &str) -> u64 {
    int_attr(attrs, key).unwrap_or(0).max(0) as u64
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
            key_strindex: 0,
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
    fn kv_int(key: &str, value: i64) -> KeyValue {
        kv(
            key,
            AnyValue {
                value: Some(OtlpValue::IntValue(value)),
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

    fn conversation_starts_span(
        attrs: Vec<KeyValue>,
    ) -> opentelemetry_proto::tonic::trace::v1::Span {
        opentelemetry_proto::tonic::trace::v1::Span {
            trace_id: vec![1; 16],
            span_id: vec![2; 8],
            trace_state: String::new(),
            parent_span_id: vec![],
            flags: 0,
            name: "session_init".into(),
            kind: 0,
            start_time_unix_nano: 1,
            end_time_unix_nano: 2,
            attributes: vec![],
            dropped_attributes_count: 0,
            events: vec![opentelemetry_proto::tonic::trace::v1::span::Event {
                time_unix_nano: 0,
                name: "codex.conversation_starts".into(),
                attributes: attrs,
                dropped_attributes_count: 0,
            }],
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
        // metrics が先に届き、正規の完全名バケットを確立する。
        let metric_req = make_metric_req(
            SERVICE_CLAUDE,
            vec![
                claude_token_metric("claude-opus-4-7[1m]", "max", "input", 10),
                claude_cost_metric("claude-opus-4-7[1m]", "max", 0.5),
            ],
        );
        agg.ingest_metrics(&metric_req);
        // ログは bare model 名で届くが、[1m] バケットへ統合されるべき。
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
        // ログが先に bare name で届く。
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
        // その後 metrics で完全名が分かるため、bare バケットを統合する必要がある。
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
    fn codex_sse_response_completed_log_counts_token_usage() {
        let agg = Aggregator::new();
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "codex.conversation_starts",
            vec![
                kv_str("provider_name", PROVIDER_OPENAI),
                kv_str("model", "gpt-5.5"),
                kv_str("reasoning_effort", "xhigh"),
            ],
        ));
        let count = agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "",
            vec![
                kv_str("event.name", "codex.sse_event"),
                kv_str("event.kind", "response.completed"),
                kv_str("model", "gpt-5.5"),
                kv_str("input_token_count", "100"),
                kv_str("output_token_count", "20"),
                kv_int("cached_token_count", 50),
                kv_int("reasoning_token_count", 30),
                kv_str("tool_token_count", "120"),
            ],
        ));

        assert_eq!(count, 1);
        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CODEX).unwrap();
        let bucket = agent
            .buckets
            .get(&format!("{PROVIDER_OPENAI}/gpt-5.5/xhigh"))
            .unwrap();
        assert_eq!(bucket.stats.input_tokens, 100);
        assert_eq!(bucket.stats.output_tokens, 20);
        assert_eq!(bucket.stats.cache_read_tokens, 50);
        assert_eq!(bucket.stats.reasoning_output_tokens, 30);
        assert_eq!(bucket.stats.request_count, 0);
    }

    #[test]
    fn codex_sse_uses_matching_conversation_effort_when_sessions_interleave() {
        let agg = Aggregator::new();
        // 実ログでは複数 conversation の SSE が混在するため、最後に見た session ではなく
        // `conversation.id` が一致する session の effort を使う必要がある。
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "codex.conversation_starts",
            vec![
                kv_str("conversation.id", "conv-xhigh"),
                kv_str("provider_name", PROVIDER_OPENAI),
                kv_str("model", "gpt-5.5"),
                kv_str("reasoning_effort", "xhigh"),
            ],
        ));
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "codex.conversation_starts",
            vec![
                kv_str("conversation.id", "conv-medium"),
                kv_str("provider_name", PROVIDER_OPENAI),
                kv_str("model", "gpt-5.5"),
                kv_str("reasoning_effort", "medium"),
            ],
        ));

        let count = agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "",
            vec![
                kv_str("event.name", "codex.sse_event"),
                kv_str("event.kind", "response.completed"),
                kv_str("conversation.id", "conv-xhigh"),
                kv_str("model", "gpt-5.5"),
                kv_str("input_token_count", "100"),
                kv_str("output_token_count", "20"),
                kv_int("cached_token_count", 50),
                kv_int("reasoning_token_count", 30),
                kv_str("tool_token_count", "120"),
            ],
        ));

        assert_eq!(count, 1);
        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CODEX).unwrap();
        assert!(
            !agent
                .buckets
                .contains_key(&format!("{PROVIDER_OPENAI}/gpt-5.5/medium"))
        );
        let bucket = agent
            .buckets
            .get(&format!("{PROVIDER_OPENAI}/gpt-5.5/xhigh"))
            .unwrap();
        assert_eq!(bucket.stats.input_tokens, 100);
        assert_eq!(bucket.stats.output_tokens, 20);
        assert_eq!(bucket.stats.cache_read_tokens, 50);
        assert_eq!(bucket.stats.reasoning_output_tokens, 30);
    }

    #[test]
    fn codex_sse_tool_only_completion_is_ignored() {
        let agg = Aggregator::new();
        let count = agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "",
            vec![
                kv_str("event.name", "codex.sse_event"),
                kv_str("event.kind", "response.completed"),
                kv_str("model", "gpt-5.5"),
                kv_str("input_token_count", "13145"),
                kv_str("output_token_count", "0"),
                kv_int("cached_token_count", 0),
                kv_int("reasoning_token_count", 0),
                kv_str("tool_token_count", "13145"),
            ],
        ));

        assert_eq!(count, 0);
        assert!(!agg.snapshot().agents.contains_key(AGENT_CODEX));
    }

    #[test]
    fn codex_late_conversation_start_moves_sse_tokens_to_effort_bucket() {
        let agg = Aggregator::new();
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "",
            vec![
                kv_str("event.name", "codex.sse_event"),
                kv_str("event.kind", "response.completed"),
                kv_str("model", "gpt-5.5"),
                kv_str("input_token_count", "100"),
            ],
        ));
        agg.ingest_traces(&make_trace_req(
            SERVICE_CODEX_EXEC,
            vec![conversation_starts_span(vec![
                kv_str("event.name", "codex.conversation_starts"),
                kv_str("provider_name", PROVIDER_OPENAI),
                kv_str("model", "gpt-5.5"),
                kv_str("reasoning_effort", "high"),
            ])],
        ));

        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CODEX).unwrap();
        assert!(
            !agent
                .buckets
                .contains_key(&format!("{PROVIDER_OPENAI}/gpt-5.5/{UNKNOWN}"))
        );
        let bucket = agent
            .buckets
            .get(&format!("{PROVIDER_OPENAI}/gpt-5.5/high"))
            .unwrap();
        assert_eq!(bucket.stats.input_tokens, 100);
    }

    #[test]
    fn codex_sse_token_log_prevents_later_metric_double_count() {
        let agg = Aggregator::new();
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "",
            vec![
                kv_str("event.name", "codex.sse_event"),
                kv_str("event.kind", "response.completed"),
                kv_str("model", "gpt-5.5"),
                kv_str("input_token_count", "100"),
            ],
        ));
        agg.ingest_metrics(&make_metric_req(
            SERVICE_CODEX_EXEC,
            vec![codex_token_metric("gpt-5.5", "input", 999.0)],
        ));

        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CODEX).unwrap();
        let bucket = agent
            .buckets
            .get(&format!("{PROVIDER_OPENAI}/gpt-5.5/{UNKNOWN}"))
            .unwrap();
        assert_eq!(bucket.stats.input_tokens, 100);
    }

    #[test]
    fn codex_token_source_is_tracked_per_model() {
        let agg = Aggregator::new();
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "",
            vec![
                kv_str("event.name", "codex.sse_event"),
                kv_str("event.kind", "response.completed"),
                kv_str("model", "gpt-5.5"),
                kv_str("input_token_count", "100"),
            ],
        ));
        agg.ingest_metrics(&make_metric_req(
            SERVICE_CODEX_EXEC,
            vec![
                codex_token_metric("gpt-5.5", "input", 999.0),
                codex_token_metric("gpt-5.4-mini", "input", 40.0),
            ],
        ));

        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CODEX).unwrap();
        let gpt55 = agent
            .buckets
            .get(&format!("{PROVIDER_OPENAI}/gpt-5.5/{UNKNOWN}"))
            .unwrap();
        let gpt54 = agent
            .buckets
            .get(&format!("{PROVIDER_OPENAI}/gpt-5.4-mini/{UNKNOWN}"))
            .unwrap();
        assert_eq!(gpt55.stats.input_tokens, 100);
        assert_eq!(gpt54.stats.input_tokens, 40);
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
        // Cumulative Sum を手動で組み立てる。
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
