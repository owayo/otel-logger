//! `otel-logger init` が、受信側の読む設定ファイルへ書くことを確かめる。
//!
//! `--config` は global なので `init --help` にも出る。修正前の `init` は `--path` しか見ずに
//! XDG の既定パスへ書いていたため、`init --config <別のファイル> -f` は指定したファイルを
//! 作らないまま既定パスの設定を雛形で上書きし、`-n init -f` も上書きしていた。
//! `OTEL_LOGGER_CONFIG` を設定した環境では、既定パスに作った後の起動が
//! 「configuration file does not exist」で失敗した。
//!
//! 利用者の設定を読ませない・書かせないよう、HOME と XDG_CONFIG_HOME を一時ディレクトリに
//! 向け、既定パスに目印付きの設定を置いてから子プロセスとして起動する。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// 既定パスに置く利用者の設定。雛形で上書きされたら分かるよう目印を入れる。
const MARKED_CONFIG: &str =
    "# marker: written by the user, not by `otel-logger init`\nsummary = true\n";

/// HOME と XDG_CONFIG_HOME を一時ディレクトリに向けた実行環境。
struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    /// 既定パスに目印付きの設定を置いた状態で作る。
    fn new() -> Self {
        let sandbox = Self {
            root: tempfile::tempdir().unwrap(),
        };
        let default = sandbox.default_config();
        std::fs::create_dir_all(default.parent().unwrap()).unwrap();
        std::fs::write(&default, MARKED_CONFIG).unwrap();
        sandbox
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn home(&self) -> PathBuf {
        self.path("home")
    }

    /// `$HOME/.config` と同じ場所にし、どちらの環境変数で解決しても同じファイルを指させる。
    fn xdg_config_home(&self) -> PathBuf {
        self.home().join(".config")
    }

    /// 受信側が `--config` / `OTEL_LOGGER_CONFIG` なしで読む既定パス。
    fn default_config(&self) -> PathBuf {
        self.xdg_config_home()
            .join("otel-logger")
            .join("config.toml")
    }

    fn default_config_body(&self) -> String {
        std::fs::read_to_string(self.default_config()).unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_otel-logger"));
        // 開発者や CI の環境変数 (`OTEL_LOGGER_CONFIG` など) で出力先が変わらないようにする。
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("OTEL_LOGGER_") {
                command.env_remove(key);
            }
        }
        command
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.xdg_config_home())
            // 相対パスの解決先も一時ディレクトリに閉じ込める。
            .current_dir(self.root.path())
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .output()
            .expect("otel-logger を起動できること")
    }

    /// 一時ディレクトリ配下のディレクトリ (`None`) とファイル (中身) の一覧。
    /// 「何も書かない」ことの確認に使う。
    fn snapshot(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn walk(dir: &Path, entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    entries.insert(path.clone(), None);
                    walk(&path, entries);
                } else {
                    let body = std::fs::read(&path).unwrap();
                    entries.insert(path, Some(body));
                }
            }
        }
        let mut entries = BTreeMap::new();
        walk(self.root.path(), &mut entries);
        entries
    }
}

fn utf8(path: &Path) -> &str {
    path.to_str().expect("一時ディレクトリのパスは UTF-8")
}

fn assert_success(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} が失敗した: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `init` が書いた雛形か (利用者の設定ではないか)。
fn assert_template(path: &Path) {
    let body = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{} に雛形が無い: {e}", path.display()));
    assert!(
        body.starts_with("# otel-logger"),
        "{}: {body}",
        path.display()
    );
}

/// 回帰テスト: `init --config <PATH>` は受信側が読むそのパスへ書き、既定パスの設定に触れない。
/// `--config` は global なので、サブコマンドの前後どちらに置いても同じ扱いにする。
#[test]
fn init_writes_to_the_config_path_and_keeps_the_default_config() {
    let sandbox = Sandbox::new();
    let after = sandbox.path("after-subcommand.toml");
    let before = sandbox.path("before-subcommand.toml");

    for args in [
        ["init", "--config", utf8(&after), "-f"],
        ["--config", utf8(&before), "init", "-f"],
    ] {
        assert_success(&sandbox.run(&args), &format!("{args:?}"));
    }

    assert_template(&after);
    assert_template(&before);
    assert_eq!(
        sandbox.default_config_body(),
        MARKED_CONFIG,
        "既定パスの設定を雛形で上書きしない"
    );
}

/// 回帰テスト: `OTEL_LOGGER_CONFIG` を設定した環境の `init` は、そのパスへ書く。
/// 修正前は既定パスへ書き、同じ環境での起動が「configuration file does not exist」で失敗した。
#[test]
fn init_writes_to_otel_logger_config_and_the_receiver_reads_it() {
    let sandbox = Sandbox::new();
    let env_config = sandbox.path("from-env.toml");

    let output = sandbox
        .command()
        .env("OTEL_LOGGER_CONFIG", &env_config)
        .arg("init")
        .output()
        .unwrap();
    assert_success(&output, "OTEL_LOGGER_CONFIG を設定した init");
    assert_template(&env_config);
    assert_eq!(sandbox.default_config_body(), MARKED_CONFIG);

    // 同じ環境の受信側は、生成したファイルを読んで起動検証に成功する。雛形の既定の
    // 出力先 (/var/log) と bind (0.0.0.0) は、一時ディレクトリと ephemeral port で上書きする。
    let logs = sandbox.path("logs");
    let output = sandbox
        .command()
        .env("OTEL_LOGGER_CONFIG", &env_config)
        .args([
            "--dry-run",
            "--log-dir",
            utf8(&logs),
            "--grpc-addr",
            "127.0.0.1:0",
            "--http-addr",
            "127.0.0.1:0",
        ])
        .output()
        .unwrap();
    assert_success(&output, "init の後の起動検証");
}

/// 回帰テスト: `--dry-run` と `init` の組み合わせは、何も書かずに失敗する。
/// 修正前は `-n` を無視して、既定パスの設定を雛形で上書きしていた。
#[test]
fn init_with_dry_run_fails_without_writing_anything() {
    let sandbox = Sandbox::new();
    let new_config = sandbox.path("new.toml");
    let before = sandbox.snapshot();

    for args in [
        vec!["-n", "init", "-f"],
        vec!["--dry-run", "init", "--config", utf8(&new_config)],
        vec!["--dry-run", "init", "--path", utf8(&new_config), "--daemon"],
    ] {
        let output = sandbox.run(&args);
        assert!(!output.status.success(), "{args:?} は失敗すること");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--dry-run"),
            "{args:?}: 失敗の理由に --dry-run を挙げる: {stderr}"
        );
        assert_eq!(sandbox.snapshot(), before, "{args:?} は何も書かないこと");
    }
}

/// `--path` は `init` 専用の出力先なので `--config` より優先する。どちらも無ければ既定パスへ書き、
/// 既存の設定の上書きには従来どおり `--force` が要る。
#[test]
fn init_path_wins_over_config_and_falls_back_to_the_default_path() {
    let sandbox = Sandbox::new();
    let explicit = sandbox.path("explicit.toml");
    let receiver = sandbox.path("receiver.toml");

    let output = sandbox.run(&[
        "init",
        "--path",
        utf8(&explicit),
        "--config",
        utf8(&receiver),
    ]);
    assert_success(&output, "--path と --config を両方指定した init");
    assert_template(&explicit);
    assert!(
        !receiver.exists(),
        "--path があれば --config のパスには書かない"
    );
    assert_eq!(sandbox.default_config_body(), MARKED_CONFIG);

    let output = sandbox.run(&["init"]);
    assert!(
        !output.status.success(),
        "既存の既定パスは --force 無しでは上書きしない"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("already exists"));
    assert_eq!(sandbox.default_config_body(), MARKED_CONFIG);

    assert_success(&sandbox.run(&["init", "-f"]), "既定パスへの init -f");
    assert_template(&sandbox.default_config());
}
