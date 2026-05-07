use std::io::{self, Write as _};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use serde::Serialize;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;

use crate::aggregator::{Aggregator, UsageSnapshot};
use crate::cli::Settings;
use crate::format;

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
    file: Option<Mutex<BufWriter<File>>>,
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

        let file = match settings.log_file.as_deref() {
            Some(path) => Some(Mutex::new(open_log_file(path).await?)),
            None => None,
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

        if let Some(file) = self.inner.file.as_ref()
            && let Err(e) = self.write_jsonl(file, &record).await
        {
            tracing::error!(error = %e, kind = record.kind(), "failed to write JSONL");
        }

        if self.inner.stdout_enabled
            && let Err(e) = self.write_pretty(&record, summary_snapshot).await
        {
            tracing::error!(error = %e, kind = record.kind(), "failed to write stdout");
        }
    }

    async fn write_jsonl(
        &self,
        file: &Mutex<BufWriter<File>>,
        record: &TelemetryRecord,
    ) -> Result<()> {
        let mut line = serde_json::to_vec(record).context("serialize telemetry to JSON")?;
        line.push(b'\n');
        let mut guard = file.lock().await;
        guard
            .write_all(&line)
            .await
            .context("append JSONL line to log file")?;
        Ok(())
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
        if let Some(file) = self.inner.file.as_ref() {
            let mut guard = file.lock().await;
            guard.flush().await.context("flush JSONL log file")?;
            guard
                .get_mut()
                .sync_all()
                .await
                .context("fsync JSONL log file")?;
        }
        // stdout is line-buffered when attached to a terminal; do a best-effort
        // flush via spawn_blocking.
        let _ = tokio::task::spawn_blocking(|| {
            let _ = io::stdout().flush();
        })
        .await;
        Ok(())
    }
}

async fn open_log_file(path: &Path) -> Result<BufWriter<File>> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create parent directory of {}", path.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("open log file {}", path.display()))?;
    Ok(BufWriter::new(file))
}
