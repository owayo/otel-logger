//! 診断ログ (tracing) を stderr へ書き出す、待たない writer。
//!
//! 受信処理 (OTLP の ACK 経路) が出す診断ログを stderr へ直接書くと、読み手が読まない
//! pipe (`2>&1 | less` でスクロールを止めた、Docker / systemd の log driver が詰まった) では
//! write(2) が戻らず、ACK が止まる。exporter が timeout して再送すると、保存済みの payload が
//! JSONL に重複し、累計も二重計上される。人が読める出力 (stdout / pretty-log) を ACK 経路から
//! 切り離したのと同じ理由で、診断ログも専用のスレッドへ渡し、追いつかない分は捨てる。
//! 捨てた件数は、次に書けた時に 1 行で知らせる。

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// queue に溜められる診断ログの件数。1 件は tracing の event 1 行 (数百 byte) なので、
/// 満杯でも数百 KB に収まる。
const QUEUE_CAPACITY: usize = 1024;

/// 終了時に、溜まっている診断ログの書き出しを待つ上限。stderr が止まっていても、
/// プロセスの終了を人質に取られない。
const FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

enum Message {
    Line(Vec<u8>),
    /// ここまでに積んだ行を書き終えたら知らせる。
    Flush(mpsc::Sender<()>),
}

/// tracing-subscriber の `with_writer` に渡す writer。clone は軽い。
///
/// `write` は queue に積むだけで、満杯なら捨てて数える。呼び出し元を待たせず、
/// 書き込みの失敗も返さない (tracing-subscriber が代わりに `eprintln!` で報告しようとして
/// panic する経路を作らない)。
#[derive(Clone)]
pub struct DiagnosticWriter {
    tx: SyncSender<Message>,
    dropped: Arc<AtomicU64>,
}

impl Write for DiagnosticWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.tx.try_send(Message::Line(buf.to_vec())).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 書き出し用スレッドの持ち手。drop すると、溜まっている診断ログを期限まで書き出す。
pub struct DiagnosticGuard {
    tx: SyncSender<Message>,
    flush_timeout: Duration,
}

impl DiagnosticGuard {
    /// `deadline` までに、これまでに積んだ行を書き終えたか。
    fn flush_until(&self, deadline: Instant) -> bool {
        let (done_tx, done_rx) = mpsc::channel();
        let mut message = Message::Flush(done_tx);
        loop {
            match self.tx.try_send(message) {
                Ok(()) => break,
                Err(TrySendError::Full(back)) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    message = back;
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        done_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .is_ok()
    }
}

impl Drop for DiagnosticGuard {
    fn drop(&mut self) {
        self.flush_until(Instant::now() + self.flush_timeout);
    }
}

/// stderr へ書く writer と、終了時に書き出しを待つ guard を作る。
pub fn stderr() -> io::Result<(DiagnosticWriter, DiagnosticGuard)> {
    spawn(io::stderr(), QUEUE_CAPACITY, FLUSH_TIMEOUT)
}

fn spawn<W: Write + Send + 'static>(
    out: W,
    capacity: usize,
    flush_timeout: Duration,
) -> io::Result<(DiagnosticWriter, DiagnosticGuard)> {
    let (tx, rx) = mpsc::sync_channel(capacity);
    let dropped = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&dropped);
    thread::Builder::new()
        .name("diagnostics".into())
        .spawn(move || run(rx, out, &counter))?;
    Ok((
        DiagnosticWriter {
            tx: tx.clone(),
            dropped,
        },
        DiagnosticGuard { tx, flush_timeout },
    ))
}

fn run<W: Write>(rx: Receiver<Message>, mut out: W, dropped: &AtomicU64) {
    while let Ok(message) = rx.recv() {
        match message {
            Message::Line(line) => {
                // 捨てた件数を、次に書ける時に知らせる。知らせる行が書けなければ、
                // その件数と今回の行を数え直して次の機会に回す。
                let lost = dropped.swap(0, Ordering::Relaxed);
                if lost > 0 && out.write_all(dropped_note(lost).as_bytes()).is_err() {
                    dropped.fetch_add(lost.saturating_add(1), Ordering::Relaxed);
                    continue;
                }
                if out.write_all(&line).is_err() {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            Message::Flush(done) => {
                let _ = out.flush();
                let _ = done.send(());
            }
        }
    }
}

/// 捨てた件数を知らせる 1 行。tracing の行と並べて読めるよう、時刻と水準を先頭に置く。
fn dropped_note(lost: u64) -> String {
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    format!(
        "{now}  WARN {lost} diagnostic log line(s) were dropped while stderr was not writable \
         or not keeping up / stderr に書けない・追いつかない間に、診断ログを {lost} 行捨てました\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, Mutex};

    /// gate が開くまで書き込みを止める出力先 (読まない pipe の代わり)。書けた分は溜める。
    #[derive(Clone, Default)]
    struct StalledOutput {
        state: Arc<(Mutex<StalledState>, Condvar)>,
    }

    #[derive(Default)]
    struct StalledState {
        open: bool,
        /// 書き込みを止めている最中か (テストが「スレッドが詰まった」ことを待つのに使う)。
        blocked: bool,
        written: Vec<u8>,
    }

    impl StalledOutput {
        fn open(&self) {
            let (state, changed) = &*self.state;
            state.lock().unwrap().open = true;
            changed.notify_all();
        }

        fn wait_until_blocked(&self) {
            let (state, changed) = &*self.state;
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut guard = state.lock().unwrap();
            while !guard.blocked {
                let left = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !left.is_zero(),
                    "書き出し用スレッドが出力先で止まらなかった"
                );
                guard = changed.wait_timeout(guard, left).unwrap().0;
            }
        }

        fn written(&self) -> String {
            String::from_utf8(self.state.0.lock().unwrap().written.clone()).unwrap()
        }
    }

    impl Write for StalledOutput {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let (state, changed) = &*self.state;
            let mut guard = state.lock().unwrap();
            guard.blocked = !guard.open;
            changed.notify_all();
            while !guard.open {
                guard = changed.wait(guard).unwrap();
            }
            guard.blocked = false;
            guard.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// 失敗の有無を切り替えられる出力先 (ENOSPC の間だけ書けないディスクの代わり)。
    #[derive(Clone, Default)]
    struct FlakyOutput {
        failing: Arc<Mutex<bool>>,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for FlakyOutput {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if *self.failing.lock().unwrap() {
                return Err(io::Error::other("no space left on device"));
            }
            self.written.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// 回帰テスト: 読み手が読まない stderr でも、診断ログを出す側を待たせない。
    ///
    /// 受信処理の WARN が stderr の write(2) で止まると ACK が返らず、exporter の再送で
    /// 保存済みの batch が JSONL に重複する。溢れた分は捨て、書けるようになったら件数を知らせる。
    #[test]
    fn stalled_output_does_not_block_writers_and_reports_dropped_lines() {
        let output = StalledOutput::default();
        let (mut writer, guard) = spawn(output.clone(), 2, Duration::from_secs(5)).unwrap();
        writer.write_all(b"first\n").unwrap();
        output.wait_until_blocked();

        let started = Instant::now();
        for line in ["second\n", "third\n", "lost-1\n", "lost-2\n", "lost-3\n"] {
            assert_eq!(writer.write(line.as_bytes()).unwrap(), line.len());
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "出力先が止まっていても write は待たない"
        );
        assert_eq!(writer.dropped.load(Ordering::Relaxed), 3);

        output.open();
        assert!(guard.flush_until(Instant::now() + Duration::from_secs(5)));
        let written = output.written();
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines.len(), 4, "{written}");
        assert_eq!(lines[0], "first");
        assert!(
            lines[1].contains(" WARN 3 diagnostic log line(s) were dropped"),
            "{written}"
        );
        assert_eq!(&lines[2..], ["second", "third"]);
    }

    /// 書けなかった行も捨てた件数に数え、書けるようになった時に知らせる。
    #[test]
    fn failed_writes_are_reported_once_output_recovers() {
        let output = FlakyOutput::default();
        let (mut writer, guard) = spawn(output.clone(), 16, Duration::from_secs(5)).unwrap();
        *output.failing.lock().unwrap() = true;
        writer.write_all(b"while full 1\n").unwrap();
        writer.write_all(b"while full 2\n").unwrap();
        assert!(guard.flush_until(Instant::now() + Duration::from_secs(5)));
        assert!(output.written.lock().unwrap().is_empty());

        *output.failing.lock().unwrap() = false;
        writer.write_all(b"recovered\n").unwrap();
        assert!(guard.flush_until(Instant::now() + Duration::from_secs(5)));
        let written = String::from_utf8(output.written.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines.len(), 2, "{written}");
        assert!(lines[0].contains(" WARN 2 diagnostic log line(s) were dropped"));
        assert_eq!(lines[1], "recovered");
    }

    /// 出力先が止まったままでも、終了時の書き出し待ちは期限で打ち切る。
    #[test]
    fn flush_gives_up_at_the_deadline_when_output_is_stalled() {
        let output = StalledOutput::default();
        let (mut writer, guard) = spawn(output.clone(), 2, Duration::from_millis(50)).unwrap();
        writer.write_all(b"stuck\n").unwrap();
        output.wait_until_blocked();

        let started = Instant::now();
        assert!(!guard.flush_until(Instant::now() + Duration::from_millis(50)));
        drop(guard);
        assert!(started.elapsed() < Duration::from_secs(2));
        // テストの後始末として書き出し用スレッドを止めたままにしない。
        output.open();
    }
}
