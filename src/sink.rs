use std::fs::OpenOptions as StdOpenOptions;
use std::io::{self, BufWriter as StdBufWriter, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use logroller::{LogRoller, LogRollerBuilder, Rotation, RotationAge, TimeZone};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use serde::Serialize;
use tokio::sync::Mutex;

use crate::aggregator::{Aggregator, UsageSnapshot};
use crate::cli::{LogSink, Settings};
use crate::format;

const ROTATION_PREFIX: &str = "otel-logger";

/// 内部で扱う正規化済み record。内部の OTLP protobuf 型をそのまま保持するため、
/// JSONL serialization は欠落しない。pretty rendering では注目すべき field だけを抜き出す。
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TelemetryRecord {
    Traces(Box<ExportTraceServiceRequest>),
    Metrics(Box<ExportMetricsServiceRequest>),
    Logs(Box<ExportLogsServiceRequest>),
}

impl TelemetryRecord {
    pub fn kind(&self) -> &'static str {
        match self {
            TelemetryRecord::Traces(_) => "traces",
            TelemetryRecord::Metrics(_) => "metrics",
            TelemetryRecord::Logs(_) => "logs",
        }
    }
}

/// JSONL 出力 backend。追記専用ファイルか日次ローテーション付き directory writer のどちらか。
/// どちらも内部は同期的な `std::io::Write` なので、書き込みは `spawn_blocking` 上で行う。
enum JsonlWriter {
    File(StdMutex<StdBufWriter<std::fs::File>>),
    Roller(StdMutex<LogRoller>),
    #[cfg(test)]
    Fail,
}

impl JsonlWriter {
    fn write_line(&self, line: &[u8]) -> Result<()> {
        match self {
            Self::File(m) => {
                let mut g = m.lock().expect("jsonl file mutex poisoned");
                g.write_all(line).context("append JSONL line")
            }
            Self::Roller(m) => {
                let mut g = m.lock().expect("jsonl roller mutex poisoned");
                g.write_all(line).context("append JSONL line (rotated)")
            }
            #[cfg(test)]
            Self::Fail => anyhow::bail!("forced JSONL failure"),
        }
    }

    fn flush(&self) -> Result<()> {
        match self {
            Self::File(m) => {
                let mut g = m.lock().expect("jsonl file mutex poisoned");
                g.flush().context("flush JSONL file")?;
                g.get_mut().sync_all().context("fsync JSONL file")
            }
            Self::Roller(m) => {
                let mut g = m.lock().expect("jsonl roller mutex poisoned");
                g.flush().context("flush JSONL roller")
            }
            #[cfg(test)]
            Self::Fail => Ok(()),
        }
    }
}

/// 受信した telemetry の出力先。内部が `Arc` なので clone は軽く、
/// すべての gRPC / HTTP handler が同じ sink を共有する。
#[derive(Clone)]
pub struct Sink {
    inner: Arc<SinkInner>,
}

struct SinkInner {
    stdout_enabled: bool,
    color: bool,
    summary_enabled: bool,
    file: Option<JsonlWriter>,
    /// stdout 全体を守る粗い mutex。telemetry payload は 1 record で多くの行に展開されるため、
    /// 別 writer と interleave すると人が読める stream として壊れる。
    stdout_lock: Mutex<()>,
    aggregator: Aggregator,
}

impl Sink {
    pub async fn from_settings(settings: &Settings) -> Result<Self> {
        let stdout_enabled = !settings.no_stdout;
        let color = settings.color.enabled_for_stdout() && stdout_enabled;

        let file = match settings.log_sink.as_ref() {
            None => None,
            Some(LogSink::File(path)) => {
                let path = path.clone();
                tracing::info!(path = %path.display(), "JSONL sink: appending to file");
                let writer = tokio::task::spawn_blocking(move || open_log_file_sync(&path))
                    .await
                    .context("join open_log_file task")??;
                Some(JsonlWriter::File(StdMutex::new(writer)))
            }
            Some(LogSink::Directory { dir, keep_days }) => {
                let dir = dir.clone();
                let keep_days = *keep_days;
                tracing::info!(
                    dir = %dir.display(),
                    keep_days,
                    "JSONL sink: rotating daily in directory"
                );
                let roller =
                    tokio::task::spawn_blocking(move || open_rotated_sync(&dir, keep_days))
                        .await
                        .context("join open_rotated task")??;
                Some(JsonlWriter::Roller(StdMutex::new(roller)))
            }
        };

        Ok(Self {
            inner: Arc::new(SinkInner {
                stdout_enabled,
                color,
                summary_enabled: settings.summary,
                file,
                stdout_lock: Mutex::new(()),
                aggregator: Aggregator::new(),
            }),
        })
    }

    /// 実行中の累計使用量 aggregator を借用する。
    /// HTTP `/stats` handler はこれを使い、要求時点の snapshot を生成する。
    pub fn aggregator(&self) -> &Aggregator {
        &self.inner.aggregator
    }

    /// 単一の telemetry batch を保存する。
    ///
    /// JSONL 出力が設定されている場合、永続化に失敗したら `Err` を返す。受信した
    /// payload を欠落なく保存する方針なので、失敗を握りつぶさず呼び出し元 (HTTP / gRPC
    /// handler) に伝え、OTLP exporter 側で retry できるようにする。
    /// stdout はベストエフォートで、書き込みに失敗しても tracing にだけ記録する。
    pub async fn record(&self, record: TelemetryRecord) -> Result<()> {
        if self.inner.file.is_some() {
            self.write_jsonl(&record)
                .await
                .with_context(|| format!("persist {} batch to JSONL", record.kind()))?;
        }

        // JSONL 永続化が成功した batch だけを集計する。失敗時は exporter が retry するため、
        // 先に集計すると同じ payload を二重計上してしまう。
        let samples_present = match &record {
            TelemetryRecord::Logs(req) => self.inner.aggregator.ingest_logs(req) > 0,
            TelemetryRecord::Traces(req) => self.inner.aggregator.ingest_traces(req) > 0,
            TelemetryRecord::Metrics(req) => self.inner.aggregator.ingest_metrics(req) > 0,
        };
        let summary_snapshot = if samples_present && self.inner.summary_enabled {
            Some(self.inner.aggregator.snapshot())
        } else {
            None
        };

        if self.inner.stdout_enabled
            && let Err(e) = self.write_pretty(&record, summary_snapshot).await
        {
            tracing::error!(error = %e, kind = record.kind(), "failed to write stdout");
        }
        Ok(())
    }

    async fn write_jsonl(&self, record: &TelemetryRecord) -> Result<()> {
        let mut line = serde_json::to_vec(record).context("serialize telemetry to JSON")?;
        line.push(b'\n');
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(writer) = inner.file.as_ref() {
                writer.write_line(&line)?;
            }
            Ok(())
        })
        .await
        .context("join JSONL write task")?
    }

    async fn write_pretty(
        &self,
        record: &TelemetryRecord,
        summary: Option<UsageSnapshot>,
    ) -> Result<()> {
        let _guard = self.inner.stdout_lock.lock().await;
        let rendered = format::render(record, self.inner.color);
        let summary_rendered = summary.map(|s| format::render_summary(&s, self.inner.color));
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            let stdout = io::stdout();
            let mut handle = stdout.lock();
            handle.write_all(rendered.as_bytes())?;
            if let Some(s) = summary_rendered {
                handle.write_all(s.as_bytes())?;
            }
            handle.flush()?;
            Ok(())
        })
        .await
        .context("join stdout write task")?
        .context("write to stdout")?;
        Ok(())
    }

    /// buffer 済み書き込みを flush する。SIGTERM でも末尾 batch を失わないよう、
    /// graceful shutdown から呼び出す。
    pub async fn flush(&self) -> Result<()> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(writer) = inner.file.as_ref() {
                writer.flush()?;
            }
            Ok(())
        })
        .await
        .context("join JSONL flush task")??;
        // terminal 接続時の stdout は line-buffered なので、spawn_blocking で best-effort flush する。
        let _ = tokio::task::spawn_blocking(|| {
            let _ = io::stdout().flush();
        })
        .await;
        Ok(())
    }
}

fn open_log_file_sync(path: &Path) -> Result<StdBufWriter<std::fs::File>> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent directory of {}", path.display()))?;
    }
    let file = StdOpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open log file {}", path.display()))?;
    Ok(StdBufWriter::new(file))
}

fn open_rotated_sync(dir: &Path, keep_days: u32) -> Result<LogRoller> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create log directory {}", dir.display()))?;
    cleanup_old_rotated_logs(dir, keep_days)
        .with_context(|| format!("cleanup old log files in {}", dir.display()))?;
    let max_keep = keep_days.max(1) as u64;
    let appender = LogRollerBuilder::new(dir, Path::new(ROTATION_PREFIX))
        .rotation(Rotation::AgeBased(RotationAge::Daily))
        .time_zone(TimeZone::Local)
        .max_keep_files(max_keep)
        .build()
        .map_err(|e| anyhow::anyhow!("build log roller: {e}"))?;
    Ok(appender)
}

/// mtime 基準で `keep_days` より古い `otel-logger.*` ファイルを削除する。
/// claw-hooks の `cleanup_old_logs` と揃え、社内 CLI 間で挙動を一貫させる。
fn cleanup_old_rotated_logs(dir: &Path, keep_days: u32) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(u64::from(keep_days) * 24 * 60 * 60))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_rotated_log_filename(filename) {
            continue;
        }
        if let Ok(metadata) = entry.metadata()
            && let Ok(modified) = metadata.modified()
            && modified < cutoff
        {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

fn is_rotated_log_filename(filename: &str) -> bool {
    let Some(date) = filename.strip_prefix(&format!("{ROTATION_PREFIX}.")) else {
        return false;
    };
    let bytes = date.as_bytes();
    bytes.len() == 10
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv_str(key: &str, value: &str) -> opentelemetry_proto::tonic::common::v1::KeyValue {
        use opentelemetry_proto::tonic::common::v1::AnyValue;
        use opentelemetry_proto::tonic::common::v1::any_value::Value as OtlpValue;

        opentelemetry_proto::tonic::common::v1::KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(OtlpValue::StringValue(value.to_string())),
            }),
            key_strindex: 0,
        }
    }

    fn claude_api_request_log() -> ExportLogsServiceRequest {
        use opentelemetry_proto::tonic::common::v1::any_value::Value as OtlpValue;
        use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope};
        use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
        use opentelemetry_proto::tonic::resource::v1::Resource;

        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                resource: Some(Resource {
                    attributes: vec![kv_str("service.name", "claude-code")],
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
                            value: Some(OtlpValue::StringValue(
                                "claude_code.api_request".to_string(),
                            )),
                        }),
                        attributes: vec![
                            kv_str("model", "claude-opus-4-7"),
                            kv_str("effort", "max"),
                            kv_str("input_tokens", "1"),
                            kv_str("output_tokens", "2"),
                            kv_str("cache_read_tokens", "3"),
                            kv_str("cache_creation_tokens", "4"),
                            kv_str("duration_ms", "5"),
                            kv_str("cost_usd", "0.01"),
                        ],
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

    fn failing_jsonl_sink() -> Sink {
        Sink {
            inner: Arc::new(SinkInner {
                stdout_enabled: false,
                color: false,
                summary_enabled: true,
                file: Some(JsonlWriter::Fail),
                stdout_lock: Mutex::new(()),
                aggregator: Aggregator::new(),
            }),
        }
    }

    fn touch_mtime(path: &Path, when: SystemTime) {
        // `File::set_times` + `FileTimes::set_modified` は cross-platform で安定している。
        let times = std::fs::FileTimes::new().set_modified(when);
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(times).unwrap();
    }

    #[test]
    fn cleanup_removes_only_old_otel_logger_prefixed_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let old = dir.path().join("otel-logger.2020-01-01");
        let recent = dir.path().join("otel-logger.2099-01-01");
        let pid = dir.path().join("otel-logger.pid");
        let stderr = dir.path().join("otel-logger.stderr.log");
        let jsonl = dir.path().join("otel-logger.jsonl");
        let unrelated = dir.path().join("other-app.log");
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&recent, "recent").unwrap();
        std::fs::write(&pid, "12345").unwrap();
        std::fs::write(&stderr, "stderr").unwrap();
        std::fs::write(&jsonl, "jsonl").unwrap();
        std::fs::write(&unrelated, "other").unwrap();

        let three_days_ago = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        touch_mtime(&old, three_days_ago);
        touch_mtime(&pid, three_days_ago);
        touch_mtime(&stderr, three_days_ago);
        touch_mtime(&jsonl, three_days_ago);
        touch_mtime(&unrelated, three_days_ago);

        cleanup_old_rotated_logs(dir.path(), 1).unwrap();

        assert!(!old.exists(), "old otel-logger file should be deleted");
        assert!(recent.exists(), "recent otel-logger file should be kept");
        assert!(pid.exists(), "pid file must not be touched");
        assert!(stderr.exists(), "stderr file must not be touched");
        assert!(jsonl.exists(), "non-rotated JSONL file must not be touched");
        assert!(unrelated.exists(), "unrelated file must not be touched");
    }

    fn settings_with_log_file(path: std::path::PathBuf) -> crate::cli::Settings {
        use crate::cli::{ColorMode, LogSink};
        crate::cli::Settings {
            grpc_addr: "127.0.0.1:0".parse().unwrap(),
            http_addr: "127.0.0.1:0".parse().unwrap(),
            log_sink: Some(LogSink::File(path)),
            no_stdout: true,
            summary: false,
            color: ColorMode::Never,
            dry_run: false,
        }
    }

    /// 正常系: JSONL writer が成功すれば `record` は Ok を返し、ファイルに 1 行追記される。
    #[tokio::test]
    async fn record_persists_payload_and_returns_ok() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("otel-logger.jsonl");
        let sink = Sink::from_settings(&settings_with_log_file(log_path.clone()))
            .await
            .unwrap();

        let req = ExportLogsServiceRequest {
            resource_logs: vec![],
        };
        sink.record(TelemetryRecord::Logs(Box::new(req)))
            .await
            .expect("record で永続化に成功すること");
        sink.flush().await.unwrap();

        let body = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            body.contains("\"kind\":\"logs\""),
            "JSON Lines に追記される"
        );
        assert!(body.ends_with('\n'), "各レコードは改行で終わる");
    }

    #[tokio::test]
    async fn record_does_not_update_stats_when_jsonl_write_fails() {
        let sink = failing_jsonl_sink();
        let result = sink
            .record(TelemetryRecord::Logs(Box::new(claude_api_request_log())))
            .await;

        assert!(result.is_err(), "JSONL 永続化失敗は呼び出し元へ返す");
        assert!(
            sink.aggregator().snapshot().agents.is_empty(),
            "retry 対象の payload は永続化成功まで集計しない"
        );
    }
}
