use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// 先頭の `~` / `~/` を現在ユーザーの `$HOME` に展開する。
///
/// CLI 引数は shell が展開することが多いが、環境変数や TOML 設定値は展開されない。
/// `~user/...` 形式は OS ごとのユーザー DB 解決が必要になるため、現状はそのまま返す。
pub fn expand_current_user_path(path: PathBuf) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    expand_user_path(path, home.as_deref())
}

pub(crate) fn expand_user_path(path: PathBuf, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return path;
    };
    // `HOME=""` (空文字) は未設定と同等に扱う。空 home のまま `~/x` を join すると
    // `x` (プロセス CWD 相対) に化け、`~` 単独では空パスになって open に失敗する。
    // `default_config_path` 側が `filter(|v| !v.is_empty())` で空 HOME を弾くのと挙動を揃える。
    // 空文字 HOME は cron / systemd unit / コンテナ等で実際に発生しうる正当な POSIX 状態。
    if home.as_os_str().is_empty() {
        return path;
    }
    // `path.to_str()` に頼ると、非 UTF-8 バイトを含むパス (Unix の `OsStr`) では
    // `to_str()` が `None` を返して `~` 展開が無音で抜け、ドキュメントの契約に反して
    // CWD 直下へ `~` ディレクトリを作ってしまう。`OsStr` のまま component 単位で判定し、
    // 非 UTF-8 パスでも先頭 `~` / `~/` を展開する。
    if path.as_os_str() == OsStr::new("~") {
        return home.to_path_buf();
    }
    if let Ok(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_user_path_replaces_leading_tilde_with_home() {
        let home = PathBuf::from("/Users/alice");
        assert_eq!(
            expand_user_path(PathBuf::from("~"), Some(&home)),
            PathBuf::from("/Users/alice"),
        );
        assert_eq!(
            expand_user_path(PathBuf::from("~/tmp"), Some(&home)),
            PathBuf::from("/Users/alice/tmp"),
        );
        assert_eq!(
            expand_user_path(PathBuf::from("~/a/b/c"), Some(&home)),
            PathBuf::from("/Users/alice/a/b/c"),
        );
    }

    #[test]
    fn expand_user_path_leaves_non_tilde_paths_untouched() {
        let home = PathBuf::from("/Users/alice");
        for raw in ["/var/log/otel", "tmp/foo", "/opt/~/cache", "~bob/tmp"] {
            assert_eq!(
                expand_user_path(PathBuf::from(raw), Some(&home)),
                PathBuf::from(raw),
                "展開してはいけないパス: {raw}",
            );
        }
    }

    #[test]
    fn expand_user_path_returns_input_when_home_is_unset() {
        assert_eq!(
            expand_user_path(PathBuf::from("~/tmp"), None),
            PathBuf::from("~/tmp"),
        );
    }

    #[cfg(unix)]
    #[test]
    fn expand_user_path_expands_non_utf8_paths() {
        // 非 UTF-8 バイト (0xFF) を含む `~/...` でも展開する。`to_str()` ベースの実装では
        // ここで展開が抜け、CWD 直下に `~` を作ってしまっていた (回帰防止)。
        use std::os::unix::ffi::OsStrExt;
        let home = PathBuf::from("/Users/alice");
        let raw = PathBuf::from(OsStr::from_bytes(&[b'~', b'/', b'x', 0xFF]));
        let expected = home.join(OsStr::from_bytes(&[b'x', 0xFF]));
        assert_eq!(expand_user_path(raw, Some(&home)), expected);
    }

    #[test]
    fn expand_user_path_treats_empty_home_as_unset() {
        // `HOME=""` は未設定と同等に扱い、`~/x` を CWD 相対パスへ化けさせない。
        // 呼び出し元 (`expand_current_user_path` / config / cli) は空チェック無しで
        // `Some(PathBuf::from(""))` を渡しうるため、関数側で防ぐ。
        let empty = PathBuf::new();
        assert_eq!(
            expand_user_path(PathBuf::from("~/tmp"), Some(&empty)),
            PathBuf::from("~/tmp"),
        );
        assert_eq!(
            expand_user_path(PathBuf::from("~"), Some(&empty)),
            PathBuf::from("~"),
        );
    }
}
