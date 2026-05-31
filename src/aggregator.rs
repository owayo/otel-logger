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
        self.request_count = self.request_count.saturating_add(sample.request_count);
        self.input_tokens = self.input_tokens.saturating_add(sample.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(sample.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(sample.cache_read_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(sample.cache_creation_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(sample.reasoning_output_tokens);
        self.cost_usd = finite_saturating_add_f64(self.cost_usd, sample.cost_usd);
        self.duration_ms = self.duration_ms.saturating_add(sample.duration_ms);
    }

    fn subtract(&mut self, sample: &ModelStats) {
        self.request_count = self.request_count.saturating_sub(sample.request_count);
        self.input_tokens = self.input_tokens.saturating_sub(sample.input_tokens);
        self.output_tokens = self.output_tokens.saturating_sub(sample.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_sub(sample.cache_read_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_sub(sample.cache_creation_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_sub(sample.reasoning_output_tokens);
        self.cost_usd = (self.cost_usd - sample.cost_usd).max(0.0);
        self.duration_ms = self.duration_ms.saturating_sub(sample.duration_ms);
    }

    fn has_usage(&self) -> bool {
        self.input_tokens != 0
            || self.output_tokens != 0
            || self.cache_read_tokens != 0
            || self.cache_creation_tokens != 0
            || self.reasoning_output_tokens != 0
            || self.cost_usd != 0.0
    }

    /// `request_count` / `duration_ms` も含めた「何か値が入っているか」の判定。
    /// バケット削除条件で使う (`has_usage` は token/cost 系のみ判定するため不十分)。
    fn has_any_value(&self) -> bool {
        self.has_usage() || self.request_count != 0 || self.duration_ms != 0
    }

    /// `self - other` を saturating で返す (`self` は変更しない)。
    fn saturating_sub_stats(&self, other: &ModelStats) -> ModelStats {
        let mut out = self.clone();
        out.subtract(other);
        out
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

    /// Claude は API request ログと metrics の両方に token/cost を持つ。
    /// ログは request_id 単位で即時に届き、metrics より新しい分まで含むため、
    /// ログが見えた model/effort ではログを token source として採用する。
    claude_usage_sources: HashMap<String, ClaudeUsageSource>,

    /// metrics を先に計上した後で同じ model/effort の API request ログが届いた場合に、
    /// 二重計上を避けるため取り消す metrics 側の累計 usage。
    claude_metric_usage: HashMap<String, ModelStats>,

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

    /// `conversation.id` 付きの SSE 完了ログを受信したが、対応する `conversation_starts`
    /// session が未受信のため `effort=unknown` バケットに入った usage の内訳。
    /// 後から session が届いた conversation の分だけを切り出して effort バケットへ
    /// 移すために使う。並行する別 conversation の usage を巻き込まないことが目的。
    codex_unknown_effort_sse: HashMap<UnknownEffortKey, ModelStats>,
}

/// `codex_unknown_effort_sse` 用の合成キー。
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct UnknownEffortKey {
    provider: String,
    model: String,
    conversation_id: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeUsageSource {
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
                claude_usage_sources: HashMap::new(),
                claude_metric_usage: HashMap::new(),
                codex_last_session: None,
                codex_sessions: HashMap::new(),
                codex_token_sources: HashMap::new(),
                codex_unknown_effort_sse: HashMap::new(),
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
                        && let Some((bucket, request_stats, usage_stats)) =
                            extract_claude_api_request_meta(&mut g, log)
                    {
                        record_into(&mut g, AGENT_CLAUDE, &bucket, &request_stats);
                        record_claude_log_usage(&mut g, &bucket, &usage_stats);
                        count += 1;
                        continue;
                    }
                    if (service == SERVICE_CODEX_TUI || service == SERVICE_CODEX_EXEC)
                        && let Some(session) = extract_codex_conversation_starts(log)
                    {
                        update_codex_session(&mut g, session);
                    }
                    if (service == SERVICE_CODEX_TUI || service == SERVICE_CODEX_EXEC)
                        && let Some(parsed) = extract_codex_sse_response_completed(&g, log)
                    {
                        let CodexSseExtraction {
                            bucket,
                            stats,
                            unknown_conversation_id,
                        } = parsed;
                        let source_key = codex_token_source_key(&bucket.model);
                        if g.codex_token_sources.get(&source_key)
                            != Some(&CodexTokenSource::Metrics)
                        {
                            record_into(&mut g, AGENT_CODEX, &bucket, &stats);
                            // unknown effort で計上した分のうち、conversation.id が分かっている
                            // ものは、後で session が届いたら正しい effort バケットへ移せるよう
                            // pending として控えておく (model/conversation の組ごとに合算)。
                            if let Some(cid) = unknown_conversation_id {
                                let key = UnknownEffortKey {
                                    provider: bucket.provider.clone(),
                                    model: bucket.model.clone(),
                                    conversation_id: cid,
                                };
                                g.codex_unknown_effort_sse
                                    .entry(key)
                                    .or_default()
                                    .add(&stats);
                            }
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

fn subtract_from(g: &mut AggregatorInner, agent: &str, bucket: &Bucket, stats: &ModelStats) {
    let Some(agent_stats) = g.agents.get_mut(agent) else {
        return;
    };
    agent_stats.total.subtract(stats);
    if let Some(bucket_stats) = agent_stats.buckets.get_mut(&bucket.key()) {
        bucket_stats.stats.subtract(stats);
    }
}

fn record_claude_log_usage(g: &mut AggregatorInner, bucket: &Bucket, stats: &ModelStats) {
    if !stats.has_usage() {
        return;
    }
    let source_key = claude_usage_source_key(&bucket.model, &bucket.effort);
    if g.claude_usage_sources.get(&source_key) == Some(&ClaudeUsageSource::Metrics)
        && let Some(metric_stats) = g.claude_metric_usage.remove(&source_key)
    {
        subtract_from(g, AGENT_CLAUDE, bucket, &metric_stats);
    }
    record_into(g, AGENT_CLAUDE, bucket, stats);
    g.claude_usage_sources
        .insert(source_key, ClaudeUsageSource::Logs);
}

fn record_claude_metric_usage(
    g: &mut AggregatorInner,
    bucket: &Bucket,
    stats: &ModelStats,
) -> bool {
    if !stats.has_usage() {
        return false;
    }
    let source_key = claude_usage_source_key(&bucket.model, &bucket.effort);
    if g.claude_usage_sources.get(&source_key) == Some(&ClaudeUsageSource::Logs) {
        return false;
    }
    record_into(g, AGENT_CLAUDE, bucket, stats);
    g.claude_usage_sources
        .insert(source_key.clone(), ClaudeUsageSource::Metrics);
    g.claude_metric_usage
        .entry(source_key)
        .or_default()
        .add(stats);
    true
}

/// `claude_code.api_request` は `request_count` (1) と `duration_ms` を持つが、
/// `model` 属性から `[1m]` のようなバリアント接尾辞が落ちる。過去に観測した完全名へ
/// 戻して、metrics と同じバケットに入るようにする。
fn extract_claude_api_request_meta(
    g: &mut AggregatorInner,
    log: &LogRecord,
) -> Option<(Bucket, ModelStats, ModelStats)> {
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
    let request_stats = ModelStats {
        request_count: 1,
        duration_ms: int_attr(attrs, "duration_ms").unwrap_or(0).max(0) as u64,
        ..Default::default()
    };
    let usage_stats = ModelStats {
        input_tokens: u64_attr(attrs, "input_tokens"),
        output_tokens: u64_attr(attrs, "output_tokens"),
        cache_read_tokens: u64_attr(attrs, "cache_read_tokens"),
        cache_creation_tokens: u64_attr(attrs, "cache_creation_tokens"),
        cost_usd: f64_attr(attrs, "cost_usd").unwrap_or(0.0).max(0.0),
        ..Default::default()
    };
    Some((
        Bucket::from_parts(PROVIDER_ANTHROPIC, model, effort),
        request_stats,
        usage_stats,
    ))
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

struct CodexSseExtraction {
    bucket: Bucket,
    stats: ModelStats,
    /// `conversation.id` 付きで届いたが、対応する session が未受信のため effort=unknown と
    /// なった場合に限り `Some(id)` を返す。後続の `update_codex_session` で当該 conversation
    /// の usage だけを切り出して effort バケットへ移すために使う。
    unknown_conversation_id: Option<String>,
}

fn extract_codex_sse_response_completed(
    g: &AggregatorInner,
    log: &LogRecord,
) -> Option<CodexSseExtraction> {
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

    // `conversation.id` がある SSE は、その conversation の session にだけ紐付ける。
    // 未受信の場合に `codex_last_session` へフォールバックすると、並行/継続 conversation の
    // effort を誤って付与する恐れがある。id が無い古い telemetry のみ last session を使う。
    let conversation_id = string_attr(attrs, "conversation.id").map(str::to_string);
    let has_conversation_id = conversation_id.as_deref().is_some_and(|id| !id.is_empty());
    let session = if has_conversation_id {
        codex_session_for_attrs(g, attrs)
    } else {
        g.codex_last_session.as_ref()
    };
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
    let bucket = Bucket::from_parts(
        codex_provider_from_session(session),
        model,
        codex_effort_from_session(session),
    );
    // session 未受信のまま unknown effort バケットへ落ちる conversation.id 付き SSE は、
    // 後で session が届いたときにバケット移動できるよう pending として保留する目印を返す。
    let unknown_conversation_id = if has_conversation_id && bucket.effort == UNKNOWN {
        conversation_id
    } else {
        None
    };
    Some(CodexSseExtraction {
        bucket,
        stats,
        unknown_conversation_id,
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
        if record_claude_metric_usage(g, &bucket, &stats) {
            hits += 1;
        }
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
        if record_claude_metric_usage(g, &bucket, &stats) {
            hits += 1;
        }
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

fn claude_usage_source_key(model: &str, effort: &str) -> String {
    let bare_model = model.split_once('[').map(|(bare, _)| bare).unwrap_or(model);
    format!(
        "{}/{}",
        non_empty(bare_model.to_string()),
        non_empty(effort.to_string())
    )
}

fn update_codex_session(g: &mut AggregatorInner, session: CodexSession) {
    let provider = session.provider.clone();
    let model = session.model.clone();
    let effort = session.effort.clone();
    let conversation_id = session.conversation_id.clone();
    if !conversation_id.is_empty() {
        g.codex_sessions
            .insert(conversation_id.clone(), session.clone());
    }
    g.codex_last_session = Some(session);
    merge_codex_unknown_effort(g, &provider, &model, &conversation_id, &effort);
}

/// `conversation_starts` (または相当する span event) が遅れて届いたとき、
/// それまで `effort=unknown` に積み上げていた SSE 完了 token を新しい effort バケットへ移す。
///
/// `conversation_id` が分かる場合は当該 conversation の累計だけ動かす。複数 conversation が
/// 同じ provider/model を共有している状況で、他の (未解決の) conversation 分まで巻き込まない
/// ようにするのが目的。conversation_id が無い古い telemetry のみ、unknown バケット全体を
/// 寄せる旧挙動を維持する。
fn merge_codex_unknown_effort(
    g: &mut AggregatorInner,
    provider: &str,
    model: &str,
    conversation_id: &str,
    effort: &str,
) {
    if model.is_empty() || effort.is_empty() || effort == UNKNOWN {
        return;
    }
    let from = Bucket::from_parts(provider.to_string(), model.to_string(), UNKNOWN.to_string());
    let to = Bucket::from_parts(provider.to_string(), model.to_string(), effort.to_string());
    if from == to {
        return;
    }
    if !conversation_id.is_empty() {
        let key = UnknownEffortKey {
            provider: from.provider.clone(),
            model: from.model.clone(),
            conversation_id: conversation_id.to_string(),
        };
        let Some(pending) = g.codex_unknown_effort_sse.remove(&key) else {
            return;
        };
        move_codex_bucket_stats(g, &from, &to, &pending);
        return;
    }
    // conversation_id が空: 古い telemetry 互換。unknown バケットの残り全体を新 effort へ移す。
    // ただし他 conversation の pending と取り違えないよう、pending として保留されている分は
    // 残しておく (該当 conversation の session 到達時に正しく動かすため)。
    let pending_total = sum_pending_for_model(g, &from.provider, &from.model);
    let Some(agent_stats) = g.agents.get(AGENT_CODEX) else {
        return;
    };
    let Some(from_stats) = agent_stats.buckets.get(&from.key()).cloned() else {
        return;
    };
    let movable = from_stats.stats.saturating_sub_stats(&pending_total);
    if !movable.has_any_value() {
        return;
    }
    move_codex_bucket_stats(g, &from, &to, &movable);
}

fn move_codex_bucket_stats(
    g: &mut AggregatorInner,
    from: &Bucket,
    to: &Bucket,
    stats: &ModelStats,
) {
    let Some(agent_stats) = g.agents.get_mut(AGENT_CODEX) else {
        return;
    };
    if let Some(from_entry) = agent_stats.buckets.get_mut(&from.key()) {
        from_entry.stats.subtract(stats);
        if !from_entry.stats.has_any_value() {
            agent_stats.buckets.remove(&from.key());
        }
    }
    let entry = agent_stats
        .buckets
        .entry(to.key())
        .or_insert_with(|| BucketStats {
            provider: to.provider.clone(),
            model: to.model.clone(),
            effort: to.effort.clone(),
            stats: ModelStats::default(),
        });
    entry.stats.add(stats);
}

fn sum_pending_for_model(g: &AggregatorInner, provider: &str, model: &str) -> ModelStats {
    let mut total = ModelStats::default();
    for (key, stats) in &g.codex_unknown_effort_sse {
        if key.provider == provider && key.model == model {
            total.add(stats);
        }
    }
    total
}

/// `codex.conversation_starts` が欠けた時の effort 補完元。
/// `codex.request.*` は OTel semantic convention ではなく Codex CLI 内部の
/// telemetry なので、対象は `handle_responses` spans に限定し、`effort` だけを更新する
/// (provider/model は `conversation_starts` または metric data point 側に任せる)。
///
/// span 側に `conversation.id` がある場合は、その conversation の既知 session の effort
/// だけを更新する。`codex_last_session` を黙って上書きすると、並行する別 conversation の
/// effort バケットへ値を寄せてしまう。`conversation.id` が無い古い span に限り、最後の
/// session に対する fallback として作用させる。
fn update_codex_effort_from_request_attrs(g: &mut AggregatorInner, attrs: &[KeyValue]) -> bool {
    let Some(effort) = string_attr(attrs, "codex.request.reasoning_effort") else {
        return false;
    };
    if effort.is_empty() {
        return false;
    }
    let conversation_id = string_attr(attrs, "conversation.id").filter(|id| !id.is_empty());
    let session = if let Some(cid) = conversation_id {
        match g.codex_sessions.get(cid).cloned() {
            Some(mut session) => {
                session.effort = effort.to_string();
                session
            }
            None => CodexSession {
                conversation_id: cid.to_string(),
                provider: PROVIDER_OPENAI.to_string(),
                model: string_attr(attrs, "model")
                    .map(str::to_string)
                    .unwrap_or_default(),
                effort: effort.to_string(),
            },
        }
    } else {
        match g.codex_last_session.clone() {
            Some(mut session) => {
                session.effort = effort.to_string();
                session
            }
            None => CodexSession {
                conversation_id: String::new(),
                provider: PROVIDER_OPENAI.to_string(),
                model: String::new(),
                effort: effort.to_string(),
            },
        }
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
        Some(NumberValue::AsDouble(d)) => finite_u64_from_f64(d),
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
        Some(s) => finite_u64_from_f64(s),
        _ => 0,
    }
}

fn finite_u64_from_f64(value: f64) -> u64 {
    // OTLP は外部から受けるため、巨大な finite double を `as u64` で飽和させると
    // `u64::MAX` が累計に混入する。安全に収まる非負値だけを採用する。
    if value.is_finite() && value >= 0.0 && value < u64::MAX as f64 {
        value as u64
    } else {
        0
    }
}

fn finite_saturating_add_f64(lhs: f64, rhs: f64) -> f64 {
    let lhs = if lhs.is_finite() && lhs > 0.0 {
        lhs
    } else {
        0.0
    };
    let rhs = if rhs.is_finite() && rhs > 0.0 {
        rhs
    } else {
        0.0
    };
    let sum = lhs + rhs;
    if sum.is_finite() { sum } else { f64::MAX }
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
        // 信頼できない telemetry source からの NaN/Infinity / range 外の double が
        // `i64::MAX` 等にサチって累計を破壊しないよう、有限かつ i64 範囲内の値のみ受け入れる。
        OtlpValue::DoubleValue(d)
            if d.is_finite() && *d >= i64::MIN as f64 && *d <= i64::MAX as f64 =>
        {
            Some(*d as i64)
        }
        OtlpValue::StringValue(s) => s.parse().ok(),
        _ => None,
    }
}

fn f64_attr(attrs: &[KeyValue], key: &str) -> Option<f64> {
    let v = attrs
        .iter()
        .find(|kv| kv.key == key)?
        .value
        .as_ref()?
        .value
        .as_ref()?;
    match v {
        // 累計集計 (`ModelStats::cost_usd` など) に Infinity / NaN が紛れると以後の値が
        // すべて壊れるため、有限な double のみ受け入れる。
        OtlpValue::DoubleValue(d) if d.is_finite() => Some(*d),
        OtlpValue::IntValue(i) => Some(*i as f64),
        OtlpValue::StringValue(s) => s.parse::<f64>().ok().filter(|v| v.is_finite()),
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

    #[test]
    fn model_stats_add_saturates_untrusted_totals() {
        let mut stats = ModelStats {
            request_count: u64::MAX,
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            cache_read_tokens: u64::MAX,
            cache_creation_tokens: u64::MAX,
            reasoning_output_tokens: u64::MAX,
            cost_usd: f64::MAX,
            duration_ms: u64::MAX,
        };

        stats.add(&ModelStats {
            request_count: 1,
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: 1,
            cache_creation_tokens: 1,
            reasoning_output_tokens: 1,
            cost_usd: f64::MAX,
            duration_ms: 1,
        });

        assert_eq!(stats.request_count, u64::MAX);
        assert_eq!(stats.input_tokens, u64::MAX);
        assert_eq!(stats.output_tokens, u64::MAX);
        assert_eq!(stats.cache_read_tokens, u64::MAX);
        assert_eq!(stats.cache_creation_tokens, u64::MAX);
        assert_eq!(stats.reasoning_output_tokens, u64::MAX);
        assert_eq!(stats.duration_ms, u64::MAX);
        assert!(stats.cost_usd.is_finite(), "cost_usd は Infinity にしない");
        assert_eq!(stats.cost_usd, f64::MAX);
    }

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
    fn kv_double(key: &str, value: f64) -> KeyValue {
        kv(
            key,
            AnyValue {
                value: Some(OtlpValue::DoubleValue(value)),
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
        handle_responses_span_with_attrs(vec![kv_str("codex.request.reasoning_effort", effort)])
    }

    /// span に追加 attribute を載せて handle_responses を組み立てるテスト用ヘルパー。
    /// 並行 conversation の effort 振り分け regression test で使う。
    fn handle_responses_span_with_attrs(
        attrs: Vec<KeyValue>,
    ) -> opentelemetry_proto::tonic::trace::v1::Span {
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
            attributes: attrs,
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
    fn claude_api_request_log_counts_usage_without_metrics() {
        let agg = Aggregator::new();
        let log_req = make_log_req(
            SERVICE_CLAUDE,
            "claude_code.api_request",
            vec![
                kv_str("model", "claude-opus-4-7"),
                kv_str("effort", "max"),
                kv_int("input_tokens", 3),
                kv_int("output_tokens", 20),
                kv_int("cache_read_tokens", 100),
                kv_int("cache_creation_tokens", 7),
                kv_double("cost_usd", 0.0123),
                kv_int("duration_ms", 1200),
            ],
        );
        agg.ingest_logs(&log_req);
        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CLAUDE).unwrap();
        let bucket = agent.buckets.get("anthropic/claude-opus-4-7/max").unwrap();
        assert_eq!(bucket.stats.request_count, 1);
        assert_eq!(bucket.stats.input_tokens, 3);
        assert_eq!(bucket.stats.output_tokens, 20);
        assert_eq!(bucket.stats.cache_read_tokens, 100);
        assert_eq!(bucket.stats.cache_creation_tokens, 7);
        assert_eq!(bucket.stats.duration_ms, 1200);
        assert!((bucket.stats.cost_usd - 0.0123).abs() < 1e-9);
    }

    #[test]
    fn claude_log_usage_prevents_later_metric_double_count() {
        let agg = Aggregator::new();
        let log_req = make_log_req(
            SERVICE_CLAUDE,
            "claude_code.api_request",
            vec![
                kv_str("model", "claude-opus-4-7"),
                kv_str("effort", "max"),
                kv_int("input_tokens", 10),
                kv_int("output_tokens", 5),
                kv_double("cost_usd", 0.5),
                kv_int("duration_ms", 900),
            ],
        );
        agg.ingest_logs(&log_req);
        let metric_req = make_metric_req(
            SERVICE_CLAUDE,
            vec![
                claude_token_metric("claude-opus-4-7", "max", "input", 999),
                claude_cost_metric("claude-opus-4-7", "max", 9.0),
            ],
        );
        agg.ingest_metrics(&metric_req);
        let snap = agg.snapshot();
        let bucket = snap
            .agents
            .get(AGENT_CLAUDE)
            .unwrap()
            .buckets
            .get("anthropic/claude-opus-4-7/max")
            .unwrap();
        assert_eq!(bucket.stats.request_count, 1);
        assert_eq!(bucket.stats.input_tokens, 10);
        assert_eq!(bucket.stats.output_tokens, 5);
        assert_eq!(bucket.stats.duration_ms, 900);
        assert!((bucket.stats.cost_usd - 0.5).abs() < 1e-9);
    }

    #[test]
    fn claude_log_usage_replaces_earlier_metric_source() {
        let agg = Aggregator::new();
        let metric_req = make_metric_req(
            SERVICE_CLAUDE,
            vec![
                claude_token_metric("claude-opus-4-7[1m]", "max", "input", 999),
                claude_cost_metric("claude-opus-4-7[1m]", "max", 9.0),
            ],
        );
        agg.ingest_metrics(&metric_req);
        let log_req = make_log_req(
            SERVICE_CLAUDE,
            "claude_code.api_request",
            vec![
                kv_str("model", "claude-opus-4-7"),
                kv_str("effort", "max"),
                kv_int("input_tokens", 10),
                kv_int("output_tokens", 5),
                kv_double("cost_usd", 0.5),
                kv_int("duration_ms", 900),
            ],
        );
        agg.ingest_logs(&log_req);
        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CLAUDE).unwrap();
        assert!(!agent.buckets.contains_key("anthropic/claude-opus-4-7/max"));
        let bucket = agent
            .buckets
            .get("anthropic/claude-opus-4-7[1m]/max")
            .unwrap();
        assert_eq!(bucket.stats.request_count, 1);
        assert_eq!(bucket.stats.input_tokens, 10);
        assert_eq!(bucket.stats.output_tokens, 5);
        assert_eq!(bucket.stats.duration_ms, 900);
        assert!((bucket.stats.cost_usd - 0.5).abs() < 1e-9);
        assert_eq!(agent.total.input_tokens, 10);
        assert!((agent.total.cost_usd - 0.5).abs() < 1e-9);
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
                codex_token_metric("gpt-5.5", "total", 9999.0), // 重複するため無視される
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
    fn codex_sse_with_unknown_conversation_id_does_not_borrow_other_session_effort() {
        let agg = Aggregator::new();
        // 既知の session (medium) を一つ登録しておく。
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

        // session が未受信の別 conversation.id を持つ SSE が届いたとき、
        // medium バケットへは入らず unknown バケットへ落ちる必要がある。
        let count = agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "",
            vec![
                kv_str("event.name", "codex.sse_event"),
                kv_str("event.kind", "response.completed"),
                kv_str("conversation.id", "conv-missing"),
                kv_str("model", "gpt-5.5"),
                kv_str("input_token_count", "100"),
            ],
        ));

        assert_eq!(count, 1);
        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CODEX).unwrap();
        assert!(
            !agent
                .buckets
                .contains_key(&format!("{PROVIDER_OPENAI}/gpt-5.5/medium")),
            "別 conversation の medium バケットへ誤って計上されてはならない"
        );
        let bucket = agent
            .buckets
            .get(&format!("{PROVIDER_OPENAI}/gpt-5.5/{UNKNOWN}"))
            .unwrap();
        assert_eq!(bucket.stats.input_tokens, 100);
    }

    #[test]
    fn codex_late_conversation_start_moves_unknown_conversation_sse_tokens() {
        let agg = Aggregator::new();
        // 先に SSE 完了ログを受け、その時点では session が未受信なので unknown へ入る。
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "",
            vec![
                kv_str("event.name", "codex.sse_event"),
                kv_str("event.kind", "response.completed"),
                kv_str("conversation.id", "conv-late"),
                kv_str("model", "gpt-5.5"),
                kv_str("input_token_count", "100"),
            ],
        ));
        // 後から conversation_starts が届けば、unknown -> high へマージされる。
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "codex.conversation_starts",
            vec![
                kv_str("conversation.id", "conv-late"),
                kv_str("provider_name", PROVIDER_OPENAI),
                kv_str("model", "gpt-5.5"),
                kv_str("reasoning_effort", "high"),
            ],
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

    /// 並行する複数 conversation のうち、片方だけ session が遅延到着しても、
    /// もう一方の conversation の pending tokens を巻き込まないことを確認する
    /// (`merge_codex_unknown_effort` の regression test)。
    #[test]
    fn codex_late_session_only_moves_matching_conversation_tokens() {
        let agg = Aggregator::new();
        // 2 つの conversation が同じ model で並行し、どちらも session 未着のまま
        // unknown effort バケットへ積まれている状況を作る。
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "",
            vec![
                kv_str("event.name", "codex.sse_event"),
                kv_str("event.kind", "response.completed"),
                kv_str("conversation.id", "conv-a"),
                kv_str("model", "gpt-5.5"),
                kv_str("input_token_count", "100"),
            ],
        ));
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "",
            vec![
                kv_str("event.name", "codex.sse_event"),
                kv_str("event.kind", "response.completed"),
                kv_str("conversation.id", "conv-b"),
                kv_str("model", "gpt-5.5"),
                kv_str("input_token_count", "50"),
            ],
        ));
        // conv-a 用 session が遅れて high で届く。
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "codex.conversation_starts",
            vec![
                kv_str("conversation.id", "conv-a"),
                kv_str("provider_name", PROVIDER_OPENAI),
                kv_str("model", "gpt-5.5"),
                kv_str("reasoning_effort", "high"),
            ],
        ));

        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CODEX).unwrap();
        // conv-a の 100 tokens だけが high バケットへ移動する。
        let high = agent
            .buckets
            .get(&format!("{PROVIDER_OPENAI}/gpt-5.5/high"))
            .expect("conv-a 用 high バケットは作成されているはず");
        assert_eq!(high.stats.input_tokens, 100);
        // conv-b の 50 tokens は unknown に残ったまま。
        let unknown = agent
            .buckets
            .get(&format!("{PROVIDER_OPENAI}/gpt-5.5/{UNKNOWN}"))
            .expect("conv-b の pending は unknown バケットに残る");
        assert_eq!(unknown.stats.input_tokens, 50);
        // 全体合計は変わらないこと (二重計上・取りこぼしが無い)。
        assert_eq!(agent.total.input_tokens, 150);
    }

    /// `handle_responses` span の `conversation.id` を尊重して、対応する session 以外の
    /// effort を破壊しないことを確認する regression test。
    #[test]
    fn codex_handle_responses_span_respects_conversation_id() {
        let agg = Aggregator::new();
        // 2 つの conversation が並行する。
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "codex.conversation_starts",
            vec![
                kv_str("conversation.id", "conv-a"),
                kv_str("provider_name", PROVIDER_OPENAI),
                kv_str("model", "gpt-5.5"),
                kv_str("reasoning_effort", "high"),
            ],
        ));
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "codex.conversation_starts",
            vec![
                kv_str("conversation.id", "conv-b"),
                kv_str("provider_name", PROVIDER_OPENAI),
                kv_str("model", "gpt-5.5"),
                kv_str("reasoning_effort", "medium"),
            ],
        ));
        // 最後の session は conv-b/medium。conv-a を狙った handle_responses span が xhigh を運ぶ。
        agg.ingest_traces(&make_trace_req(
            SERVICE_CODEX_EXEC,
            vec![handle_responses_span_with_attrs(vec![
                kv_str("conversation.id", "conv-a"),
                kv_str("codex.request.reasoning_effort", "xhigh"),
            ])],
        ));
        // conv-b の SSE が届いたとき、effort=medium のままになっているべき
        // (conv-a の xhigh 上書きが conv-b へ波及してはならない)。
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "",
            vec![
                kv_str("event.name", "codex.sse_event"),
                kv_str("event.kind", "response.completed"),
                kv_str("conversation.id", "conv-b"),
                kv_str("model", "gpt-5.5"),
                kv_str("input_token_count", "30"),
            ],
        ));

        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CODEX).unwrap();
        assert!(
            agent
                .buckets
                .get(&format!("{PROVIDER_OPENAI}/gpt-5.5/medium"))
                .is_some_and(|b| b.stats.input_tokens == 30),
            "conv-b の token は medium バケットに残る (xhigh に巻き込まれない)"
        );
    }

    /// `int_attr` / `f64_attr` が NaN / Infinity を受け取った時、累計を破壊しないことを確認する。
    #[test]
    fn claude_api_request_log_ignores_non_finite_double_values() {
        let agg = Aggregator::new();
        let log_req = make_log_req(
            SERVICE_CLAUDE,
            "claude_code.api_request",
            vec![
                kv_str("model", "claude-opus-4-7"),
                kv_str("effort", "max"),
                // double で infinity / NaN が紛れても、累計が壊れないこと。
                kv_double("input_tokens", f64::INFINITY),
                kv_double("output_tokens", f64::NAN),
                kv_double("duration_ms", f64::NEG_INFINITY),
                kv_double("cost_usd", f64::INFINITY),
            ],
        );
        agg.ingest_logs(&log_req);
        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CLAUDE).unwrap();
        let bucket = agent.buckets.get("anthropic/claude-opus-4-7/max").unwrap();
        assert_eq!(bucket.stats.request_count, 1);
        assert_eq!(bucket.stats.input_tokens, 0);
        assert_eq!(bucket.stats.output_tokens, 0);
        assert_eq!(bucket.stats.duration_ms, 0);
        assert!(
            bucket.stats.cost_usd.is_finite() && bucket.stats.cost_usd == 0.0,
            "cost_usd は有限な 0 を保つ"
        );
    }

    /// metric の double 値が `u64` 範囲外でも、飽和した巨大値を累計に混ぜないことを確認する。
    #[test]
    fn metric_double_values_outside_u64_range_are_ignored() {
        let in_range = make_double_dp(vec![], 42.9);
        assert_eq!(number_value_as_u64(&in_range), 42);

        let too_large_number = make_double_dp(vec![], f64::MAX);
        assert_eq!(number_value_as_u64(&too_large_number), 0);

        let rounded_past_u64_max = make_double_dp(vec![], u64::MAX as f64);
        assert_eq!(number_value_as_u64(&rounded_past_u64_max), 0);

        let too_large_histogram = make_hist_dp(vec![], f64::MAX);
        assert_eq!(histogram_sum_as_u64(&too_large_histogram), 0);

        let rounded_histogram = make_hist_dp(vec![], u64::MAX as f64);
        assert_eq!(histogram_sum_as_u64(&rounded_histogram), 0);
    }

    /// `conversation_id` 付きの SSE 完了ログを受けた後、別 conversation の session が
    /// 届いても、unknown バケットの内容が無関係な effort バケットへ移動しないことを確認する。
    #[test]
    fn codex_session_for_unrelated_conversation_does_not_move_pending_tokens() {
        let agg = Aggregator::new();
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "",
            vec![
                kv_str("event.name", "codex.sse_event"),
                kv_str("event.kind", "response.completed"),
                kv_str("conversation.id", "conv-a"),
                kv_str("model", "gpt-5.5"),
                kv_str("input_token_count", "100"),
            ],
        ));
        // 関係ない conv-b の session が medium で届く。
        agg.ingest_logs(&make_log_req(
            SERVICE_CODEX_EXEC,
            "codex.conversation_starts",
            vec![
                kv_str("conversation.id", "conv-b"),
                kv_str("provider_name", PROVIDER_OPENAI),
                kv_str("model", "gpt-5.5"),
                kv_str("reasoning_effort", "medium"),
            ],
        ));

        let snap = agg.snapshot();
        let agent = snap.agents.get(AGENT_CODEX).unwrap();
        assert!(
            !agent
                .buckets
                .contains_key(&format!("{PROVIDER_OPENAI}/gpt-5.5/medium")),
            "conv-b の session で conv-a の pending を medium へ動かしてはならない"
        );
        let unknown = agent
            .buckets
            .get(&format!("{PROVIDER_OPENAI}/gpt-5.5/{UNKNOWN}"))
            .unwrap();
        assert_eq!(unknown.stats.input_tokens, 100);
    }
}
