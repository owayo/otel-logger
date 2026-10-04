//! JSONL の部分書き込みを取り消し、書き込み時の暦日でファイルを切り替える。

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{Local, NaiveDate};

/// 部分書き込みの取り消しに必要な操作。テストではディスク障害を注入する。
trait AppendStorage: Write {
    fn len(&self) -> io::Result<u64>;
    fn truncate(&mut self, len: u64) -> io::Result<()>;
}

impl AppendStorage for File {
    fn len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.set_len(len)?;
        // macOS は O_APPEND でも RLIMIT_FSIZE を現在位置に対して先に検査する。
        // set_len は位置を戻さないため、次の小さな batch まで EFBIG になるのを防ぐ。
        self.seek(SeekFrom::Start(len))?;
        Ok(())
    }
}

struct TransactionalAppend<F> {
    file: F,
    /// truncate 自体が失敗したときも、次の行を壊れた末尾へ連結しない。
    rollback: Option<u64>,
}

impl<F: AppendStorage> TransactionalAppend<F> {
    /// ファイルを切り替える場合も、未完了の取り消しを先に済ませる。
    fn repair(&mut self) -> io::Result<()> {
        if let Some(start) = self.rollback {
            self.file.truncate(start)?;
            self.rollback = None;
        }
        Ok(())
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.repair()?;
        let start = self.file.len()?;
        if let Err(error) = self.file.write_all(bytes).and_then(|()| self.file.flush()) {
            self.rollback = Some(start);
            if self.file.truncate(start).is_ok() {
                self.rollback = None;
            }
            return Err(error);
        }
        Ok(())
    }
}

pub(crate) struct AppendFile {
    writer: TransactionalAppend<File>,
}

impl AppendFile {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.create(true).read(true).append(true);
        crate::path::restrict_new_file_mode(&mut options);
        let mut file = options.open(path)?;
        recover_tail(&mut file)?;
        Ok(Self {
            writer: TransactionalAppend {
                file,
                rollback: None,
            },
        })
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.append(bytes)
    }

    pub(crate) fn sync_all(&self) -> io::Result<()> {
        self.writer.file.sync_all()
    }
}

/// ACK 前に改行まで書く契約なので、改行のない末尾は未完了の batch だけである。
/// 全ファイルを読むことなく、末尾から固定サイズで最後の改行を探して取り消す。
fn recover_tail(file: &mut File) -> io::Result<()> {
    let original_len = file.metadata()?.len();
    let mut end = original_len;
    let mut buf = [0_u8; 8192];
    let valid_len = loop {
        if end == 0 {
            break 0;
        }
        let start = end.saturating_sub(buf.len() as u64);
        let size = (end - start) as usize;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf[..size])?;
        if let Some(pos) = buf[..size].iter().rposition(|byte| *byte == b'\n') {
            break start + pos as u64 + 1;
        }
        end = start;
    };
    if valid_len != original_len {
        AppendStorage::truncate(file, valid_len)?;
        tracing::warn!(
            discarded_bytes = original_len - valid_len,
            "removed incomplete log tail before appending; the batch was not acknowledged"
        );
    }
    Ok(())
}

/// 無通信の期間が何日あっても、次の書き込みを当日のファイルへ保存する。
pub(crate) struct DailyWriter {
    dir: PathBuf,
    prefix: String,
    suffix: Option<String>,
    date: NaiveDate,
    file: AppendFile,
}

impl DailyWriter {
    pub(crate) fn new(dir: &Path, prefix: &str, suffix: Option<&str>) -> io::Result<Self> {
        Self::at_date(dir, prefix, suffix, Local::now().date_naive())
    }

    fn at_date(
        dir: &Path,
        prefix: &str,
        suffix: Option<&str>,
        date: NaiveDate,
    ) -> io::Result<Self> {
        let file = AppendFile::open(&daily_path(dir, prefix, suffix, date))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            prefix: prefix.to_string(),
            suffix: suffix.map(str::to_string),
            date,
            file,
        })
    }

    fn append_at(&mut self, bytes: &[u8], date: NaiveDate) -> io::Result<()> {
        if self.date != date {
            // 前日の壊れた tail を残したまま writer を捨てない。
            self.file.writer.repair()?;
            let file = AppendFile::open(&daily_path(
                &self.dir,
                &self.prefix,
                self.suffix.as_deref(),
                date,
            ))?;
            self.file = file;
            self.date = date;
        }
        self.file.append(bytes)
    }
}

fn daily_path(dir: &Path, prefix: &str, suffix: Option<&str>, date: NaiveDate) -> PathBuf {
    let name = format!("{prefix}.{date}");
    dir.join(match suffix {
        Some(suffix) => format!("{name}.{suffix}"),
        None => name,
    })
}

impl Write for DailyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.append_at(buf, Local::now().date_naive())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.writer.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// process ごとのファイル上限で、実際の部分 write と EFBIG 後の再送を検証する。
    /// 上限と signal 設定は別 process に閉じ込め、並行テストへ影響させない。
    #[cfg(unix)]
    #[test]
    fn file_size_limit_failure_allows_small_retry_and_restart() {
        const CHILD: &str = "APPEND_LIMIT_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    "ulimit -f 32; trap '' 25; exec \"$1\" \"$2\" --exact --nocapture",
                    "--",
                ])
                .arg(std::env::current_exe().unwrap())
                .arg("storage::tests::file_size_limit_failure_allows_small_retry_and_restart")
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut writer = AppendFile::open(&path).unwrap();
        writer.append(b"{}\n").unwrap();
        assert!(writer.append(&vec![b'x'; 100_000]).is_err());
        writer
            .append(b"{}\n")
            .expect("truncate 後は小さい batch を再送できる");
        assert_eq!(std::fs::read(&path).unwrap(), b"{}\n{}\n");
        // crash を模した末尾は上限位置まで残る。再 open 後も次の行を保存できる。
        assert!(writer.writer.file.write_all(&vec![b'x'; 100_000]).is_err());
        drop(writer);
        AppendFile::open(&path).unwrap().append(b"{}\n").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"{}\n{}\n{}\n");
    }

    struct FailingFile {
        bytes: Vec<u8>,
        fail_after: Option<usize>,
        truncate_fails: bool,
        flush_fails: bool,
    }

    impl Write for FailingFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail_after == Some(0) {
                return Err(io::Error::other("disk full"));
            }
            let n = self.fail_after.unwrap_or(buf.len()).min(buf.len());
            self.bytes.extend_from_slice(&buf[..n]);
            if let Some(left) = self.fail_after.as_mut() {
                *left -= n;
            }
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.flush_fails {
                return Err(io::Error::other("flush failed"));
            }
            Ok(())
        }
    }

    impl AppendStorage for FailingFile {
        fn len(&self) -> io::Result<u64> {
            Ok(self.bytes.len() as u64)
        }
        fn truncate(&mut self, len: u64) -> io::Result<()> {
            if self.truncate_fails {
                return Err(io::Error::other("truncate failed"));
            }
            self.bytes.truncate(len as usize);
            Ok(())
        }
    }

    #[test]
    fn failed_append_rolls_back_before_retry() {
        let line = b"{\"batch\":2}\n";
        for limit in [0, 1, line.len() - 1] {
            let mut writer = TransactionalAppend {
                file: FailingFile {
                    bytes: b"{}\n".to_vec(),
                    fail_after: Some(limit),
                    truncate_fails: false,
                    flush_fails: false,
                },
                rollback: None,
            };
            assert!(writer.append(line).is_err());
            assert_eq!(writer.file.bytes, b"{}\n");
            writer.file.fail_after = None;
            writer.append(line).unwrap();
            assert_eq!(writer.file.bytes, b"{}\n{\"batch\":2}\n");
        }
    }

    #[test]
    fn failed_rollback_blocks_new_ack_until_repaired() {
        let mut writer = TransactionalAppend {
            file: FailingFile {
                bytes: b"{}\n".to_vec(),
                fail_after: Some(1),
                truncate_fails: true,
                flush_fails: false,
            },
            rollback: None,
        };
        assert!(writer.append(b"{\"bad\":1}\n").is_err());
        writer.file.fail_after = None;
        assert!(writer.append(b"{}\n").is_err());
        assert_eq!(writer.file.bytes, b"{}\n{");
        writer.file.truncate_fails = false;
        writer.append(b"{}\n").unwrap();
        assert_eq!(writer.file.bytes, b"{}\n{}\n");
    }

    #[test]
    fn reopening_recovers_only_the_incomplete_tail() {
        let dir = tempfile::tempdir().unwrap();
        for prefix in [b"".as_slice(), b"{}\n".as_slice()] {
            let path = dir.path().join("test.jsonl");
            let mut bytes = prefix.to_vec();
            bytes.extend_from_slice(&vec![b'x'; 20_000]);
            std::fs::write(&path, bytes).unwrap();
            AppendFile::open(&path)
                .unwrap()
                .append(b"{\"ok\":1}\n")
                .unwrap();
            let mut expected = prefix.to_vec();
            expected.extend_from_slice(b"{\"ok\":1}\n");
            assert_eq!(std::fs::read(&path).unwrap(), expected);
        }
    }

    #[test]
    fn flush_failure_rolls_back_the_whole_line() {
        let mut writer = TransactionalAppend {
            file: FailingFile {
                bytes: b"{}\n".to_vec(),
                fail_after: None,
                truncate_fails: false,
                flush_fails: true,
            },
            rollback: None,
        };
        assert!(writer.append(b"{\"batch\":2}\n").is_err());
        assert_eq!(writer.file.bytes, b"{}\n");
        writer.file.flush_fails = false;
        writer.append(b"{}\n").unwrap();
        assert_eq!(writer.file.bytes, b"{}\n{}\n");
    }

    #[test]
    fn rotation_waits_for_previous_day_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let first = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let later = first.succ_opt().unwrap();
        let first_path = daily_path(dir.path(), "otel-logger", None, first);
        let mut writer = DailyWriter::at_date(dir.path(), "otel-logger", None, first).unwrap();
        writer.append_at(b"{}\n", first).unwrap();
        // write 後の truncate 失敗を、読み取り専用 fd で決定的に再現する。
        writer.file.writer.file.write_all(b"partial").unwrap();
        writer.file.writer.rollback = Some(3);
        writer.file.writer.file = File::open(&first_path).unwrap();
        assert!(writer.append_at(b"{}\n", later).is_err());
        assert!(!daily_path(dir.path(), "otel-logger", None, later).exists());
        writer.file.writer.file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&first_path)
            .unwrap();
        writer.append_at(b"{}\n", later).unwrap();
        assert_eq!(std::fs::read(first_path).unwrap(), b"{}\n");
        assert_eq!(
            std::fs::read(daily_path(dir.path(), "otel-logger", None, later)).unwrap(),
            b"{}\n"
        );
    }

    #[test]
    fn daily_writer_uses_current_date_after_idle_and_recovers_its_tail() {
        let dir = tempfile::tempdir().unwrap();
        let first = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let later = NaiveDate::from_ymd_opt(2026, 10, 4).unwrap();
        for (prefix, suffix) in [("otel-logger", None), ("otel-logger.pretty", Some("log"))] {
            let mut writer = DailyWriter::at_date(dir.path(), prefix, suffix, first).unwrap();
            writer.append_at(b"first\n", first).unwrap();
            let later_path = daily_path(dir.path(), prefix, suffix, later);
            std::fs::write(&later_path, b"saved\nincomplete").unwrap();
            writer.append_at(b"later\n", later).unwrap();
            assert_eq!(
                std::fs::read(daily_path(dir.path(), prefix, suffix, first)).unwrap(),
                b"first\n"
            );
            assert_eq!(std::fs::read(later_path).unwrap(), b"saved\nlater\n");
            assert!(!daily_path(dir.path(), prefix, suffix, first.succ_opt().unwrap()).exists());
        }
    }
}
