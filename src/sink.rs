use std::fs::OpenOptions as StdOpenOptions;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime};

use time::OffsetDateTime;
use tokio::sync::{RwLock, mpsc, oneshot};

use anyhow::{Context, Result};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use serde::Serialize;

use crate::aggregator::Aggregator;
use crate::cli::{LogSink, Settings};
use crate::format;
use crate::forward::ProxyRouter;
use crate::storage::{AppendFile, DailyWriter};

/// 日次 JSONL のファイル名 `otel-logger.YYYY-MM-DD` の前半。
const ROTATION_PREFIX: &str = "otel-logger";

/// `--pretty-log` の日次ファイル名 `otel-logger.pretty.YYYY-MM-DD.log` の前半と拡張子。
///
/// JSONL の `otel-logger.YYYY-MM-DD` と区別できる名前にする。JSONL 側の厳密な名前判定
/// (日付部分が 10 文字の実在暦日) にも一致しない。
const PRETTY_LOG_PREFIX: &str = "otel-logger.pretty";
const PRETTY_LOG_SUFFIX: &str = "log";

/// poison した mutex から回復して guard を返す。
///
/// poison は「以前どこかで panic した」という記録でしかなく、JSONL の中身自体は
/// 壊れていない。ここで panic を再生産すると `spawn_blocking` が `JoinError` を返し、
/// 以後すべての batch が恒久的に 503 になる。503 は retryable なので exporter は
/// 無限に再送し続け、1 件も保存されないまま欠測が止まらなくなる。
fn lock_recovering<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
    File(StdMutex<AppendFile>),
    Roller(Box<RotatedWriter>),
    #[cfg(test)]
    Fail,
}

/// 日次ローテーション付き writer。
/// 保持は mtime と実在暦日を検証する cleanup に一本化し、日付が変わるたびに実行する。
/// 同じディレクトリにある pretty ファイルも整理し、無効化後に残った古い出力も回収する。
struct RotatedWriter {
    roller: StdMutex<DailyWriter>,
    dir: PathBuf,
    keep_days: u32,
    /// 最後に cleanup を走らせた日 (UTC 基準の Julian day)。
    last_cleanup_day: StdMutex<i32>,
}

impl RotatedWriter {
    /// 日付が変わっていたら古いファイルの掃除を試みる。常駐したままでも保持日数が
    /// 効くようにするためで、cleanup の失敗で JSONL 書き込みは止めない
    /// (欠測させないことを優先する)。
    fn cleanup_if_day_changed(&self) {
        let today = OffsetDateTime::now_utc().date().to_julian_day();
        {
            let mut last = lock_recovering(&self.last_cleanup_day);
            if *last == today {
                return;
            }
            *last = today;
        }
        if let Err(e) = cleanup_old_rotated_logs(&self.dir, self.keep_days) {
            tracing::warn!(
                error = %e,
                dir = %self.dir.display(),
                "failed to clean up rotated log files"
            );
        }
    }

    /// 保持対象の JSONL 日次ファイルを shutdown 時にまとめて fsync する。
    /// ローテーション前のファイルも対象とし、best-effort の pretty は対象にしない。
    fn sync_rotated_files(&self) -> Result<()> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            // ディレクトリごと消えているなら同期するものは無い。
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("read log directory {}", self.dir.display()));
            }
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            // symlink を辿って無関係なファイルを同期しない (cleanup 側と揃える)。
            // `d_type` が `DT_UNKNOWN` の FS (NFS / 一部 FUSE) では `lstat` に落ちるため、
            // readdir との間に並行削除が入ると `NotFound` になる。正常系として飛ばす。
            match entry.file_type() {
                Ok(ft) if ft.is_file() => {}
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| format!("stat log file {}", path.display()));
                }
            }
            let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !is_rotated_log_filename(filename) {
                continue;
            }
            let file = match StdOpenOptions::new().write(true).open(&path) {
                Ok(file) => file,
                // 並行して rotation / cleanup が走った場合は同期対象から外れただけ。
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("reopen rotated JSONL {}", path.display()));
                }
            };
            file.sync_all()
                .with_context(|| format!("fsync rotated JSONL {}", path.display()))?;
        }
        Ok(())
    }
}

impl JsonlWriter {
    fn write_line(&self, line: &[u8]) -> Result<()> {
        // 永続化失敗を上位で 5xx に変換する以上、ACK 時点で改行まで最低限
        // kernel に渡しておく必要がある。fsync まで毎 batch で待つと throughput が落ちるため
        // ここでは書き込み完了まで待ち、disk への確定は graceful shutdown 時の sync_all に任せる。
        match self {
            Self::File(m) => {
                let mut g = lock_recovering(m);
                g.append(line).context("append JSONL line")?;
                Ok(())
            }
            Self::Roller(w) => {
                w.cleanup_if_day_changed();
                let mut g = lock_recovering(&w.roller);
                g.write_all(line).context("append JSONL line (rotated)")?;
                g.flush().context("flush JSONL line (rotated)")?;
                Ok(())
            }
            #[cfg(test)]
            Self::Fail => anyhow::bail!("forced JSONL failure"),
        }
    }

    fn flush(&self) -> Result<()> {
        match self {
            Self::File(m) => {
                let g = lock_recovering(m);
                g.sync_all().context("fsync JSONL file")
            }
            Self::Roller(w) => {
                let mut g = lock_recovering(&w.roller);
                g.flush().context("flush JSONL roller")?;
                // lock を持ったまま fsync する。先に解放すると、並行 batch の書き込みが
                // fsync の後ろに挟まって同期されないまま残る。
                w.sync_rotated_files()
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

/// 人が読める出力キューの深さ (batch 数)。出力先ごとに持つ。
///
/// 読み手が遅い場合 (`| less` でスクロールを止める、launchd / systemd / Docker log
/// driver の読み出しが一時的に詰まる、pretty-log のディスクが遅い等) でも、OTLP の ACK を
/// 止めないための緩衝。溢れた分は捨てる。
const PRETTY_QUEUE_CAPACITY: usize = 256;

/// 出力先ごとの queue に積める rendered 文字列の合計バイト数。
///
/// 件数の上限だけでは memory の上限にならない。1 リクエストは `OTLP_MAX_REQUEST_BYTES`
/// (32MiB) まで受け付けるため、大きな batch を展開した文字列が 256 件溜まると、
/// 出力先 1 つで数 GB に届きうる。超えた分は件数の超過と同じく捨てる
/// (人が読める出力はベストエフォートで、JSONL 側に payload は残っている)。
const PRETTY_QUEUE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// `flush()` が pretty writer の追いつきを待つ上限。全出力先で 1 つの期限を共有する。
/// 出力先が完全に詰まっている場合に shutdown を人質に取られないようにする。
const PRETTY_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// 書き込みの失敗が続いている間に、状況を再通知する間隔。
const PRETTY_FAILURE_REPORT_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// 最後の失敗からこの時間が過ぎた後の成功で、はじめて「回復した」とみなす。
///
/// 空き容量がわずかなディスクのように、小さい batch は書けて大きい batch は落ちる状態では
/// 成功と失敗が交互に来る。成功 1 回で回復とみなすと、batch ごとに「失敗し始めた」
/// 「回復した」を出して stderr を伸ばすうえ、直後の失敗を間引くと「回復した」が
/// 最後の通知のまま実際には失敗し続ける、という誤解を招く。
const PRETTY_RECOVERY_QUIET_PERIOD: Duration = Duration::from_secs(60);

/// 書き込み失敗の通知を、出力先ごとに間引くための状態。
///
/// 失敗のたびに error を出すと、ディスクが埋まっている間じゅう batch ごとに stderr が
/// 伸びる (stderr もローテーションされない)。かといって最初の 1 回だけでは、数日続いて
/// 回復しない障害の手掛かりが最初の 1 行しか残らない。そこで、失敗し始めた時に 1 回、
/// 続いている間は `PRETTY_FAILURE_REPORT_INTERVAL` ごとに 1 回、回復した時に 1 回だけ
/// 知らせる。失敗が `PRETTY_RECOVERY_QUIET_PERIOD` より短い間隔で繰り返す間は、成功が
/// 挟まっても 1 つの障害として扱う。
#[derive(Debug, Default)]
struct WriteHealth {
    /// 失敗が続いている区間。`None` なら障害は起きていない。
    failing: Option<FailureStreak>,
}

/// 失敗が続いている 1 区間。
#[derive(Debug)]
struct FailureStreak {
    since: Instant,
    failures: u64,
    /// この区間で最後に失敗した時刻。回復の判定に使う。
    last_failure: Instant,
    /// この区間で最後に通知した時刻。
    last_report: Instant,
}

/// `WriteHealth` が求める通知。
#[derive(Debug, PartialEq, Eq)]
enum HealthReport {
    /// 何も出さない。
    Silent,
    /// 失敗し始めた。
    Started,
    /// 失敗が続いている。
    Ongoing { failures: u64, elapsed: Duration },
    /// 回復した。
    Recovered { failures: u64, elapsed: Duration },
}

impl WriteHealth {
    fn on_failure(&mut self, now: Instant) -> HealthReport {
        let Some(streak) = self.failing.as_mut() else {
            self.failing = Some(FailureStreak {
                since: now,
                failures: 1,
                last_failure: now,
                last_report: now,
            });
            return HealthReport::Started;
        };
        streak.failures = streak.failures.saturating_add(1);
        streak.last_failure = now;
        if now.saturating_duration_since(streak.last_report) < PRETTY_FAILURE_REPORT_INTERVAL {
            return HealthReport::Silent;
        }
        streak.last_report = now;
        HealthReport::Ongoing {
            failures: streak.failures,
            elapsed: now.saturating_duration_since(streak.since),
        }
    }

    fn on_success(&mut self, now: Instant) -> HealthReport {
        match self.failing.as_ref() {
            Some(streak)
                if now.saturating_duration_since(streak.last_failure)
                    >= PRETTY_RECOVERY_QUIET_PERIOD =>
            {
                let report = HealthReport::Recovered {
                    failures: streak.failures,
                    elapsed: now.saturating_duration_since(streak.since),
                };
                self.failing = None;
                report
            }
            // 障害が無い、または直前まで失敗していた (まだ回復とみなさない)。
            _ => HealthReport::Silent,
        }
    }

    /// 障害が続いていれば、失敗件数・障害の経過時間・最後の失敗からの経過時間。
    fn unresolved(&self, now: Instant) -> Option<(u64, Duration, Duration)> {
        self.failing.as_ref().map(|streak| {
            (
                streak.failures,
                now.saturating_duration_since(streak.since),
                now.saturating_duration_since(streak.last_failure),
            )
        })
    }
}

/// pretty writer 1 本分の状態。
#[derive(Debug, Default)]
struct PrettyWriterState {
    /// stdout の読み手が消えた (EPIPE)。以後この出力先には書かない。
    ///
    /// 読み手が消えた後 (`otel-logger | head` の終了、pager の終了など) も batch ごとに
    /// 書き込みを試すと、失敗のたびに stderr へ error を積み上げることになる。
    /// EPIPE は telemetry の欠落ではないので、JSONL 永続化・累計集計・proxy 転送は続け、
    /// OTLP 側のエラーにも変換しない。
    broken_pipe: bool,
    health: WriteHealth,
}

impl PrettyWriterState {
    /// この batch を書くべきか。stdout の pipe が閉じた後は書かない。
    fn should_write(&self) -> bool {
        !self.broken_pipe
    }

    /// 書き込み結果を状態に反映し、必要な診断ログを出す。返り値はテストで検証する。
    ///
    /// EPIPE で止めるのは stdout だけ。pretty-log のファイルが EPIPE を返すことは
    /// 通常ないが、返しても一時的な失敗として扱い、恒久停止させない。それ以外の
    /// I/O error も一時的な可能性があるため、止めずに次の batch で書き直す。
    fn record_result(
        &mut self,
        target: &PrettyTarget,
        result: io::Result<()>,
        now: Instant,
    ) -> HealthReport {
        match result {
            Ok(()) => {
                let report = self.health.on_success(now);
                log_health(target.name(), &report, None);
                report
            }
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe && target.is_stdout() => {
                self.broken_pipe = true;
                tracing::warn!(
                    "stdout closed (broken pipe); disabling human-readable output on stdout. \
                     JSONL persistence, usage aggregation, proxy forwarding and `--pretty-log` \
                     continue. / stdout が閉じられた (broken pipe) ため、stdout への人が読める\
                     出力を停止します。JSONL 永続化・累計集計・proxy 転送・`--pretty-log` は\
                     継続します。"
                );
                HealthReport::Silent
            }
            Err(e) => {
                let report = self.health.on_failure(now);
                let error = anyhow::Error::new(e).context(format!("write to {}", target.name()));
                log_health(target.name(), &report, Some(&error));
                report
            }
        }
    }

    /// shutdown 時点でも障害が回復していなければ、最後の状況を 1 回だけ記録する。
    fn report_unresolved_at_shutdown(&self, target: &PrettyTarget, now: Instant) {
        if let Some((failures, elapsed, since_last_failure)) = self.health.unresolved(now) {
            tracing::warn!(
                destination = target.name(),
                failed_batches = failures,
                failing_for_secs = elapsed.as_secs(),
                last_failure_secs_ago = since_last_failure.as_secs(),
                "human-readable output had not recovered from write failures at shutdown"
            );
        }
    }
}

/// `WriteHealth` の通知を tracing へ出す。
///
/// 原因は `{:#}` で chain ごと文字列にしてから Debug で記録する。anyhow の Display は
/// 最外の context しか出さず、実ログでは `error=write to stdout` だけで原因が
/// 分からなかった。Debug にするのは、パスなどに紛れた制御文字をそのまま端末へ
/// 出さないため。
fn log_health(destination: &str, report: &HealthReport, error: Option<&anyhow::Error>) {
    let error = error.map(|e| format!("{e:#}"));
    match report {
        HealthReport::Silent => {}
        HealthReport::Started => tracing::error!(
            destination,
            error = ?error,
            "failed to write human-readable output; repeated failures are reported every \
             {} minutes until it recovers. JSONL persistence is unaffected. \
             / 人が読める出力の書き込みに失敗しました。回復するまで、続く失敗は {} 分ごとに\
             まとめて報告します。JSONL 永続化には影響しません。",
            PRETTY_FAILURE_REPORT_INTERVAL.as_secs() / 60,
            PRETTY_FAILURE_REPORT_INTERVAL.as_secs() / 60,
        ),
        HealthReport::Ongoing { failures, elapsed } => tracing::warn!(
            destination,
            error = ?error,
            failed_batches = failures,
            failing_for_secs = elapsed.as_secs(),
            "human-readable output is still failing"
        ),
        HealthReport::Recovered { failures, elapsed } => tracing::info!(
            destination,
            failed_batches = failures,
            failing_for_secs = elapsed.as_secs(),
            "human-readable output recovered"
        ),
    }
}

/// 永続化失敗は exporter に再送させるが、サーバー側にも原因と回復状況を残す。
fn log_jsonl_health(report: &HealthReport, error: Option<&anyhow::Error>) {
    let error = error.map(|error| format!("{error:#}"));
    match report {
        HealthReport::Silent => {}
        HealthReport::Started => tracing::error!(error = ?error,
            "JSONL persistence failed; rejecting batches with retryable status"),
        HealthReport::Ongoing { failures, elapsed } => tracing::warn!(error = ?error,
            failed_batches = failures, failing_for_secs = elapsed.as_secs(),
            "JSONL persistence is still failing"),
        HealthReport::Recovered { failures, elapsed } => tracing::info!(
            failed_batches = failures,
            failing_for_secs = elapsed.as_secs(),
            "JSONL persistence recovered"
        ),
    }
}

/// 人が読める出力の書き出し先。
#[derive(Clone)]
enum PrettyTarget {
    /// プロセスの stdout (fd 1)。所有者は親プロセスなので、otel-logger はローテーションしない。
    Stdout,
    /// `--pretty-log`: `log-dir` 内の日次ファイル (`otel-logger.pretty.YYYY-MM-DD.log`)。
    /// otel-logger 自身が開いたファイルなので、保存先も保持期間も分かっている。
    File(Arc<StdMutex<DailyWriter>>),
    /// テスト用: 書いた内容を溜める。
    #[cfg(test)]
    Capture(Arc<StdMutex<Vec<u8>>>),
    /// テスト用: gate が開くまで書き込みを止める (読み手が止まった stdout の代わり)。
    #[cfg(test)]
    Stall(Arc<(StdMutex<bool>, std::sync::Condvar)>),
}

impl PrettyTarget {
    /// 診断ログに載せる出力先の名前。
    fn name(&self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::File(_) => "pretty-log",
            #[cfg(test)]
            Self::Capture(_) | Self::Stall(_) => "test",
        }
    }

    fn is_stdout(&self) -> bool {
        matches!(self, Self::Stdout)
    }

    /// 1 batch 分を書いて flush する。blocking I/O なので `spawn_blocking` から呼ぶ。
    fn write_blocking(&self, rendered: &str, summary: Option<&str>) -> io::Result<()> {
        match self {
            Self::Stdout => {
                let stdout = io::stdout();
                let mut handle = stdout.lock();
                write_pretty_to(&mut handle, rendered, summary)
            }
            Self::File(roller) => {
                let mut roller = lock_recovering(roller);
                write_pretty_to(&mut *roller, rendered, summary)
            }
            #[cfg(test)]
            Self::Capture(buf) => write_pretty_to(&mut *lock_recovering(buf), rendered, summary),
            #[cfg(test)]
            Self::Stall(gate) => {
                let (open, opened) = &**gate;
                let mut open = lock_recovering(open);
                while !*open {
                    open = opened.wait(open).unwrap_or_else(|p| p.into_inner());
                }
                Ok(())
            }
        }
    }
}

/// 人が読める出力の 1 単位。
///
/// 書き込みを OTLP の ACK 経路から切り離すために、出力先ごとの専用 writer task へ渡す。
/// ACK 経路で待つと、読み手が遅いだけで全 batch の応答が止まり、exporter の timeout →
/// retry によって同じ payload が JSONL へ二重に書かれ、累計も二重計上される。
struct PrettyJob {
    /// 出力先の色設定で render 済みの文字列。色設定が同じ出力先どうしで共有する。
    rendered: Arc<str>,
    want_summary: bool,
    /// `flush()` が使う同期点。channel は FIFO なので、この job が処理された時点で
    /// 先行する job はすべて出力済み。
    sync: Option<oneshot::Sender<()>>,
}

/// 人が読める出力先 1 つ分の受け渡し口。
///
/// 出力先ごとに queue・writer task・捨てた件数・失敗の状態を分ける。1 本の writer で
/// 両方へ書くと、止まった stdout が pretty-log まで止め、逆も同様になる。同じ record を
/// 独立に書き出すので、捨てた batch や summary の時点は出力先ごとに異なりうる。
struct PrettyOutput {
    /// 診断ログに載せる出力先の名前。
    name: &'static str,
    /// この出力先向けに色付きで render するか。
    color: bool,
    tx: mpsc::Sender<PrettyJob>,
    /// queue に積まれている (処理中を含む) rendered 文字列の合計バイト数。
    queued_bytes: Arc<AtomicUsize>,
    /// `queued_bytes` の上限。
    max_queued_bytes: usize,
    /// 追いつかずに捨てた batch 数。
    dropped: AtomicU64,
}

impl PrettyOutput {
    /// 出力先の writer task を起動し、受け渡し口を返す。
    fn spawn(target: PrettyTarget, color: bool, aggregator: Arc<Aggregator>) -> Self {
        let (tx, rx) = mpsc::channel::<PrettyJob>(PRETTY_QUEUE_CAPACITY);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let name = target.name();
        tokio::spawn(run_pretty_writer(
            rx,
            target,
            Arc::clone(&queued_bytes),
            aggregator,
            color,
        ));
        Self {
            name,
            color,
            tx,
            queued_bytes,
            max_queued_bytes: PRETTY_QUEUE_MAX_BYTES,
            dropped: AtomicU64::new(0),
        }
    }

    /// 1 batch 分を queue に積む。溢れたら待たずに捨てる。
    fn enqueue(&self, rendered: Arc<str>, want_summary: bool) {
        let len = rendered.len();
        let queued = self.queued_bytes.fetch_add(len, AtomicOrdering::Relaxed);
        // queue が空でも上限を超える batch は通さない。制御文字を多く含む payload は escape で
        // 数倍に膨らむ (NUL 1 byte → `\u{0000}` 8 byte) ため、32MiB の request 1 つが上限を
        // 大きく超えうる。出力先が止まっている間それを抱え続けると、上限の意味が無くなる。
        let over_budget = queued.saturating_add(len) > self.max_queued_bytes;
        let job = PrettyJob {
            rendered,
            want_summary,
            sync: None,
        };
        if over_budget || self.tx.try_send(job).is_err() {
            // queue が詰まっている = 読み手が遅い、または writer が停止した。
            // 人が読める出力はベストエフォートなので捨てる。JSONL 永続化・累計集計・
            // proxy 転送は既に完了しているため、telemetry の欠測にはならない。
            self.queued_bytes.fetch_sub(len, AtomicOrdering::Relaxed);
            let dropped = self.dropped.fetch_add(1, AtomicOrdering::Relaxed) + 1;
            if dropped.is_power_of_two() {
                tracing::warn!(
                    destination = self.name,
                    dropped_total = dropped,
                    "human-readable output writer is behind; dropping output. \
                     JSONL persistence, usage aggregation and proxy forwarding are unaffected. \
                     / 人が読める出力の書き出しが追いつかないため、出力を捨てています。\
                     JSONL 永続化・累計集計・proxy 転送には影響しません。"
                );
            }
        }
    }

    /// queue 済みの出力を writer が書き終えるまで待つ。
    ///
    /// channel は FIFO なので、同期 job が処理された時点で末尾の summary まで出力済み。
    /// 同期 job の enqueue と完了待ちの両方を `deadline` で打ち切る (出力先が完全に
    /// 詰まっていても shutdown を人質に取らせない)。
    async fn drain(&self, deadline: tokio::time::Instant) {
        let (sync_tx, sync_rx) = oneshot::channel();
        let job = PrettyJob {
            rendered: Arc::from(""),
            want_summary: false,
            sync: Some(sync_tx),
        };
        let drained = tokio::time::timeout_at(deadline, async move {
            self.tx.send(job).await.ok()?;
            sync_rx.await.ok()
        })
        .await;
        if !matches!(drained, Ok(Some(()))) {
            tracing::warn!(
                destination = self.name,
                "human-readable output did not drain before shutdown; some output was lost"
            );
        }
    }
}

/// 出力先 1 つ分の人が読める出力を直列に書き出す専用 task。
///
/// writer が出力先ごとに 1 本なので record 同士が interleave せず、`stdout_lock` のような
/// 粗い排他も要らない。累計 snapshot も「書く直前」に取るため、出力先ごとに
/// snapshot 順と出力順が一致し、出力上で累計が逆行しない。
async fn run_pretty_writer(
    mut rx: mpsc::Receiver<PrettyJob>,
    target: PrettyTarget,
    queued_bytes: Arc<AtomicUsize>,
    aggregator: Arc<Aggregator>,
    color: bool,
) {
    let mut state = PrettyWriterState::default();
    while let Some(job) = rx.recv().await {
        if state.should_write() && !job.rendered.is_empty() {
            let summary = job
                .want_summary
                .then(|| format::render_summary(&aggregator.snapshot(), color));
            let writer = target.clone();
            let rendered = Arc::clone(&job.rendered);
            let result = tokio::task::spawn_blocking(move || {
                writer.write_blocking(&rendered, summary.as_deref())
            })
            .await
            // writer の panic も書き込みの失敗として数え、次の batch で書き直す。
            .unwrap_or_else(|e| Err(io::Error::other(e)));
            state.record_result(&target, result, Instant::now());
        }
        queued_bytes.fetch_sub(job.rendered.len(), AtomicOrdering::Relaxed);
        if let Some(sync) = job.sync {
            state.report_unresolved_at_shutdown(&target, Instant::now());
            let _ = sync.send(());
        }
    }
}

/// pretty 出力と (必要なら) 累計サマリーを 1 つの writer へ書き出す。
/// 出力先の handle を直接触らないので、error 経路を単体テストできる。
fn write_pretty_to<W: io::Write>(
    writer: &mut W,
    rendered: &str,
    summary: Option<&str>,
) -> io::Result<()> {
    writer.write_all(rendered.as_bytes())?;
    if let Some(summary) = summary {
        writer.write_all(summary.as_bytes())?;
    }
    writer.flush()
}

struct SinkInner {
    summary_enabled: bool,
    /// shutdown は受理済み処理の完了を待ってから閉じ、新しい受信を拒否する。
    admission: Arc<RwLock<bool>>,
    jsonl_health: StdMutex<WriteHealth>,
    file: Option<JsonlWriter>,
    /// stdout への人が読める出力 (`--no-stdout` なら `None`)。
    stdout: Option<PrettyOutput>,
    /// `--pretty-log` の日次ファイルへの人が読める出力 (無効なら `None`)。
    pretty_log: Option<PrettyOutput>,
    aggregator: Arc<Aggregator>,
    /// OTLP proxy 転送ルーター (未設定なら `None`)。JSONL 永続化が成功した後で
    /// service.name で振り分けて `try_send` する。
    proxy: Option<ProxyRouter>,
}

impl SinkInner {
    /// 有効な人が読める出力先。
    fn pretty_outputs(&self) -> impl Iterator<Item = &PrettyOutput> {
        self.stdout.iter().chain(self.pretty_log.iter())
    }
}

impl Sink {
    pub async fn from_settings(settings: &Settings) -> Result<Self> {
        Self::from_settings_with_proxy(settings, None).await
    }

    /// Sink を作りつつ、既に spawn 済みの proxy router を装着する。
    /// server::run が worker を spawn した後、その router を渡す。
    pub async fn from_settings_with_proxy(
        settings: &Settings,
        proxy: Option<ProxyRouter>,
    ) -> Result<Self> {
        // JSONL を先に開く。directory sink の起動時 cleanup を、pretty-log の writer が
        // 当日のファイルを作るより前に済ませるため。
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
                Some(JsonlWriter::Roller(Box::new(roller)))
            }
        };

        let aggregator = Arc::new(Aggregator::new());
        let stdout = (!settings.no_stdout).then(|| {
            PrettyOutput::spawn(
                PrettyTarget::Stdout,
                settings.color.enabled_for_stdout(),
                Arc::clone(&aggregator),
            )
        });
        let pretty_log = match settings.pretty_log_dir.as_ref() {
            None => None,
            Some(dir) => {
                let dir = dir.clone();
                tracing::info!(
                    dir = %dir.display(),
                    "pretty log: writing human-readable output as daily files in directory"
                );
                let roller = tokio::task::spawn_blocking(move || open_pretty_log_sync(&dir))
                    .await
                    .context("join open_pretty_log task")??;
                // ファイルは検索・共有・長期閲覧用なので、`color = "always"` でも色を付けない。
                // ANSI を後から取り除くのではなく、最初から色なしで render する。
                Some(PrettyOutput::spawn(
                    PrettyTarget::File(Arc::new(StdMutex::new(roller))),
                    false,
                    Arc::clone(&aggregator),
                ))
            }
        };

        Ok(Self {
            inner: Arc::new(SinkInner {
                admission: Arc::new(RwLock::new(true)),
                jsonl_health: StdMutex::new(WriteHealth::default()),
                summary_enabled: settings.summary,
                file,
                stdout,
                pretty_log,
                aggregator,
                proxy,
            }),
        })
    }

    /// proxy 転送用 Router への参照。`/stats` 出力に metrics を含めるために使う。
    pub fn proxy(&self) -> Option<&ProxyRouter> {
        self.inner.proxy.as_ref()
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
    /// 人が読める出力 (stdout / pretty-log) はベストエフォートで、書き込みに失敗しても
    /// tracing にだけ記録する。
    pub async fn record(&self, record: TelemetryRecord) -> Result<()> {
        let admission = Arc::clone(&self.inner.admission).read_owned().await;
        anyhow::ensure!(*admission, "JSONL sink is shutting down");
        let sink = self.clone();
        // handler が cancel されても、blocking I/O の完了まで受理中の guard を保持する。
        // listener を abort した後に接続 task が残っても、最後の fsync を追い越せない。
        tokio::spawn(async move {
            let _admission = admission;
            sink.record_admitted(record).await
        })
        .await
        .context("join admitted record task")?
    }

    async fn record_admitted(&self, record: TelemetryRecord) -> Result<()> {
        if self.inner.file.is_some() {
            let result = self
                .write_jsonl(&record)
                .await
                .with_context(|| format!("persist {} batch to JSONL", record.kind()));
            let mut health = lock_recovering(&self.inner.jsonl_health);
            let report = match &result {
                Ok(()) => health.on_success(Instant::now()),
                Err(_) => health.on_failure(Instant::now()),
            };
            log_jsonl_health(&report, result.as_ref().err());
            result?;
        }

        // JSONL 永続化が成功した batch だけを集計する。失敗時は exporter が retry するため、
        // 先に集計すると同じ payload を二重計上してしまう。
        let samples_present = match &record {
            TelemetryRecord::Logs(req) => self.inner.aggregator.ingest_logs(req) > 0,
            TelemetryRecord::Traces(req) => self.inner.aggregator.ingest_traces(req) > 0,
            TelemetryRecord::Metrics(req) => self.inner.aggregator.ingest_metrics(req) > 0,
        };
        let want_summary = samples_present && self.inner.summary_enabled;

        // 人が読める出力は ACK 経路から切り離す (fire-and-forget)。ここで待つと、
        // 読み手が遅いだけで全 batch の応答が止まり、exporter の timeout → retry で
        // 同じ payload が JSONL へ二重に書かれ、累計も二重計上される。
        self.queue_pretty(&record, want_summary);

        // JSONL 永続化と集計が成功した batch だけを proxy に流す。
        // ここでは fire-and-forget (bounded channel の try_send)。
        // 転送失敗は route worker 側で retry する。
        if let Some(router) = self.inner.proxy.as_ref() {
            router.notify(&record);
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

    /// 人が読める出力を出力先ごとの writer task へ渡す。累計 snapshot は writer 側で
    /// 取るので、ここでは render だけ行う (CPU バウンドでブロックしない)。
    fn queue_pretty(&self, record: &TelemetryRecord, want_summary: bool) {
        // 色設定ごとに 1 回だけ render して出力先の間で共有する。stdout を色なしで
        // 出している (端末でない、`NO_COLOR`) なら、pretty-log と同じ文字列を使い回せる。
        let mut rendered: [Option<Arc<str>>; 2] = [None, None];
        for output in self.inner.pretty_outputs() {
            let text = rendered[usize::from(output.color)]
                .get_or_insert_with(|| Arc::from(format::render(record, output.color)));
            output.enqueue(Arc::clone(text), want_summary);
        }
    }

    /// 新しい受信を拒否し、受理済み batch の完了後に最終同期する。
    pub async fn shutdown(&self) -> Result<()> {
        let mut admission = self.inner.admission.write().await;
        *admission = false;
        self.flush().await
    }

    /// buffer 済み書き込みを flush する。SIGTERM でも末尾 batch を失わないよう、
    /// graceful shutdown から呼び出す。
    ///
    /// 人が読める出力は writer が batch ごとに flush しているので、ここでは queue の
    /// 書き出しを待つだけにする。以前はこの後に `io::stdout().flush()` を待っていたが、
    /// writer が stdout の write(2) で止まっていると stdout の lock が解けず、
    /// drain の上限を過ぎても shutdown が無期限に止まっていた。
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
        self.drain_pretty_until(tokio::time::Instant::now() + PRETTY_FLUSH_TIMEOUT)
            .await;
        Ok(())
    }

    /// 全出力先の writer が queue 済みの出力を書き終えるのを、共有の期限まで並行に待つ。
    ///
    /// 出力先ごとに順番に待つと、止まった stdout が期限を使い切り、後ろの pretty-log は
    /// 1 件も書き出せないまま打ち切られる。期限を過ぎた分は捨てる (人が読める出力は
    /// ベストエフォートで、JSONL 側に payload は残っている)。
    async fn drain_pretty_until(&self, deadline: tokio::time::Instant) {
        tokio::join!(
            drain_pretty_output(self.inner.stdout.as_ref(), deadline),
            drain_pretty_output(self.inner.pretty_log.as_ref(), deadline)
        );
    }
}

/// 出力先が有効なら、その writer の drain を `deadline` まで待つ。
async fn drain_pretty_output(output: Option<&PrettyOutput>, deadline: tokio::time::Instant) {
    if let Some(output) = output {
        output.drain(deadline).await;
    }
}

fn open_log_file_sync(path: &Path) -> Result<AppendFile> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        crate::path::create_private_dir(parent)
            .with_context(|| format!("create parent directory of {}", path.display()))?;
    }
    AppendFile::open(path).with_context(|| format!("open log file {}", path.display()))
}

fn open_rotated_sync(dir: &Path, keep_days: u32) -> Result<RotatedWriter> {
    crate::path::create_private_dir(dir)
        .with_context(|| format!("create log directory {}", dir.display()))?;
    // `--log-keep-days 0` が来た時に cutoff = now() となり全 rotated file が
    // 削除されてしまうのを防ぐため、最低 1 日は保持する。
    let max_keep = keep_days.max(1);
    cleanup_old_rotated_logs(dir, max_keep)
        .with_context(|| format!("cleanup old log files in {}", dir.display()))?;
    let appender =
        DailyWriter::new(dir, ROTATION_PREFIX, None).context("open daily JSONL writer")?;
    Ok(RotatedWriter {
        roller: StdMutex::new(appender),
        dir: dir.to_path_buf(),
        keep_days: max_keep,
        last_cleanup_day: StdMutex::new(OffsetDateTime::now_utc().date().to_julian_day()),
    })
}

/// JSONL と同じローカル暦日の境界で切り替える。保持管理は JSONL 側がまとめて行う。
fn open_pretty_log_sync(dir: &Path) -> Result<DailyWriter> {
    crate::path::create_private_dir(dir)
        .with_context(|| format!("create log directory {}", dir.display()))?;
    DailyWriter::new(dir, PRETTY_LOG_PREFIX, Some(PRETTY_LOG_SUFFIX))
        .context("open daily pretty writer")
}

/// mtime 基準で `keep_days` より古い otel-logger の日次ファイル (JSONL と `--pretty-log`) を
/// 削除する。
///
/// `--pretty-log` が無効でも pretty の日次ファイルを対象にする。無効に戻した後に
/// 残ったファイルを整理する主体が他に無いため。
fn cleanup_old_rotated_logs(dir: &Path, keep_days: u32) -> Result<()> {
    // `exists()` は権限エラーや symlink loop も `false` に潰すため、保持期間が
    // 実際には効いていないのに起動が成功してしまう。`try_exists()` で区別する。
    if !dir
        .try_exists()
        .with_context(|| format!("inspect log directory {}", dir.display()))?
    {
        return Ok(());
    }
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(u64::from(keep_days) * 24 * 60 * 60))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // LogRoller が生成する通常ファイルだけを対象にする。`Path::is_file()` は
        // symlink を辿るため、同じ名前の symlink まで誤って削除対象にしてしまう。
        // 下の `entry.metadata()` と同じく、並行削除による `NotFound` は正常系として
        // 飛ばす。ここで `?` すると cleanup ごと失敗し、`open_rotated_sync` が
        // 起動時に Err を返して receiver が上がらない (= 全件欠測) 。
        match entry.file_type() {
            Ok(ft) if ft.is_file() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("stat log file {}", path.display())),
        }
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if daily_log_kind(filename).is_none() {
            continue;
        }
        // 失敗を握り潰すと、保持期間が実際には効いていなくても起動が成功してしまい、
        // ディスクを使い切った時点で JSONL 永続化そのものが止まる。並行削除で
        // 消えていた場合だけを正常系として扱い、それ以外は呼び出し元へ伝播する。
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("stat log file {}", path.display())),
        };
        // mtime を取得できない環境では保持判定ができないので、消さずに残す。
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified >= cutoff {
            continue;
        }
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(e).with_context(|| format!("remove old log file {}", path.display()));
        }
    }
    Ok(())
}

/// `log-dir` に otel-logger が書く日次ファイルの種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DailyLogKind {
    /// 受信 payload の lossless な JSONL (`otel-logger.YYYY-MM-DD`)。
    Jsonl,
    /// `--pretty-log` の人が読める出力 (`otel-logger.pretty.YYYY-MM-DD.log`)。
    Pretty,
}

/// ファイル名が otel-logger の日次ファイルなら、その種類を返す。
///
/// 受け付けるのは上の 2 形式だけで、日付は実在する暦日に限る。`otel-logger.pid` や
/// `otel-logger.stderr.log` のような同 prefix の別用途ファイル、拡張子違い
/// (`otel-logger.pretty.YYYY-MM-DD` / `otel-logger.pretty.YYYY-MM-DD.log.gz`) は `None`。
/// bool で「どちらかの日次ファイルか」だけを返すと、shutdown の fsync のような JSONL
/// 専用の処理まで pretty を巻き込むため、種類を返す。
fn daily_log_kind(filename: &str) -> Option<DailyLogKind> {
    if let Some(date) = filename
        .strip_prefix(PRETTY_LOG_PREFIX)
        .and_then(|rest| rest.strip_prefix('.'))
        .and_then(|rest| rest.strip_suffix(PRETTY_LOG_SUFFIX))
        .and_then(|rest| rest.strip_suffix('.'))
    {
        return is_calendar_date(date).then_some(DailyLogKind::Pretty);
    }
    let date = filename
        .strip_prefix(ROTATION_PREFIX)
        .and_then(|rest| rest.strip_prefix('.'))?;
    is_calendar_date(date).then_some(DailyLogKind::Jsonl)
}

/// JSONL の日次ファイル名か。
fn is_rotated_log_filename(filename: &str) -> bool {
    daily_log_kind(filename) == Some(DailyLogKind::Jsonl)
}

/// `YYYY-MM-DD` 形式で、かつ実在する暦日か。
fn is_calendar_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    if bytes.len() != 10
        || !bytes[0..4].iter().all(u8::is_ascii_digit)
        || bytes[4] != b'-'
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || bytes[7] != b'-'
        || !bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        return false;
    }

    // 桁だけを見ると `2026-99-99` のように LogRoller が生成しない名前まで削除対象に
    // なる。年月日を暦として検証し、実在する日次ローテーション名だけを許可する。
    let Ok(year) = date[0..4].parse::<i32>() else {
        return false;
    };
    let Ok(month) = date[5..7].parse::<u8>() else {
        return false;
    };
    let Ok(day) = date[8..10].parse::<u8>() else {
        return false;
    };
    let Ok(month) = time::Month::try_from(month) else {
        return false;
    };
    time::Date::from_calendar_date(year, month, day).is_ok()
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
                admission: Arc::new(RwLock::new(true)),
                jsonl_health: StdMutex::new(WriteHealth::default()),
                summary_enabled: true,
                file: Some(JsonlWriter::Fail),
                stdout: None,
                pretty_log: None,
                aggregator: Arc::new(Aggregator::new()),
                proxy: None,
            }),
        }
    }

    /// JSONL を持たず、人が読める出力先だけを差し替えた sink。
    fn sink_with_pretty_targets(
        stdout: Option<PrettyTarget>,
        pretty_log: Option<PrettyTarget>,
        summary_enabled: bool,
    ) -> Sink {
        let aggregator = Arc::new(Aggregator::new());
        let spawn =
            |target: PrettyTarget| PrettyOutput::spawn(target, false, Arc::clone(&aggregator));
        Sink {
            inner: Arc::new(SinkInner {
                admission: Arc::new(RwLock::new(true)),
                jsonl_health: StdMutex::new(WriteHealth::default()),
                summary_enabled,
                file: None,
                stdout: stdout.map(spawn),
                pretty_log: pretty_log.map(spawn),
                aggregator: Arc::clone(&aggregator),
                proxy: None,
            }),
        }
    }

    fn open_gate(gate: &(StdMutex<bool>, std::sync::Condvar)) {
        *lock_recovering(&gate.0) = true;
        gate.1.notify_all();
    }

    async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(
                Instant::now() < deadline,
                "5 秒以内に満たされなかった: {what}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn files_of_kind(dir: &Path, kind: DailyLogKind) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(daily_log_kind)
                    == Some(kind)
            })
            .collect()
    }

    fn touch_mtime(path: &Path, when: SystemTime) {
        // `File::set_times` + `FileTimes::set_modified` は cross-platform で安定している。
        let times = std::fs::FileTimes::new().set_modified(when);
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(times).unwrap();
    }

    /// 回帰テスト: `--log-keep-days 0` で起動した場合に rotated JSONL を全削除しないこと。
    /// `open_rotated_sync` が cleanup と log roller の両方で `max(1)` を適用するため、
    /// cleanup 側も最低 1 日は保持して新規 file が即削除されない挙動を担保する。
    #[test]
    fn open_rotated_sync_with_zero_keep_days_retains_recent_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let recent = dir.path().join("otel-logger.2099-01-01");
        std::fs::write(&recent, "recent").unwrap();
        // mtime をほぼ現在にしておけば、`keep_days.max(1)` 経由で残るはず。
        touch_mtime(&recent, SystemTime::now() - Duration::from_secs(60));

        let roller = open_rotated_sync(dir.path(), 0).expect("roller を構築できる");
        drop(roller);

        assert!(
            recent.exists(),
            "keep_days=0 でも mtime が新しい rotated file は残る"
        );
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
        let invalid_date = dir.path().join("otel-logger.2020-99-99");
        let old_pretty = dir.path().join("otel-logger.pretty.2020-01-01.log");
        let recent_pretty = dir.path().join("otel-logger.pretty.2099-01-01.log");
        let pretty_without_suffix = dir.path().join("otel-logger.pretty.2020-01-01");
        let pretty_compressed = dir.path().join("otel-logger.pretty.2020-01-01.log.gz");
        let pretty_invalid_date = dir.path().join("otel-logger.pretty.2020-99-99.log");
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&recent, "recent").unwrap();
        std::fs::write(&pid, "12345").unwrap();
        std::fs::write(&stderr, "stderr").unwrap();
        std::fs::write(&jsonl, "jsonl").unwrap();
        std::fs::write(&unrelated, "other").unwrap();
        std::fs::write(&invalid_date, "invalid date").unwrap();
        for path in [
            &old_pretty,
            &recent_pretty,
            &pretty_without_suffix,
            &pretty_compressed,
            &pretty_invalid_date,
        ] {
            std::fs::write(path, "pretty").unwrap();
        }

        let three_days_ago = SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        for path in [
            &old,
            &pid,
            &stderr,
            &jsonl,
            &unrelated,
            &invalid_date,
            &old_pretty,
            &pretty_without_suffix,
            &pretty_compressed,
            &pretty_invalid_date,
        ] {
            touch_mtime(path, three_days_ago);
        }

        cleanup_old_rotated_logs(dir.path(), 1).unwrap();

        assert!(!old.exists(), "old otel-logger file should be deleted");
        assert!(recent.exists(), "recent otel-logger file should be kept");
        assert!(pid.exists(), "pid file must not be touched");
        assert!(stderr.exists(), "stderr file must not be touched");
        assert!(jsonl.exists(), "non-rotated JSONL file must not be touched");
        assert!(unrelated.exists(), "unrelated file must not be touched");
        assert!(
            invalid_date.exists(),
            "実在しない日付のファイルはローテーション出力として削除しない"
        );
        assert!(
            !old_pretty.exists(),
            "pretty-log の古い日次ファイルも同じ保持日数で削除する"
        );
        assert!(
            recent_pretty.exists(),
            "新しい pretty-log の日次ファイルは残す"
        );
        assert!(
            pretty_without_suffix.exists(),
            "`.log` の無い名前は pretty-log の出力ではない"
        );
        assert!(
            pretty_compressed.exists(),
            "拡張子が余分に付いた名前は pretty-log の出力ではない"
        );
        assert!(
            pretty_invalid_date.exists(),
            "実在しない日付の pretty-log 名は削除しない"
        );
    }

    #[test]
    fn rotated_log_filename_requires_a_real_calendar_date() {
        assert!(is_rotated_log_filename("otel-logger.2024-02-29"));
        assert!(!is_rotated_log_filename("otel-logger.2023-02-29"));
        assert!(!is_rotated_log_filename("otel-logger.2026-13-01"));
        assert!(!is_rotated_log_filename("otel-logger.2026-04-31"));
    }

    /// JSONL 専用の処理 (shutdown の fsync) に pretty-log を巻き込まないよう、
    /// 2 種類の日次ファイルを名前で区別できること。
    #[test]
    fn daily_log_kind_distinguishes_jsonl_and_pretty_files() {
        assert_eq!(
            daily_log_kind("otel-logger.2026-09-30"),
            Some(DailyLogKind::Jsonl)
        );
        assert_eq!(
            daily_log_kind("otel-logger.pretty.2026-09-30.log"),
            Some(DailyLogKind::Pretty)
        );
        assert!(!is_rotated_log_filename(
            "otel-logger.pretty.2026-09-30.log"
        ));
        for name in [
            "otel-logger.pretty.2026-09-30",
            "otel-logger.pretty.2026-09-30.log.gz",
            "otel-logger.pretty.2026-02-30.log",
            "otel-logger.prettyx.2026-09-30.log",
            "otel-logger.pretty..2026-09-30.log",
            "otel-logger.stdout.log",
            "otel-logger.2026-09-30.log",
        ] {
            assert_eq!(daily_log_kind(name), None, "{name} は日次ファイルではない");
        }
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_does_not_remove_symlink_named_like_rotated_log() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("target.jsonl");
        let link = dir.path().join("otel-logger.2020-01-01");
        std::fs::write(&target, "not a LogRoller output").unwrap();
        symlink(&target, &link).unwrap();

        // cutoff=now としても symlink は通常ファイルではないため削除しない。
        cleanup_old_rotated_logs(dir.path(), 0).unwrap();

        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "LogRoller が生成しない symlink は削除対象外"
        );
    }

    #[test]
    fn write_pretty_to_appends_summary_after_the_record() {
        let mut buf: Vec<u8> = Vec::new();

        write_pretty_to(&mut buf, "record\n", Some("summary\n")).unwrap();

        assert_eq!(String::from_utf8(buf).unwrap(), "record\nsummary\n");
    }

    #[test]
    fn stdout_broken_pipe_disables_further_pretty_output() {
        let mut state = PrettyWriterState::default();
        assert!(state.should_write());

        let report = state.record_result(
            &PrettyTarget::Stdout,
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed")),
            Instant::now(),
        );

        assert_eq!(
            report,
            HealthReport::Silent,
            "EPIPE は telemetry の欠落ではないので失敗として報告しない"
        );
        assert!(
            !state.should_write(),
            "読み手が消えた後に書き続けると、失敗のたびに stderr へ error を積み上げてしまう"
        );
    }

    /// EPIPE で恒久停止させるのは stdout だけ。pretty-log のファイルは読み手の有無と
    /// 関係ないため、同じ error kind でも一時的な失敗として扱い、書き続ける。
    #[test]
    fn broken_pipe_on_a_file_target_does_not_disable_output() {
        let mut state = PrettyWriterState::default();
        let target = PrettyTarget::Capture(Arc::default());

        let report = state.record_result(
            &target,
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "odd filesystem")),
            Instant::now(),
        );

        assert_eq!(report, HealthReport::Started);
        assert!(state.should_write());
    }

    #[test]
    fn stdout_transient_error_keeps_pretty_output_enabled() {
        let mut state = PrettyWriterState::default();

        let report = state.record_result(
            &PrettyTarget::Stdout,
            Err(io::Error::new(io::ErrorKind::Interrupted, "eintr")),
            Instant::now(),
        );

        assert_eq!(report, HealthReport::Started, "最初の失敗は報告する");
        assert!(
            state.should_write(),
            "一時的な error で人が読める出力を恒久停止しない"
        );
    }

    /// 回帰テスト: 失敗し続ける出力先の通知を間引く。
    ///
    /// ディスクが埋まっている間じゅう batch ごとに error を出すと、ローテーションされない
    /// stderr が伸びる。失敗の開始・継続中の定期報告・回復だけを知らせる。
    #[test]
    fn write_health_reports_start_reminders_and_recovery_only() {
        let t0 = Instant::now();
        let mut health = WriteHealth::default();

        assert_eq!(health.on_failure(t0), HealthReport::Started);
        for i in 1..100 {
            assert_eq!(
                health.on_failure(t0 + Duration::from_secs(i)),
                HealthReport::Silent,
                "続く失敗は報告間隔まで黙る"
            );
        }
        let reminder = t0 + PRETTY_FAILURE_REPORT_INTERVAL;
        assert_eq!(
            health.on_failure(reminder),
            HealthReport::Ongoing {
                failures: 101,
                elapsed: PRETTY_FAILURE_REPORT_INTERVAL,
            },
            "失敗が続いていれば報告間隔ごとに件数と経過時間を知らせる"
        );
        assert_eq!(
            health.unresolved(reminder),
            Some((101, PRETTY_FAILURE_REPORT_INTERVAL, Duration::ZERO))
        );

        assert_eq!(
            health.on_success(reminder + Duration::from_secs(5)),
            HealthReport::Silent,
            "直前まで失敗していたなら、成功 1 回ではまだ回復とみなさない"
        );
        let recovered = reminder + PRETTY_RECOVERY_QUIET_PERIOD;
        assert_eq!(
            health.on_success(recovered),
            HealthReport::Recovered {
                failures: 101,
                elapsed: PRETTY_FAILURE_REPORT_INTERVAL + PRETTY_RECOVERY_QUIET_PERIOD,
            }
        );
        assert_eq!(health.unresolved(recovered), None);
        assert_eq!(
            health.on_success(recovered),
            HealthReport::Silent,
            "正常な書き込みは報告しない"
        );
    }

    /// 回帰テスト: 成功と失敗が交互に来る (空きの少ないディスクで、小さい batch だけ
    /// 書ける等) 間は 1 つの障害として扱う。batch ごとに開始と回復を出すと stderr が
    /// 伸び、直後の失敗を間引くと「回復した」が最後の通知のまま失敗し続けてしまう。
    #[test]
    fn write_health_treats_alternating_failures_as_one_outage() {
        let t0 = Instant::now();
        let mut health = WriteHealth::default();

        assert_eq!(health.on_failure(t0), HealthReport::Started);
        for i in 1..50 {
            let now = t0 + Duration::from_secs(i);
            let report = if i % 2 == 0 {
                health.on_failure(now)
            } else {
                health.on_success(now)
            };
            assert_eq!(
                report,
                HealthReport::Silent,
                "{i} 秒目: 成功が挟まっても同じ障害の続き"
            );
        }
        // 最後の失敗は 48 秒目。静かな期間を過ぎた成功で、回復を 1 回だけ知らせる。
        let last_failure = t0 + Duration::from_secs(48);
        assert_eq!(
            health.on_success(last_failure + PRETTY_RECOVERY_QUIET_PERIOD),
            HealthReport::Recovered {
                failures: 25,
                elapsed: Duration::from_secs(48) + PRETTY_RECOVERY_QUIET_PERIOD,
            }
        );
    }

    /// 回復を知らせた後の失敗は、報告間隔を待たずに新しい障害として知らせる。
    #[test]
    fn write_health_reports_a_new_outage_right_after_recovery() {
        let t0 = Instant::now();
        let mut health = WriteHealth::default();

        assert_eq!(health.on_failure(t0), HealthReport::Started);
        let recovered = t0 + PRETTY_RECOVERY_QUIET_PERIOD;
        assert!(matches!(
            health.on_success(recovered),
            HealthReport::Recovered { failures: 1, .. }
        ));
        assert_eq!(
            health.on_failure(recovered + Duration::from_secs(1)),
            HealthReport::Started
        );
    }

    /// 回帰テスト: queue の件数上限だけでは memory を抑えられないので、rendered の
    /// 合計バイト数でも打ち切る。捨てた batch は計上から巻き戻す。
    #[tokio::test]
    async fn enqueue_drops_batches_beyond_the_byte_budget() {
        // writer を起動せず、queue に積まれた状態を観察する。容量 1 で満杯も再現する。
        let (tx, mut rx) = mpsc::channel(1);
        let output = PrettyOutput {
            name: "test",
            color: false,
            tx,
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            max_queued_bytes: 10,
            dropped: AtomicU64::new(0),
        };
        let counters = || {
            (
                output.dropped.load(AtomicOrdering::Relaxed),
                output.queued_bytes.load(AtomicOrdering::Relaxed),
            )
        };

        output.enqueue(Arc::from("0123456789A"), false);
        assert_eq!(
            counters(),
            (1, 0),
            "queue が空でも上限を超える batch は捨て、計上から外す"
        );

        output.enqueue(Arc::from("0123"), false);
        assert_eq!(counters(), (1, 4));
        output.enqueue(Arc::from("45"), false);
        assert_eq!(
            counters(),
            (2, 4),
            "上限内でも channel が満杯なら捨て、計上を巻き戻す"
        );
        output.enqueue(Arc::from("0123456"), false);
        assert_eq!(
            counters(),
            (3, 4),
            "溜まっている分に足すと上限を超える batch は捨てる"
        );

        assert_eq!(&*rx.try_recv().unwrap().rendered, "0123");
        drop(rx);
        output.enqueue(Arc::from("67"), false);
        assert_eq!(
            counters(),
            (4, 4),
            "writer が止まった (channel closed) 分も捨てた件数に数え、計上を巻き戻す"
        );
    }

    /// writer が書き終えた batch は計上から外れ、queue の予算が戻る。
    #[tokio::test]
    async fn writer_releases_the_byte_budget_after_writing() {
        let captured = Arc::new(StdMutex::new(Vec::new()));
        let output = PrettyOutput::spawn(
            PrettyTarget::Capture(Arc::clone(&captured)),
            false,
            Arc::new(Aggregator::new()),
        );

        for line in ["a\n", "b\n", "c\n"] {
            output.enqueue(Arc::from(line), false);
        }
        output
            .drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await;

        assert_eq!(output.queued_bytes.load(AtomicOrdering::Relaxed), 0);
        assert_eq!(output.dropped.load(AtomicOrdering::Relaxed), 0);
        assert_eq!(&*lock_recovering(&captured), b"a\nb\nc\n");
    }

    /// 回帰テスト: 止まった出力先が、もう一方の出力先を巻き込まない。
    ///
    /// 1 本の writer で両方へ書くと、読み手が止まった stdout が pretty-log まで止める。
    /// shutdown の drain も共有の期限で打ち切り、止まった側に期限を使い切られない。
    #[tokio::test]
    async fn stalled_destination_does_not_block_the_other_destination() {
        let gate = Arc::new((StdMutex::new(false), std::sync::Condvar::new()));
        let captured = Arc::new(StdMutex::new(Vec::new()));
        let sink = sink_with_pretty_targets(
            Some(PrettyTarget::Stall(Arc::clone(&gate))),
            Some(PrettyTarget::Capture(Arc::clone(&captured))),
            false,
        );

        for _ in 0..3 {
            sink.record(TelemetryRecord::Logs(Box::new(claude_api_request_log())))
                .await
                .expect("人が読める出力が止まっても OTLP の ACK は止めない");
        }
        wait_until("pretty-log 側に 3 batch が書かれる", || {
            String::from_utf8_lossy(&lock_recovering(&captured))
                .matches("body=claude_code.api_request")
                .count()
                == 3
        })
        .await;

        let started = Instant::now();
        sink.drain_pretty_until(tokio::time::Instant::now() + Duration::from_millis(200))
            .await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "止まった出力先があっても共有の期限で drain を打ち切る: {:?}",
            started.elapsed()
        );

        // blocking thread を解放する (runtime の drop が完了を待ち続けないように)。
        open_gate(&gate);
    }

    /// `--pretty-log` は log-dir に色なしの日次ファイルを書き、`--summary` の累計も載せる。
    /// stdout 用の `color = "always"` はファイルに漏らさない。
    #[tokio::test]
    async fn pretty_log_writes_uncolored_records_and_summary_to_daily_file() {
        use crate::cli::ColorMode;

        let dir = tempfile::TempDir::new().unwrap();
        let mut settings = settings_with_log_dir(dir.path().to_path_buf());
        settings.pretty_log_dir = Some(dir.path().to_path_buf());
        settings.color = ColorMode::Always;
        settings.summary = true;
        let sink = Sink::from_settings(&settings).await.unwrap();
        assert!(
            sink.inner.stdout.is_none(),
            "no-stdout なので stdout へは書かない"
        );

        sink.record(TelemetryRecord::Logs(Box::new(claude_api_request_log())))
            .await
            .unwrap();
        sink.flush().await.unwrap();

        let pretty_files = files_of_kind(dir.path(), DailyLogKind::Pretty);
        assert_eq!(pretty_files.len(), 1, "当日の pretty-log が 1 つできる");
        let body = std::fs::read_to_string(&pretty_files[0]).unwrap();
        assert!(
            body.contains("body=claude_code.api_request"),
            "record の行を書く: {body}"
        );
        assert!(
            body.contains("[stats:claude-code]"),
            "`--summary` の累計も pretty-log へ書く: {body}"
        );
        assert!(!body.contains('\x1b'), "ファイルには色を付けない: {body:?}");
        assert_eq!(
            files_of_kind(dir.path(), DailyLogKind::Jsonl).len(),
            1,
            "JSONL は従来どおり別ファイルに書く"
        );
    }

    #[tokio::test]
    async fn no_stdout_settings_disable_pretty_and_summary_output() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut settings = settings_with_log_file(dir.path().join("out.jsonl"));
        // `--pretty-log` が無ければ、`--summary` を併用しても `--no-stdout` が勝つ
        // (summary も人が読める出力へ書くため)。
        settings.summary = true;

        let sink = Sink::from_settings(&settings).await.unwrap();

        assert!(
            sink.inner.pretty_outputs().next().is_none(),
            "no-stdout 指定時は summary も含め人が読める出力を書かない"
        );
    }

    fn settings_with_log_file(path: std::path::PathBuf) -> crate::cli::Settings {
        use crate::cli::{ColorMode, LogSink};
        crate::cli::Settings {
            grpc_addr: "127.0.0.1:0".parse().unwrap(),
            http_addr: "127.0.0.1:0".parse().unwrap(),
            log_sink: Some(LogSink::File(path)),
            no_stdout: true,
            pretty_log_dir: None,
            summary: false,
            color: ColorMode::Never,
            dry_run: false,
            proxy: None,
        }
    }

    fn settings_with_log_dir(dir: std::path::PathBuf) -> crate::cli::Settings {
        use crate::cli::{ColorMode, LogSink};
        crate::cli::Settings {
            grpc_addr: "127.0.0.1:0".parse().unwrap(),
            http_addr: "127.0.0.1:0".parse().unwrap(),
            log_sink: Some(LogSink::Directory { dir, keep_days: 1 }),
            no_stdout: true,
            pretty_log_dir: None,
            summary: false,
            color: ColorMode::Never,
            dry_run: false,
            proxy: None,
        }
    }

    #[tokio::test]
    async fn shutdown_waits_for_cancelled_handler_and_rejects_later_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let sink = Sink::from_settings(&settings_with_log_file(path.clone()))
            .await
            .unwrap();
        let JsonlWriter::File(writer) = sink.inner.file.as_ref().unwrap() else {
            panic!()
        };
        // writer を別 thread で止め、handler の cancel と disk I/O 完了の順を固定する。
        let gate = Arc::new((StdMutex::new(false), std::sync::Condvar::new()));
        let (locked_tx, locked_rx) = oneshot::channel();
        let blocker_sink = sink.clone();
        let blocker_gate = gate.clone();
        let blocker = tokio::task::spawn_blocking(move || {
            let JsonlWriter::File(writer) = blocker_sink.inner.file.as_ref().unwrap() else {
                panic!()
            };
            let _writer = lock_recovering(writer);
            locked_tx.send(()).unwrap();
            let mut open = lock_recovering(&blocker_gate.0);
            while !*open {
                open = blocker_gate.1.wait(open).unwrap();
            }
        });
        locked_rx.await.unwrap();
        assert!(writer.try_lock().is_err());
        let recording_sink = sink.clone();
        let handler = tokio::spawn(async move {
            recording_sink
                .record(TelemetryRecord::Logs(Box::default()))
                .await
        });
        wait_until("受理済み guard", || {
            sink.inner.admission.try_write().is_err()
        })
        .await;
        handler.abort();
        let _ = handler.await;
        let closing_sink = sink.clone();
        let closing = tokio::spawn(async move { closing_sink.shutdown().await });
        tokio::task::yield_now().await;
        assert!(
            !closing.is_finished(),
            "書き込み完了前に最終同期を終了しない"
        );
        open_gate(&gate);
        blocker.await.unwrap();
        closing.await.unwrap().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(serde_json::from_slice::<serde_json::Value>(&bytes).is_ok());
        assert!(
            sink.record(TelemetryRecord::Logs(Box::default()))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes);
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

    /// 日次ローテーション出力でも `JsonlWriter::Roller` 経由で payload を欠落なく保存できること。
    /// `LogRoller` はサイズが大きいため enum 内では間接化しているが、書き込み動作は同じ。
    #[tokio::test]
    async fn record_persists_payload_to_rotated_directory_sink() {
        let dir = tempfile::TempDir::new().unwrap();
        let sink = Sink::from_settings(&settings_with_log_dir(dir.path().to_path_buf()))
            .await
            .unwrap();

        let req = ExportLogsServiceRequest {
            resource_logs: vec![],
        };
        sink.record(TelemetryRecord::Logs(Box::new(req)))
            .await
            .expect("directory sink でも record で永続化に成功すること");

        let rotated_files = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_rotated_log_filename)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rotated_files.len(),
            1,
            "日次ローテーションファイルが 1 つ作成される"
        );
        let body = std::fs::read_to_string(&rotated_files[0]).unwrap();
        assert!(
            body.contains("\"kind\":\"logs\""),
            "rotated JSON Lines に追記される"
        );
        assert!(body.ends_with('\n'), "各レコードは改行で終わる");

        // ACK 時点の batch flush とは別に、graceful shutdown の最終 flush も成功すること。
        sink.flush().await.unwrap();
    }

    /// 回帰テスト: `--log-dir` 経路の fsync がローテーション名のファイルだけを対象にする。
    ///
    /// 日付切り替え前のファイルも disk へ確定するため、日次ファイルを開き直して
    /// `sync_all` する。その際、同じ prefix の別用途ファイル (`otel-logger.pid` など) を
    /// 巻き込んで書き込みモードで開かないことを固定する。
    #[test]
    fn sync_rotated_files_skips_files_that_are_not_daily_logs() {
        let dir = tempfile::TempDir::new().unwrap();
        // 同じ prefix だがローテーション名ではないファイルを read-only で置く。
        // 対象に含めてしまうと `write(true)` の open が失敗するので検出できる。
        // pretty-log の日次ファイルも、JSONL 専用の fsync には含めない。
        for name in ["otel-logger.pid", "otel-logger.pretty.2026-09-30.log"] {
            let unrelated = dir.path().join(name);
            std::fs::write(&unrelated, "4242").unwrap();
            let mut perms = std::fs::metadata(&unrelated).unwrap().permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&unrelated, perms).unwrap();
        }

        let writer = open_rotated_sync(dir.path(), 7).unwrap();
        writer
            .sync_rotated_files()
            .expect("JSONL の日次ファイル以外は fsync 対象に含めない");

        // 日次ファイルを作ってから呼び直しても成功する。
        let jsonl = JsonlWriter::Roller(Box::new(writer));
        jsonl.write_line(b"{}\n").unwrap();
        jsonl.flush().expect("日次ファイルを fsync できる");
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

    /// `record()` が `Ok` を返した時点で payload が disk 上の JSONL に到達していること。
    /// 旧実装では `BufWriter` のメモリバッファに留まったまま ACK し、後続の `sink.flush()`
    /// 失敗で batch が失われる余地があったため、その regression test を兼ねる。
    #[tokio::test]
    async fn record_writes_to_kernel_before_returning_ok() {
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
        // ここでは sink.flush() を呼ばずに直接ファイルを読む。writer のままなら 0 byte の
        // 可能性があるが、新実装では batch 毎の flush で kernel に渡している。
        let body = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            body.contains("\"kind\":\"logs\""),
            "ACK 前に kernel へ書き込まれている必要がある: actual={body:?}"
        );
    }
}
