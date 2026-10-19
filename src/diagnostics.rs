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
    // 直前の行を途中まで書いたところで失敗したか。
    let mut torn = false;
    while let Ok(message) = rx.recv() {
        match message {
            Message::Line(line) => {
                // 捨てた件数を、次に書ける時に知らせる。知らせる行が書けなければ、
                // その件数と今回の行を数え直して次の機会に回す。
                let lost = dropped.swap(0, Ordering::Relaxed);
                if lost > 0 && !write_line(&mut out, dropped_note(lost).as_bytes(), &mut torn) {
                    dropped.fetch_add(lost.saturating_add(1), Ordering::Relaxed);
                    continue;
                }
                if !write_line(&mut out, &line, &mut torn) {
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

/// 1 行を書き切ったか。前の行が途中で切れていれば、先に改行を補う。
///
/// ディスクが埋まると write(2) は書ける分だけ書いてから ENOSPC を返すので、改行の無い
/// 断片が残る。改行を補わないと、空きが戻った後の最初の行 (捨てた件数の知らせ) が断片の
/// 続きに連結され、行頭の時刻で探せなくなる。
fn write_line<W: Write>(out: &mut W, line: &[u8], torn: &mut bool) -> bool {
    if *torn {
        if write_counting(out, b"\n").is_err() {
            return false;
        }
        *torn = false;
    }
    match write_counting(out, line) {
        Ok(()) => true,
        Err(written) => {
            *torn = written > 0;
            false
        }
    }
}

/// `buf` を書き切る。失敗した時は、それまでに書けた byte 数を返す (`write_all` は返さない)。
fn write_counting<W: Write>(out: &mut W, buf: &[u8]) -> Result<(), usize> {
    let mut written = 0;
    while written < buf.len() {
        match out.write(&buf[written..]) {
            Ok(0) => return Err(written),
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(written),
        }
    }
    Ok(())
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

    /// 残りの容量だけ書いてから ENOSPC で失敗する出力先 (空きの尽きかけたディスクの代わり)。
    /// `room` が `None` の間は全部書ける。
    #[derive(Clone, Default)]
    struct ShortWriteOutput {
        room: Arc<Mutex<Option<usize>>>,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for ShortWriteOutput {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut room = self.room.lock().unwrap();
            let n = match *room {
                None => buf.len(),
                Some(0) => return Err(io::Error::other("no space left on device")),
                Some(left) => {
                    let n = left.min(buf.len());
                    *room = Some(left - n);
                    n
                }
            };
            self.written.lock().unwrap().extend_from_slice(&buf[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// 回帰テスト: 行の途中まで書いて失敗した後は、次の行の前に改行を補う。
    ///
    /// 修正前は `write_all` が途中まで書いた断片をそのまま残し、空きが戻った後の最初の行
    /// (捨てた件数の知らせ) が断片の後ろに連結されて、行頭の時刻で探せなかった。補う改行
    /// 自体が書けない間は、その行も捨てて数える。
    #[test]
    fn torn_line_is_terminated_before_the_next_line() {
        let output = ShortWriteOutput::default();
        let (mut writer, guard) = spawn(output.clone(), 16, Duration::from_secs(5)).unwrap();
        *output.room.lock().unwrap() = Some(4);
        writer.write_all(b"partial-line\n").unwrap();
        writer.write_all(b"while full\n").unwrap();
        assert!(guard.flush_until(Instant::now() + Duration::from_secs(5)));
        assert_eq!(*output.written.lock().unwrap(), b"part");

        *output.room.lock().unwrap() = None;
        writer.write_all(b"recovered\n").unwrap();
        assert!(guard.flush_until(Instant::now() + Duration::from_secs(5)));
        let written = String::from_utf8(output.written.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines.len(), 3, "{written:?}");
        assert_eq!(lines[0], "part", "断片はそこで行を終える");
        assert!(
            lines[1].contains(" WARN 2 diagnostic log line(s) were dropped"),
            "知らせは新しい行から始まる: {written:?}"
        );
        assert_eq!(lines[2], "recovered");
    }

    /// gate が開くまで止まり、開いた後は `failing` の間だけ失敗する出力先。
    /// 「読まれない pipe で溢れた」後に「ディスクが埋まって書けない」が続く状況の代わり。
    #[derive(Clone, Default)]
    struct StalledThenFailingOutput {
        stalled: StalledOutput,
        failing: Arc<Mutex<bool>>,
    }

    impl Write for StalledThenFailingOutput {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            // gate が開くまで止まり、開いた後の失敗の有無はその時点の設定で決める。
            let (state, changed) = &*self.stalled.state;
            let mut guard = state.lock().unwrap();
            guard.blocked = !guard.open;
            changed.notify_all();
            while !guard.open {
                guard = changed.wait(guard).unwrap();
            }
            guard.blocked = false;
            if *self.failing.lock().unwrap() {
                return Err(io::Error::other("no space left on device"));
            }
            guard.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// queue が溢れて捨てた行と、書き込みに失敗した行 (知らせる行自体が書けなかった場合を
    /// 含む) を、それぞれ 1 回ずつ数えて 1 行で知らせる。
    ///
    /// 知らせる行が書けなかった時に件数を数え直さないと、溢れた分の件数が消える。逆に
    /// 二重に数えると、実際より多く捨てたように見える。
    #[test]
    fn full_queue_and_failed_writes_are_counted_once_each() {
        let output = StalledThenFailingOutput::default();
        let (mut writer, guard) = spawn(output.clone(), 2, Duration::from_secs(5)).unwrap();
        writer.write_all(b"in-flight\n").unwrap();
        output.stalled.wait_until_blocked();
        for line in ["queued-1\n", "queued-2\n", "overflow-1\n", "overflow-2\n"] {
            writer.write_all(line.as_bytes()).unwrap();
        }
        assert_eq!(
            writer.dropped.load(Ordering::Relaxed),
            2,
            "queue (2 件) に入らない分だけを捨てる"
        );

        // 止まっていた行も、queue の 2 行 (と、その前に知らせる行) も書けない。
        *output.failing.lock().unwrap() = true;
        output.stalled.open();
        assert!(guard.flush_until(Instant::now() + Duration::from_secs(5)));
        assert!(output.stalled.written().is_empty());

        *output.failing.lock().unwrap() = false;
        writer.write_all(b"recovered\n").unwrap();
        assert!(guard.flush_until(Instant::now() + Duration::from_secs(5)));
        let written = output.stalled.written();
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines.len(), 2, "{written}");
        // 溢れた 2 行 + 書けなかった 3 行 (in-flight / queued-1 / queued-2) = 5 行。
        assert!(
            lines[0].contains(" WARN 5 diagnostic log line(s) were dropped"),
            "{written}"
        );
        assert_eq!(lines[1], "recovered");
        assert_eq!(
            writer.dropped.load(Ordering::Relaxed),
            0,
            "知らせた件数は持ち越さない"
        );
    }

    /// queue が満杯で書き出しの依頼すら積めない時も、終了時の書き出し待ちは期限で打ち切る。
    #[test]
    fn flush_gives_up_at_the_deadline_when_the_queue_is_full() {
        let output = StalledOutput::default();
        let (mut writer, guard) = spawn(output.clone(), 1, Duration::from_millis(50)).unwrap();
        writer.write_all(b"stuck\n").unwrap();
        output.wait_until_blocked();
        writer.write_all(b"fills-the-queue\n").unwrap();
        writer.write_all(b"dropped\n").unwrap();
        assert_eq!(writer.dropped.load(Ordering::Relaxed), 1);

        let started = Instant::now();
        assert!(!guard.flush_until(Instant::now() + Duration::from_millis(50)));
        drop(guard);
        assert!(started.elapsed() < Duration::from_secs(2));
        output.open();
    }

    /// 1 行ずつ時間の掛かる出力先 (遅いが止まってはいない端末や pipe の代わり)。
    #[derive(Clone, Default)]
    struct SlowOutput {
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for SlowOutput {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            thread::sleep(Duration::from_millis(10));
            self.written.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// guard を drop すると、それまでに積んだ行を書き終えてから戻る。
    ///
    /// main の終わりで guard が落ちた直後にプロセスが終わるため、待たずに戻ると、
    /// 終了直前の診断ログ (shutdown の経過や失敗の理由) が stderr に残らない。
    #[test]
    fn dropping_the_guard_writes_out_queued_lines_before_returning() {
        let output = SlowOutput::default();
        let (mut writer, guard) = spawn(output.clone(), 16, Duration::from_secs(5)).unwrap();
        for i in 0..5 {
            writer.write_all(format!("line-{i}\n").as_bytes()).unwrap();
        }
        drop(guard);
        let written = String::from_utf8(output.written.lock().unwrap().clone()).unwrap();
        assert_eq!(
            written.lines().collect::<Vec<_>>(),
            ["line-0", "line-1", "line-2", "line-3", "line-4"]
        );
    }

    /// 捨てた件数の知らせは、tracing の行と同じ形 (先頭に RFC 3339 の時刻と水準) の 1 行にする。
    /// 改行を含めると、行単位で読む log driver や grep で知らせが分断される。
    #[test]
    fn dropped_note_is_a_single_timestamped_warn_line() {
        for lost in [1, u64::MAX] {
            let note = dropped_note(lost);
            assert!(note.ends_with('\n'), "{note:?}");
            assert_eq!(note.matches('\n').count(), 1, "{note:?}");
            let (timestamp, rest) = note.split_once("  ").unwrap();
            assert!(
                chrono::DateTime::parse_from_rfc3339(timestamp).is_ok(),
                "{timestamp:?} は RFC 3339 の時刻"
            );
            assert!(rest.starts_with(&format!("WARN {lost} ")), "{note:?}");
            assert!(
                rest.contains(&format!("診断ログを {lost} 行捨てました")),
                "{note:?}"
            );
        }
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
