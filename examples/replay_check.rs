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
        match replay(BufReader::new(file), &aggregator) {
            Ok((lines, failures)) => {
                lines_total += lines;
                decode_failures += failures;
            }
            Err(err) => {
                eprintln!("failed to read {path}: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    eprintln!("replayed {lines_total} lines ({decode_failures} undecodable)");
    match serde_json::to_string_pretty(&aggregator.snapshot()) {
        Ok(json) => {
            println!("{json}");
            // 一部だけ再生できた累計も確認用に出すが、検証自体は失敗として返す。
            if decode_failures == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(err) => {
            eprintln!("failed to serialize snapshot: {err}");
            ExitCode::FAILURE
        }
    }
}

/// 到着順のまま取り込み、空行を除いた batch 数と decode 失敗数を返す。
fn replay(reader: impl BufRead, aggregator: &Aggregator) -> std::io::Result<(usize, usize)> {
    let mut lines_total = 0;
    let mut decode_failures = 0;
    for line in reader.lines() {
        let line = line?;
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
        let decoded = match kind.as_str() {
            "logs" => serde_json::from_value::<ExportLogsServiceRequest>(value).map(|req| {
                aggregator.ingest_logs(&req);
            }),
            "metrics" => serde_json::from_value::<ExportMetricsServiceRequest>(value).map(|req| {
                aggregator.ingest_metrics(&req);
            }),
            "traces" => serde_json::from_value::<ExportTraceServiceRequest>(value).map(|req| {
                aggregator.ingest_traces(&req);
            }),
            _ => {
                decode_failures += 1;
                continue;
            }
        };
        if decoded.is_err() {
            decode_failures += 1;
        }
    }
    Ok((lines_total, decode_failures))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_counts_invalid_batches_and_continues_with_valid_signals() {
        let input = concat!(
            "\n  \t\n",
            "{broken}\n",
            "{\"kind\":\"logs\",\"resourceLogs\":[]}\n",
            "{\"kind\":\"metrics\",\"resourceMetrics\":[]}\n",
            "{\"kind\":\"traces\",\"resourceSpans\":[]}\n",
            "{\"kind\":\"logs\",\"resourceLogs\":17}\n",
            "{\"kind\":\"unknown\"}\n",
            "{\"kind\":1}\n",
            "{}\nnull\n",
        );
        assert_eq!(
            replay(input.as_bytes(), &Aggregator::new()).unwrap(),
            (9, 6)
        );
    }

    #[test]
    fn replay_preserves_usage_before_and_after_an_invalid_batch() {
        let aggregator = Aggregator::new();
        let batch = serde_json::json!({
            "kind": "logs",
            "resourceLogs": [{
                "resource": {"attributes": [{
                    "key": "service.name", "value": {"stringValue": "claude-code"}
                }]},
                "scopeLogs": [{"logRecords": [{
                    "body": {"stringValue": "claude_code.api_request"},
                    "attributes": [
                        {"key": "model", "value": {"stringValue": "test-model"}},
                        {"key": "input_tokens", "value": {"intValue": "10"}}
                    ]
                }]}]
            }]
        });
        let valid = format!("{batch}\r\n");
        assert_eq!(replay(valid.as_bytes(), &aggregator).unwrap(), (1, 0));
        let mixed = format!("{{broken}}\r\n{valid}");
        assert_eq!(replay(mixed.as_bytes(), &aggregator).unwrap(), (2, 1));
        let snapshot = aggregator.snapshot();
        assert_eq!(snapshot.agents["claude-code"].total.request_count, 2);
        assert_eq!(snapshot.agents["claude-code"].total.input_tokens, 20);
    }

    #[test]
    fn replay_reports_read_failures_instead_of_accepting_partial_input() {
        // 不正な UTF-8 は JSON の decode 失敗以前に読み取りエラーになる。
        let input = b"{\"kind\":\"logs\",\"resourceLogs\":[]}\n\xff\n";
        let error = replay(input.as_slice(), &Aggregator::new()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
