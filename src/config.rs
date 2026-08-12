use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::ColorMode;
use crate::path::expand_user_path;

/// `otel-logger init` が出力する参照用設定。source tree なしで新しい設定を
/// 生成できるよう、binary に直接埋め込む。
const DEFAULT_CONFIG_TEMPLATE: &str = r#"# otel-logger 設定ファイル
#
# 既定の場所: $XDG_CONFIG_HOME/otel-logger/config.toml
#             (XDG_CONFIG_HOME 未設定時は ~/.config/otel-logger/config.toml)
#
# `otel-logger --config /path/to/config.toml` で上書きできます。
#
# 優先順位: CLI flag > 環境変数 > 設定ファイル > 組み込み既定値。
# 以下の key はすべて任意です。コメントアウトすると既定値に戻ります。

# 受信した OTLP telemetry を欠落のない JSON Lines として保存します。親ディレクトリは
# 起動時に作成され、ファイルは追記モードで開かれ、graceful shutdown 時に fsync されます。
# `log-dir` とは同時に指定できません。
# CLI や環境変数で `log-dir` を指定した場合は、そちらが優先されます。
log-file = "/var/log/otel-logger/otel-logger.jsonl"

# 代替: 日次ローテーション付きでディレクトリへ書き出します
# (ローカル時刻で日ごとの `otel-logger.YYYY-MM-DD`)。古いファイルは起動時に整理され、
# ローテーション時にも `log-keep-days` (既定値 10) の上限が適用されます。
# `log-file` とは同時に指定できません。
# CLI や環境変数で `log-file` を指定した場合は、そちらが優先されます。
# log-dir = "/var/log/otel-logger"
# log-keep-days = 10

# 人が読める stdout 出力を抑止します。JSONL ファイルだけを書き出したい場合に使います。
no-stdout = false

# Claude/Codex の使用量更新時に、累計サマリー (input/output/cache tokens、cost、
# provider/model/effort 別内訳) を stdout へ追記します。HTTP endpoint `GET /stats` は
# このフラグに関係なく常に有効です。
summary = false

# stdout の色設定: "auto" | "always" | "never"。`auto` は NO_COLOR を尊重し、
# stdout が TTY でない場合は ANSI code を出しません。
color = "auto"

# bind address。既定値は 0.0.0.0:4317 (OTLP/gRPC) と 0.0.0.0:4318 (OTLP/HTTP) です。
# grpc-addr = "0.0.0.0:4317"
# http-addr = "0.0.0.0:4318"

# ------------------------------------------------------------
# OTLP proxy 転送 (受信 → JSONL 保存 → 上流 collector へ forward)
# ------------------------------------------------------------
#
# proxy 転送を有効にすると、受信 payload を JSONL に保存しつつ、`service.name` で
# 振り分けた上流 collector にも転送します。JSONL 保存が最優先で、転送失敗は
# 個別の route worker が指数バックオフで retry します。JSONL 出力 (`log-file` or
# `log-dir`) が必須です (両方未設定なら proxy 起動時にエラーになります)。
#
# 組み込み既定 route:
#   - name = "anthropic" → service_names = ["claude-code"]
#   - name = "openai"    → service_names = ["codex_cli_rs", "codex_exec",
#                                           "codex-app-server", "codex_mcp_server"]
#
# CLI 短縮フラグ (`--proxy-anthropic-endpoint` / `--proxy-openai-endpoint`) を使うと
# ここで endpoint を書かずに済みます。config で明示すると CLI/env より低優先です。
#
# [proxy]
# queue-capacity = 1024   # per-route の bounded channel 容量
# timeout-ms = 5000       # 1 request の I/O timeout
# retry-max = 8           # 初回送信後の指数バックオフ最大再試行回数
# checkpoint-dir = "/var/lib/otel-logger/.otel-logger-proxy"  # Phase B 用の予約設定
#
# [[proxy.routes]]
# name = "anthropic"
# # service_names を省略すると組み込み既定を継承します。
# transport = "grpc"                    # "grpc" | "http-protobuf"
# endpoint = "https://collector.example.com:4317"
# [proxy.routes.headers]
# Authorization = "env:ANTHROPIC_PROXY_TOKEN"  # env: 参照で環境変数から解決
#
# [[proxy.routes]]
# name = "openai"
# transport = "http-protobuf"
# endpoint = "https://collector.example.com"
# [proxy.routes.headers]
# Authorization = "env:OPENAI_PROXY_TOKEN"
"#;

/// `~/.config/otel-logger/config.toml` から読む永続設定。
/// すべての field は任意なので、部分的な config でも CLI flags や環境変数と
/// 正しく merge できる。
///
/// 優先順位: CLI flag > 環境変数 > 設定ファイル > 組み込み既定値。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// gRPC bind address (OTLP/gRPC)。
    pub grpc_addr: Option<SocketAddr>,
    /// HTTP bind address (OTLP/HTTP)。
    pub http_addr: Option<SocketAddr>,
    /// 受信した telemetry を JSON Lines としてこのファイルへ保存する。
    /// `log_dir` とは同時に指定できない。
    pub log_file: Option<PathBuf>,
    /// JSON Lines を日次ローテーション付きでこのディレクトリへ保存する。
    /// `log_file` とは同時に指定できない。
    pub log_dir: Option<PathBuf>,
    /// 保持する日次 JSONL ファイル数 (`log_dir` 指定時のみ有効)。
    /// 省略時の既定値は 10。
    pub log_keep_days: Option<u32>,
    /// 人が読める stdout 出力を抑止する。
    pub no_stdout: Option<bool>,
    /// 使用量更新時に stdout へ累計サマリーを追記する。
    pub summary: Option<bool>,
    /// 人が読める stdout 出力の色設定。
    pub color: Option<ColorMode>,
    /// OTLP proxy 転送設定。ここで route を定義すると、受信 payload を JSONL に
    /// 保存しつつ、`service.name` で振り分けた上流 collector にも forward する。
    pub proxy: Option<ProxyConfig>,
}

/// `[proxy]` セクションで指定される proxy 転送全体の設定。個別の route は
/// `[[proxy.routes]]` (or CLI 短縮フラグ) で定義する。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProxyConfig {
    /// 受信 → JSONL 書込み後に配送 worker へ渡す notify channel の bounded 容量。
    /// Phase A では overflow を drop counter に記録する。JSONL からの catch-up は
    /// Phase B で実装するため、通常の burst を吸収できる容量を指定する (既定 1024)。
    pub queue_capacity: Option<usize>,
    /// 1 リクエスト分の I/O timeout (milliseconds)。既定 5000ms。
    pub timeout_ms: Option<u64>,
    /// 送信失敗時の指数バックオフ最大リトライ回数。Phase A では使い切った request を
    /// 自動再送しない。JSONL からの再走査は Phase B で実装する。既定 8。
    pub retry_max: Option<u32>,
    /// Phase B で checkpoint ファイルを置く予約ディレクトリ。未指定時は JSONL 出力先
    /// (`log-file` の親 or `log-dir`) の直下に `.otel-logger-proxy` を解決する。
    pub checkpoint_dir: Option<PathBuf>,
    /// 明示的な route 定義。未指定時は組み込み既定 (anthropic + openai) を使う。
    /// name が同じ route が組み込み既定と config 側に両方あった場合は config が優先する。
    #[serde(default)]
    pub routes: Vec<ProxyRouteConfig>,
}

/// 単一 route (= 送信先 endpoint 1 つ + マッチ条件) の設定。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProxyRouteConfig {
    /// route の識別子。同一 config 内で unique。Phase B の checkpoint ファイル名にも使う。
    pub name: String,
    /// この route にマッチさせる `service.name` の集合。組み込み既定
    /// (anthropic → claude-code、openai → codex 系) がある name はこの field を
    /// 省略すれば既定値を継承する。
    #[serde(default)]
    pub service_names: Vec<String>,
    /// この route で送信する signal 種別 (logs / traces / metrics)。省略時は全種別。
    #[serde(default)]
    pub signal_types: Vec<ProxySignal>,
    /// 転送 protocol。`grpc` (OTLP/gRPC) か `http-protobuf` (OTLP/HTTP protobuf)。
    /// 既定は `grpc`。
    #[serde(default)]
    pub transport: Option<ProxyTransport>,
    /// 送信先の base URL (gRPC は "https://host:4317"、HTTP は "https://host" 相当)。
    pub endpoint: String,
    /// 追加 HTTP header (gRPC でも metadata として送る)。
    /// value に `env:VAR_NAME` を書くと起動時に環境変数を解決する。
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// 転送プロトコル。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProxyTransport {
    /// OTLP/gRPC (tonic client 経由、port 4317 慣例)。
    #[default]
    Grpc,
    /// OTLP/HTTP protobuf (reqwest 経由、port 4318 慣例)。JSON は使わない。
    HttpProtobuf,
}

/// route で扱う OTLP signal 種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProxySignal {
    Logs,
    Traces,
    Metrics,
}

impl ProxySignal {
    pub const ALL: &'static [ProxySignal] = &[Self::Logs, Self::Traces, Self::Metrics];
}

impl Config {
    /// 設定を読み込む。`explicit` が指定された場合、そのファイルは存在しなければならない。
    /// それ以外では XDG の既定パスを参照し、存在しない場合はエラーにせず空設定にする。
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::load_with_home(explicit, home.as_deref())
    }

    fn load_with_home(explicit: Option<&Path>, home: Option<&Path>) -> Result<Self> {
        match explicit {
            Some(path) => {
                let path = expand_user_path(path.to_path_buf(), home);
                if !path.exists() {
                    anyhow::bail!("configuration file does not exist: {}", path.display());
                }
                Self::load_from(&path)
            }
            None => match default_config_path() {
                Some(path) if path.exists() => Self::load_from(&path),
                _ => Ok(Self::default()),
            },
        }
    }

    fn load_from(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("read config file {}", path.display()))?;
        let config: Self =
            toml::from_str(&body).with_context(|| format!("parse TOML in {}", path.display()))?;
        Ok(config)
    }
}

/// 既定の設定パスを解決する。`$XDG_CONFIG_HOME/otel-logger/config.toml` が使えればそれを、
/// そうでなければ `$HOME/.config/otel-logger/config.toml` を返す。両方の環境変数が
/// 未設定の場合だけ `None` を返すが、通常は test でしか起きない。
pub fn default_config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("otel-logger").join("config.toml"));
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("otel-logger")
            .join("config.toml"),
    )
}

/// `write_default` の結果。呼び出し側が分かりやすい message を出すために使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOutcome {
    Created,
    Overwrote,
}

/// 同梱している `DEFAULT_CONFIG_TEMPLATE` を `path` へ書き出す。
/// 必要に応じて親ディレクトリを作成し、`force` 未指定なら既存ファイルの上書きを拒否する。
/// `force=false` の場合は `create_new` で atomic に open するため、`exists()` 後に
/// 別 process が同名ファイルを作っても上書き拒否の約束を破らない。
pub fn write_default(path: &Path, force: bool) -> Result<InitOutcome> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent directory of {}", path.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    // open 直前の `exists()` は TOCTOU を抱えるが、`force=false` の上書き拒否は
    // `create_new` 側で保証されている。ここで見たいのは Overwrote / Created の
    // 報告用シグナルだけで、判定が後追いで race してもファイル状態は壊れない。
    let pre_existed = path.try_exists().unwrap_or(false);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::bail!(
                "{} already exists; pass --force / -f to overwrite",
                path.display()
            );
        }
        Err(e) => {
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("open config file {}", path.display()));
        }
    };
    use std::io::Write as _;
    file.write_all(DEFAULT_CONFIG_TEMPLATE.as_bytes())
        .with_context(|| format!("write config file {}", path.display()))?;
    Ok(if force && pre_existed {
        InitOutcome::Overwrote
    } else {
        InitOutcome::Created
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_explicit_path_returns_error_when_missing() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("missing.toml");
        let err = Config::load(Some(&missing)).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn load_explicit_path_parses_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
grpc-addr = "127.0.0.1:14317"
http-addr = "127.0.0.1:14318"
log-file = "/tmp/test.jsonl"
no-stdout = true
summary = true
color = "never"
"#,
        )
        .unwrap();
        let config = Config::load(Some(&path)).unwrap();
        assert_eq!(config.grpc_addr.unwrap().to_string(), "127.0.0.1:14317");
        assert_eq!(config.http_addr.unwrap().to_string(), "127.0.0.1:14318");
        assert_eq!(config.log_file.unwrap(), PathBuf::from("/tmp/test.jsonl"));
        assert_eq!(config.no_stdout, Some(true));
        assert_eq!(config.summary, Some(true));
        assert_eq!(config.color, Some(crate::cli::ColorMode::Never));
    }

    #[test]
    fn load_explicit_path_expands_leading_tilde() {
        let dir = TempDir::new().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.toml"), "summary = true\n").unwrap();

        let config =
            Config::load_with_home(Some(Path::new("~/config/config.toml")), Some(dir.path()))
                .unwrap();

        assert_eq!(config.summary, Some(true));
    }

    #[test]
    fn load_rejects_unknown_fields() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "unknown-key = true\n").unwrap();
        let err = Config::load(Some(&path)).unwrap_err();
        assert!(err.to_string().contains("parse TOML"));
    }

    #[test]
    fn write_default_creates_file_and_refuses_overwrite() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        // 親ディレクトリも合わせて作成する。
        let outcome = write_default(&path, false).unwrap();
        assert_eq!(outcome, InitOutcome::Created);
        assert!(path.exists());
        // force なしで再書き込みは失敗する。
        let err = write_default(&path, false).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        // force ありなら overwrite を返す。
        let outcome = write_default(&path, true).unwrap();
        assert_eq!(outcome, InitOutcome::Overwrote);
    }

    #[test]
    fn write_default_content_is_loadable() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        write_default(&path, false).unwrap();
        // 生成したテンプレートが Config として往復解析可能であることを確認する。
        Config::load(Some(&path)).unwrap();
    }

    #[test]
    fn load_parses_proxy_section() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
log-file = "/tmp/otel.jsonl"

[proxy]
queue-capacity = 512
timeout-ms = 3000
retry-max = 5

[[proxy.routes]]
name = "anthropic"
transport = "grpc"
endpoint = "https://collector.example:4317"
[proxy.routes.headers]
Authorization = "env:MY_TOKEN"

[[proxy.routes]]
name = "openai"
service-names = ["codex_cli_rs", "codex_exec"]
signal-types = ["logs", "traces"]
transport = "http-protobuf"
endpoint = "https://collector.example"
"#,
        )
        .unwrap();

        let config = Config::load(Some(&path)).unwrap();
        let proxy = config.proxy.expect("proxy section parsed");
        assert_eq!(proxy.queue_capacity, Some(512));
        assert_eq!(proxy.timeout_ms, Some(3000));
        assert_eq!(proxy.retry_max, Some(5));
        assert_eq!(proxy.routes.len(), 2);

        let anthropic = &proxy.routes[0];
        assert_eq!(anthropic.name, "anthropic");
        assert!(anthropic.service_names.is_empty());
        assert_eq!(anthropic.transport, Some(ProxyTransport::Grpc));
        assert_eq!(anthropic.endpoint, "https://collector.example:4317");
        assert_eq!(
            anthropic.headers.get("Authorization"),
            Some(&"env:MY_TOKEN".to_string())
        );

        let openai = &proxy.routes[1];
        assert_eq!(openai.name, "openai");
        assert_eq!(
            openai.service_names,
            vec!["codex_cli_rs".to_string(), "codex_exec".to_string()]
        );
        assert_eq!(
            openai.signal_types,
            vec![ProxySignal::Logs, ProxySignal::Traces]
        );
        assert_eq!(openai.transport, Some(ProxyTransport::HttpProtobuf));
    }

    #[test]
    fn load_rejects_unknown_field_in_proxy_route() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[[proxy.routes]]
name = "x"
endpoint = "https://x"
mystery-key = 1
"#,
        )
        .unwrap();
        let err = Config::load(Some(&path)).unwrap_err();
        assert!(err.to_string().contains("parse TOML"));
    }
}
