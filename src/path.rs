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
    let Some(s) = path.to_str() else {
        return path;
    };
    if s == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = s.strip_prefix("~/") {
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
