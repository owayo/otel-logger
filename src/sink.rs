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

/// Internal normalized record. JSONL serialization is lossless because the
/// inner OTLP protobuf types are kept verbatim; pretty rendering pulls out
/// only the noteworthy fields.
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

/// JSONL output backend. Either a single append-only file or a daily-rotated
/// directory writer (both expose a synchronous `std::io::Write` underneath,
/// so all writes happen on a `spawn_blocking` thread).
enum JsonlWriter {
    File(StdMutex<StdBufWriter<std::fs::File>>),
    Roller(StdMutex<LogRoller>),
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
        }
    }
}

/// Output destination for received telemetry. Cloning is cheap (`Arc` inside),
/// so all gRPC / HTTP handlers share the same sink.
#[derive(Clone)]
pub struct Sink {
    inner: Arc<SinkInner>,
}

struct SinkInner {
    stdout_enabled: bool,
    color: bool,
    summary_enabled: bool,
    file: Option<JsonlWriter>,
    /// Coarse-grained mutex around stdout. Telemetry payloads can fan out
    /// many lines per record, and interleaving with another writer would make
    /// the human-readable stream unreadable.
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
                let writer = tokio::task::spawn_blocking(move || open_log_file_sync(&path))
                    .await
                    .context("join open_log_file task")??;
                Some(JsonlWriter::File(StdMutex::new(writer)))
            }
            Some(LogSink::Directory { dir, keep_days }) => {
                let dir = dir.clone();
                let keep_days = *keep_days;
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

    /// Borrow the running cumulative-usage aggregator. The HTTP `/stats`
    /// handler uses this to produce a snapshot on demand.
    pub fn aggregator(&self) -> &Aggregator {
        &self.inner.aggregator
    }

    /// Persist a single telemetry batch.
    /// Errors writing to one destination are logged but do not abort the other.
    pub async fn record(&self, record: TelemetryRecord) {
        // Update cumulative stats before writing so any summary line we append
        // to stdout reflects the current batch.
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

        if self.inner.file.is_some()
            && let Err(e) = self.write_jsonl(&record).await
        {
            tracing::error!(error = %e, kind = record.kind(), "failed to write JSONL");
        }

        if self.inner.stdout_enabled
            && let Err(e) = self.write_pretty(&record, summary_snapshot).await
        {
            tracing::error!(error = %e, kind = record.kind(), "failed to write stdout");
        }
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

    /// Flush any buffered writes. Called from graceful shutdown so SIGTERM
    /// does not lose the trailing batch.
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
        // stdout is line-buffered when attached to a terminal; do a best-effort
        // flush via spawn_blocking.
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

/// Remove `otel-logger.*` files older than `keep_days` (by mtime). Mirrors
/// claw-hooks's `cleanup_old_logs` so behavior is consistent across our
/// internal CLIs.
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
        if !filename.starts_with(ROTATION_PREFIX) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn touch_mtime(path: &Path, when: SystemTime) {
        // `File::set_times` + `FileTimes::set_modified` are stable cross-platform.
        let times = std::fs::FileTimes::new().set_modified(when);
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(times).unwrap();
    }

    #[test]
    fn cleanup_removes_only_old_otel_logger_prefixed_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let old = dir.path().join("otel-logger.2020-01-01");
        let recent = dir.path().join("otel-logger.2099-01-01");
        let unrelated = dir.path().join("other-app.log");
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&recent, "recent").unwrap();
        std::fs::write(&unrelated, "other").unwrap();

        let three_days_ago = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        touch_mtime(&old, three_days_ago);
        touch_mtime(&unrelated, three_days_ago);

        cleanup_old_rotated_logs(dir.path(), 1).unwrap();

        assert!(!old.exists(), "old otel-logger file should be deleted");
        assert!(recent.exists(), "recent otel-logger file should be kept");
        assert!(unrelated.exists(), "unrelated file must not be touched");
    }
}
