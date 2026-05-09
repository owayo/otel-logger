use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::ColorMode;

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
log-file = "/var/log/otel-logger/otel-logger.jsonl"

# 代替: 日次ローテーション付きでディレクトリへ書き出します
# (ローカル時刻で日ごとの `otel-logger.YYYY-MM-DD`)。古いファイルは起動時に整理され、
# ローテーション時にも `log-keep-days` (既定値 10) の上限が適用されます。
# `log-file` とは同時に指定できません。
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
}

impl Config {
    /// 設定を読み込む。`explicit` が指定された場合、そのファイルは存在しなければならない。
    /// それ以外では XDG の既定パスを参照し、存在しない場合はエラーにせず空設定にする。
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        match explicit {
            Some(path) => {
                if !path.exists() {
                    anyhow::bail!("configuration file does not exist: {}", path.display());
                }
                Self::load_from(path)
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
pub fn write_default(path: &Path, force: bool) -> Result<InitOutcome> {
    let already_exists = path.exists();
    if already_exists && !force {
        anyhow::bail!(
            "{} already exists; pass --force / -f to overwrite",
            path.display()
        );
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent directory of {}", path.display()))?;
    }
    std::fs::write(path, DEFAULT_CONFIG_TEMPLATE)
        .with_context(|| format!("write config file {}", path.display()))?;
    Ok(if already_exists {
        InitOutcome::Overwrote
    } else {
        InitOutcome::Created
    })
}
