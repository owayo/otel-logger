//! Replay saved JSONL telemetry into the aggregator and print the resulting totals.
//! 保存済み JSONL テレメトリを集計器へ再生し、累計を出力する検証用ツール。
//!
//! JSONL の各行は `{"kind":"logs|metrics|traces", "resourceLogs|resourceMetrics|resourceSpans":[...]}`
//! というフラット構造なので、`kind` を取り除いた残りを OTLP export request として復元する。
//! 到着順を保ったまま再生するため、順序依存の二重計上バグをそのまま再現できる。
//!
//! Usage / 使い方:
//!   cargo run --release --example replay_check -- <file.jsonl> [more.jsonl ...]

use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::process::ExitCode;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use otel_logger::aggregator::Aggregator;

fn main() -> ExitCode {
    let paths: Vec<String> = env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: replay_check <file.jsonl> [more.jsonl ...]");
        return ExitCode::FAILURE;
    }

    let aggregator = Aggregator::new();
    let mut lines_total = 0usize;
    let mut decode_failures = 0usize;

    for path in &paths {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) => {
                eprintln!("failed to open {path}: {err}");
                return ExitCode::FAILURE;
            }
        };
        for line in BufReader::new(file).lines() {
            let line = match line {
                Ok(line) => line,
                Err(err) => {
                    eprintln!("failed to read {path}: {err}");
                    return ExitCode::FAILURE;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            lines_total += 1;
            let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&line) else {
                decode_failures += 1;
                continue;
            };
            // `kind` は otel-logger が付与した振り分け用フィールドで OTLP 本体には無い。
            let kind = value
                .as_object_mut()
                .and_then(|obj| obj.remove("kind"))
                .and_then(|kind| kind.as_str().map(str::to_string))
                .unwrap_or_default();
            match kind.as_str() {
                "logs" => match serde_json::from_value::<ExportLogsServiceRequest>(value) {
                    Ok(req) => {
                        aggregator.ingest_logs(&req);
                    }
                    Err(_) => decode_failures += 1,
                },
                "metrics" => match serde_json::from_value::<ExportMetricsServiceRequest>(value) {
                    Ok(req) => {
                        aggregator.ingest_metrics(&req);
                    }
                    Err(_) => decode_failures += 1,
                },
                "traces" => match serde_json::from_value::<ExportTraceServiceRequest>(value) {
                    Ok(req) => {
                        aggregator.ingest_traces(&req);
                    }
                    Err(_) => decode_failures += 1,
                },
                _ => decode_failures += 1,
            }
        }
    }

    eprintln!("replayed {lines_total} lines ({decode_failures} undecodable)");
    match serde_json::to_string_pretty(&aggregator.snapshot()) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("failed to serialize snapshot: {err}");
            ExitCode::FAILURE
        }
    }
}
