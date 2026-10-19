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

/// 他ユーザーから辿れないディレクトリ (Unix では `0o700`) を作る。
///
/// JSONL には受信した telemetry がそのまま入り、`user.email` / `user.id` /
/// `organization.id` などが含まれる (`OTEL_LOG_USER_PROMPTS` 有効時はプロンプト本文も)。
/// 設定ファイルには proxy の認証ヘッダを書ける。既定の umask 022 では 0755 / 0644 で
/// 作られ、共有 CI runner や多人数ログインサーバでは同一ホストの全ユーザーが読めてしまう。
///
/// 既存ディレクトリの mode は変更しない。運用中に権限を書き換えると、意図して
/// group 共有にしている構成を壊すため。
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// 新規作成時の mode を `0o600` に制限する。既存ファイルの mode は変えない。
#[cfg(unix)]
pub fn restrict_new_file_mode(options: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
pub fn restrict_new_file_mode(_options: &mut std::fs::OpenOptions) {}

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

    /// `~/` だけ・区切りの重複・先頭が `./` のパスの境界。`~` は先頭の component の
    /// ときだけ展開し、`./~/x` (CWD 直下の `~` を明示した指定) は展開しない。
    #[test]
    fn expand_user_path_handles_separator_edge_cases() {
        let home = PathBuf::from("/Users/alice");
        assert_eq!(
            expand_user_path(PathBuf::from("~/"), Some(&home)),
            PathBuf::from("/Users/alice"),
        );
        assert_eq!(
            expand_user_path(PathBuf::from("~//x"), Some(&home)),
            PathBuf::from("/Users/alice/x"),
        );
        assert_eq!(
            expand_user_path(PathBuf::from("~/~"), Some(&home)),
            PathBuf::from("/Users/alice/~"),
            "展開は先頭の 1 つだけ"
        );
        assert_eq!(
            expand_user_path(PathBuf::from("./~/x"), Some(&home)),
            PathBuf::from("./~/x"),
        );
        assert_eq!(
            expand_user_path(PathBuf::new(), Some(&home)),
            PathBuf::new()
        );
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// 回帰テスト: 途中の階層も含め、新しく作るディレクトリは所有者だけが辿れる権限にする。
    ///
    /// JSONL には受信した telemetry (`user.email` やプロンプト本文を含みうる) がそのまま入る。
    /// 既定の umask 022 で作ると 0755 になり、共有 CI runner や多人数ログインサーバの
    /// 他ユーザーから読める。umask は権限を減らす方向にしか効かないので、group / other の
    /// bit が立っていないことを確かめる。
    #[cfg(unix)]
    #[test]
    fn create_private_dir_creates_every_missing_level_for_the_owner_only() {
        let root = tempfile::tempdir().unwrap();
        let leaf = root.path().join("a").join("b").join("c");
        create_private_dir(&leaf).unwrap();
        for dir in [
            root.path().join("a"),
            root.path().join("a").join("b"),
            leaf.clone(),
        ] {
            let mode = mode_of(&dir);
            assert_eq!(mode & 0o077, 0, "{} が {mode:o}", dir.display());
            assert_eq!(mode & 0o700, 0o700, "所有者は読み書き・走査できる");
        }
        // 既にあるディレクトリでは何もせず成功する (起動のたびに呼ぶため)。
        create_private_dir(&leaf).unwrap();
    }

    /// 既存ディレクトリの mode は変えない。意図して group 共有にしている構成を
    /// 起動のたびに壊さないため。
    #[cfg(unix)]
    #[test]
    fn create_private_dir_keeps_the_mode_of_an_existing_directory() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o750)).unwrap();

        create_private_dir(&shared).unwrap();
        create_private_dir(&shared.join("child")).unwrap();

        assert_eq!(mode_of(&shared), 0o750);
        assert_eq!(mode_of(&shared.join("child")) & 0o077, 0);
    }

    /// 同じ名前の通常ファイルがある場所はディレクトリとして使えないので、成功扱いにせず
    /// エラーを返す (`log-dir` の取り違えを起動時に止める)。
    #[test]
    fn create_private_dir_fails_when_a_file_is_in_the_way() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        assert!(create_private_dir(&file).is_err());
        assert!(create_private_dir(&file.join("child")).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"x", "ファイルは壊さない");
    }

    /// 新しく作るファイルだけを所有者限定 (0600) にし、既存ファイルの mode は変えない。
    #[cfg(unix)]
    #[test]
    fn restrict_new_file_mode_applies_only_to_newly_created_files() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let open = |path: &Path| {
            let mut options = std::fs::OpenOptions::new();
            options.create(true).append(true);
            restrict_new_file_mode(&mut options);
            options.open(path).unwrap()
        };

        let created = root.path().join("created.jsonl");
        drop(open(&created));
        assert_eq!(mode_of(&created) & 0o077, 0);
        assert_eq!(mode_of(&created) & 0o600, 0o600);

        let existing = root.path().join("existing.jsonl");
        std::fs::write(&existing, b"{}\n").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o640)).unwrap();
        drop(open(&existing));
        assert_eq!(mode_of(&existing), 0o640);
    }
}
