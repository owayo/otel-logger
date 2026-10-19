//! 受信の上限と期限 (`--max-inflight-requests` / `--receive-timeout-ms`) を、実際の
//! バイナリと TCP 接続で確かめる。
//!
//! gzip の body は展開後に最大 32MiB まで溜まるため、終端を送らずに止めた接続を並べる
//! だけで、数 KB の送信から数百 MB を抱えさせられた。受け付けからの期限で打ち切り、
//! retryable な 503 を返して枠を戻す。

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// 子プロセスの起動と応答を待つ上限。CI の遅いランナーでも余裕を持たせる。
const DEADLINE: Duration = Duration::from_secs(20);

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("bind an ephemeral port")
}

fn spawn(config: &Path, args: &[&str]) -> io::Result<Child> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_otel-logger"));
    // 開発者や CI の環境変数で出力先・proxy・上限が変わらないようにする。
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("OTEL_LOGGER_") {
            command.env_remove(key);
        }
    }
    command
        .arg("--config")
        .arg(config)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn wait_until_listening(child: &mut Child, port: u16) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            panic!("起動中に終了した: {status}");
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "listen しなかった");
        thread::sleep(Duration::from_millis(20));
    }
}

/// 応答の status code を読む (応答の先頭行だけを見る)。
fn read_status(stream: &mut TcpStream) -> io::Result<u16> {
    stream.set_read_timeout(Some(DEADLINE))?;
    let mut response = Vec::new();
    let mut buf = [0u8; 1024];
    while !response.windows(2).any(|w| w == b"\r\n") {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        response.extend_from_slice(&buf[..n]);
    }
    let response = String::from_utf8_lossy(&response);
    response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| io::Error::other(format!("unexpected response: {response:?}")))
}

/// 回帰テスト: gzip の body を送り切らない接続は、受け付けからの期限で retryable な 503 を
/// 受け取り、その枠は次の request に使える。修正前は応答が返らず、枠という考え方も無いので
/// 同じ接続を並べるだけで展開後の body を際限なく抱えた。
#[test]
fn stalled_gzip_upload_gets_503_and_frees_its_slot() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "").unwrap();
    let jsonl = dir.path().join("otel-logger.jsonl");
    let http_port = free_port();
    let grpc_addr = format!("127.0.0.1:{}", free_port());
    let http_addr = format!("127.0.0.1:{http_port}");
    let mut child = spawn(
        &config,
        &[
            "--no-stdout",
            "--log-file",
            jsonl.to_str().unwrap(),
            "--grpc-addr",
            &grpc_addr,
            "--http-addr",
            &http_addr,
            "--max-inflight-requests",
            "1",
            "--receive-timeout-ms",
            "300",
        ],
    )
    .unwrap();
    wait_until_listening(&mut child, http_port);

    // gzip の先頭 (10 byte の header と deflate の一部) だけを chunked で送り、終端を送らない。
    let mut stalled = TcpStream::connect(("127.0.0.1", http_port)).unwrap();
    let partial_gzip = [0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff, 0xed, 0xc1];
    write!(
        stalled,
        "POST /v1/logs HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/x-protobuf\r\n\
         Content-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
        partial_gzip.len()
    )
    .unwrap();
    stalled.write_all(&partial_gzip).unwrap();
    stalled.write_all(b"\r\n").unwrap();
    let stalled_status = read_status(&mut stalled);

    // 枠が戻っていれば、次の request は上限 1 でも通る。
    let body = r#"{"resourceLogs":[]}"#;
    let mut next = TcpStream::connect(("127.0.0.1", http_port)).unwrap();
    write!(
        next,
        "POST /v1/logs HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let next_status = read_status(&mut next);
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(
        stalled_status.unwrap(),
        503,
        "保存前に retryable な status で断る"
    );
    assert_eq!(next_status.unwrap(), 200, "期限で打ち切った枠は戻る");
}
