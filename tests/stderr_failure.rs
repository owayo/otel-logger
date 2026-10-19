//! stderr へ書けない状態でも、受信・永続化・終了処理が止まらないことを確かめる。
//!
//! 常駐時の stderr はファイルや pipe にリダイレクトされる。ディスク満杯 (ENOSPC) や
//! 読み手の消えた pipe (EPIPE) で診断ログが書けなくなっても、tracing の書き込み失敗が
//! panic に化けてはいけない。実機では、ディスク満杯の間に pretty-log の writer が止まり、
//! 保存済みの batch に 503 を返し、SIGTERM が無視されて SIGKILL でしか止められなくなった。
//! ここでは閉じた pipe を stderr にして同じ失敗を決定的に起こす。

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// 子プロセスの起動・終了を待つ上限。CI の遅いランナーでも余裕を持たせる。
const DEADLINE: Duration = Duration::from_secs(20);

/// CUMULATIVE temporality の Claude token metric。受け付けるが集計はせず、1 batch ごとに
/// WARN を 1 行出す (JSONL へ保存した後に診断ログを書く経路を通す)。
const CUMULATIVE_METRICS: &str = r#"{"resourceMetrics":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"claude-code"}}]},"scopeMetrics":[{"scope":{"name":"test"},"metrics":[{"name":"claude_code.token.usage","sum":{"aggregationTemporality":2,"isMonotonic":true,"dataPoints":[{"asInt":"1"}]}}]}]}]}"#;

/// 読み出し側を閉じた pipe を stderr にして otel-logger を起動する。
fn spawn_with_closed_stderr(config: &Path, args: &[&str]) -> io::Result<Child> {
    let (reader, writer) = io::pipe()?;
    drop(reader);
    spawn_with_stderr(config, args, writer)
}

/// 指定した stderr で otel-logger を起動する。
fn spawn_with_stderr(config: &Path, args: &[&str], stderr: impl Into<Stdio>) -> io::Result<Child> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otel-logger"));
    // 開発者や CI の環境変数で出力先・proxy・ログの水準が変わらないようにする。
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("OTEL_LOGGER_") {
            command.env_remove(key);
        }
    }
    command
        // 診断ログを必ず出させる (`OTEL_LOGGER_LOG=off` の継承で素通りさせない)。
        .env("OTEL_LOGGER_LOG", "info")
        .arg("--config")
        .arg(config)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
}

fn empty_config(dir: &Path) -> io::Result<std::path::PathBuf> {
    let config = dir.join("config.toml");
    std::fs::write(&config, "")?;
    Ok(config)
}

/// 空いている TCP port を 1 つ選ぶ。閉じてから子プロセスが bind するまでの間に
/// 他へ取られる可能性は残るが、テストでは十分に低い。
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("bind an ephemeral port")
}

/// 子プロセスの終了を `DEADLINE` まで待つ。間に合わなければ止めて失敗にする。
fn wait_with_deadline(child: &mut Child, what: &str) -> ExitStatus {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what}: {} 秒以内に終了しなかった", DEADLINE.as_secs());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// HTTP listener が接続を受け付けるまで待つ。先に終了したら、その終了状態で失敗にする。
fn wait_until_listening(child: &mut Child, port: u16) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            panic!("stderr が書けないだけで起動中に終了した: {status}");
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} 秒以内に listen しなかった",
            DEADLINE.as_secs()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// OTLP/HTTP の JSON を POST し、status code を返す。
fn post_json(port: u16, path: &str, body: &str) -> io::Result<u16> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(DEADLINE))?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| io::Error::other(format!("unexpected response: {response:?}")))
}

/// 回帰テスト: 起動時の診断ログが書けなくても、dry-run を最後まで実行して成功で終わる。
/// 修正前は最初の WARN / INFO で panic し、exit 101 で終わっていた。
#[test]
fn dry_run_succeeds_when_stderr_is_a_closed_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let config = empty_config(dir.path()).unwrap();
    // stdout を端末でない null にし、人が読める出力も有効のままにする。起動時の
    // 「stdout がローテーションされない」WARN と dry-run 完了の INFO の両方を通す。
    let mut child = spawn_with_closed_stderr(
        &config,
        &[
            "--dry-run",
            "--grpc-addr",
            "127.0.0.1:0",
            "--http-addr",
            "127.0.0.1:0",
        ],
    )
    .unwrap();
    let status = wait_with_deadline(&mut child, "dry run");
    assert!(status.success(), "dry run が失敗した: {status}");
}

/// 回帰テスト: JSONL へ保存した後で診断ログが書けなくても、batch に成功を返す。
///
/// 修正前は保存後の WARN で受信処理の task が panic し、保存済みの batch に 503 を返して
/// いた。OTLP exporter は 503 を再送するので、同じ payload が JSONL に重複する。
#[test]
fn batch_is_acknowledged_once_when_stderr_is_a_closed_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let config = empty_config(dir.path()).unwrap();
    let jsonl = dir.path().join("otel-logger.jsonl");
    let http_port = free_port();
    let grpc_addr = format!("127.0.0.1:{}", free_port());
    let http_addr = format!("127.0.0.1:{http_port}");
    let mut child = spawn_with_closed_stderr(
        &config,
        &[
            "--no-stdout",
            "--log-file",
            jsonl.to_str().unwrap(),
            "--grpc-addr",
            &grpc_addr,
            "--http-addr",
            &http_addr,
        ],
    )
    .unwrap();
    wait_until_listening(&mut child, http_port);

    let status = post_json(http_port, "/v1/metrics", CUMULATIVE_METRICS);
    let lines = std::fs::read_to_string(&jsonl).map(|text| text.lines().count());
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(
        status.unwrap(),
        200,
        "保存済みの batch に失敗を返すと再送で重複する"
    );
    assert_eq!(lines.unwrap(), 1);
}

/// 回帰テスト: 読み手が読まない stderr でも、診断ログの書き込みで ACK を止めない。
///
/// `2>&1 | less` でスクロールを止めた場合や、Docker / systemd の log driver が詰まった場合、
/// stderr の pipe は満杯のまま読まれない。修正前は JSONL へ保存した後の WARN が write(2) で
/// 止まり、応答が返らなかった (exporter は timeout して再送し、JSONL に重複して書く)。
#[test]
fn ack_is_not_blocked_by_a_stalled_stderr_pipe() {
    /// 1 batch ごとに WARN が 1 行 (約 130 byte) 出る。pipe の容量 (Linux / macOS は 64KiB、
    /// Windows はそれ以下) を十分に超える数を送る。
    const BATCHES: usize = 2000;

    let dir = tempfile::tempdir().unwrap();
    let config = empty_config(dir.path()).unwrap();
    let jsonl = dir.path().join("otel-logger.jsonl");
    let http_port = free_port();
    let grpc_addr = format!("127.0.0.1:{}", free_port());
    let http_addr = format!("127.0.0.1:{http_port}");
    // 読み出し側を持ったまま、読まない。
    let (stalled_reader, writer) = io::pipe().unwrap();
    let mut child = spawn_with_stderr(
        &config,
        &[
            "--no-stdout",
            "--log-file",
            jsonl.to_str().unwrap(),
            "--grpc-addr",
            &grpc_addr,
            "--http-addr",
            &http_addr,
        ],
        writer,
    )
    .unwrap();
    wait_until_listening(&mut child, http_port);

    let mut failure = None;
    for sent in 0..BATCHES {
        match post_json(http_port, "/v1/metrics", CUMULATIVE_METRICS) {
            Ok(200) => {}
            other => {
                failure = Some(format!("{sent} 件目で {other:?}"));
                break;
            }
        }
    }
    let lines = std::fs::read_to_string(&jsonl).map(|text| text.lines().count());
    let _ = child.kill();
    let _ = child.wait();
    drop(stalled_reader);

    assert_eq!(failure, None, "stderr が詰まっても応答を返し続ける");
    assert_eq!(lines.unwrap(), BATCHES);
}

/// 回帰テスト: 診断ログが書けなくても SIGTERM で終了する。
///
/// tokio はシグナルの既定動作をプロセス全体で置き換えるため、シグナルを待つ task が
/// 「received SIGTERM」のログで落ちると、以後の SIGTERM / SIGINT は誰にも届かない。
/// 修正前は SIGKILL でしか止められず、最終 fsync も行われなかった。
#[cfg(unix)]
#[test]
fn sigterm_shuts_down_when_stderr_is_a_closed_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let config = empty_config(dir.path()).unwrap();
    let jsonl = dir.path().join("otel-logger.jsonl");
    let http_port = free_port();
    let grpc_addr = format!("127.0.0.1:{}", free_port());
    let http_addr = format!("127.0.0.1:{http_port}");
    let mut child = spawn_with_closed_stderr(
        &config,
        &[
            "--no-stdout",
            "--log-file",
            jsonl.to_str().unwrap(),
            "--grpc-addr",
            &grpc_addr,
            "--http-addr",
            &http_addr,
        ],
    )
    .unwrap();
    wait_until_listening(&mut child, http_port);

    let sent = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("run kill");
    assert!(sent.success(), "SIGTERM を送れなかった");
    let status = wait_with_deadline(&mut child, "SIGTERM 後の終了");
    assert!(status.success(), "SIGTERM で正常終了しなかった: {status}");
}
