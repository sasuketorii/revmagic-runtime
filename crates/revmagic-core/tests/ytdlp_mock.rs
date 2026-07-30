//! mock yt-dlp 統合テスト (実ネットワーク不要)。
//!
//! PATH に「引数と cookie file の内容/パーミッションをダンプする fake yt-dlp」を
//! 置き、`extract_audio_via_ytdlp` を実際に回す。検証点:
//! - yt-dlp に渡る argv (referer / user-agent / extract-audio / audio-format /
//!   audio-quality / output テンプレート / --cookies のパス)。
//! - `--cookies` に渡したファイルが Netscape 形式かつ 0600。
//! - 処理完了後に cookie 一時ファイルが削除されている。
//! - 直リンク / 埋め込みの振り分け (`route_for_url`)。

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use revmagic_core::{
    extract_audio_via_ytdlp, route_for_url, BinaryResolver, MediaRoute, Progress, YtDlpOptions,
};
use url::Url;

/// 解決先を 1 つの実体パスに固定する resolver (fake yt-dlp を指す)。
struct FixedResolver {
    ytdlp: PathBuf,
}

impl BinaryResolver for FixedResolver {
    fn resolve(&self, binary: &str) -> Result<PathBuf, String> {
        if binary == "yt-dlp" {
            Ok(self.ytdlp.clone())
        } else {
            Err(format!("unexpected binary: {binary}"))
        }
    }
}

/// テスト一時ディレクトリ (Drop で再帰削除)。
struct TmpDir {
    path: PathBuf,
}

impl TmpDir {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "revmagic-ytdlp-test-{tag}-{}-{}",
            std::process::id(),
            nano()
        ));
        fs::create_dir_all(&base).unwrap();
        Self { path: base }
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn nano() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// fake yt-dlp shell script を作る。
///
/// この script は:
/// - 全 argv を `$ARGV_OUT` に 1 行 1 引数で書き出す。
/// - `--cookies <file>` があれば、その中身を `$COOKIE_COPY` に、stat の 8 進
///   パーミッションを `$COOKIE_MODE` に書き出す。
/// - `--output <dir>/%(title)s.%(ext)s` の dir に `mocktitle.mp3` を作る。
fn write_fake_ytdlp(
    dir: &Path,
    argv_out: &Path,
    cookie_copy: &Path,
    cookie_mode: &Path,
) -> PathBuf {
    let script_path = dir.join("yt-dlp");
    let script = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
: > "{argv_out}"
cookie_file=""
output_template=""
prev=""
for arg in "$@"; do
  printf '%s\n' "$arg" >> "{argv_out}"
  if [[ "$prev" == "--cookies" ]]; then
    cookie_file="$arg"
  fi
  if [[ "$prev" == "--output" ]]; then
    output_template="$arg"
  fi
  prev="$arg"
done

if [[ -n "$cookie_file" ]]; then
  cp "$cookie_file" "{cookie_copy}"
  # macOS / BSD stat -f, Linux stat -c の両対応。
  if stat -f '%Lp' "$cookie_file" > "{cookie_mode}" 2>/dev/null; then
    :
  else
    stat -c '%a' "$cookie_file" > "{cookie_mode}"
  fi
fi

# output template のディレクトリに mp3 を作る。
if [[ -n "$output_template" ]]; then
  out_dir="$(dirname "$output_template")"
  : > "$out_dir/mocktitle.mp3"
fi
exit 0
"#,
        argv_out = argv_out.display(),
        cookie_copy = cookie_copy.display(),
        cookie_mode = cookie_mode.display(),
    );
    fs::write(&script_path, script).unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();
    script_path
}

fn noop_progress() -> impl Fn(&Progress) -> Result<(), String> + Send + Sync {
    |_p: &Progress| Ok(())
}

#[test]
fn extract_audio_builds_correct_argv_and_handles_cookie_file() {
    let tmp = TmpDir::new("argv");
    let bin_dir = tmp.path.join("bin");
    let out_dir = tmp.path.join("out");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let argv_out = tmp.path.join("argv.txt");
    let cookie_copy = tmp.path.join("cookie-copy.txt");
    let cookie_mode = tmp.path.join("cookie-mode.txt");

    let fake = write_fake_ytdlp(&bin_dir, &argv_out, &cookie_copy, &cookie_mode);
    let resolver = FixedResolver { ytdlp: fake };

    let progress = noop_progress();
    let options = YtDlpOptions {
        optimize_for_size: false,
        user_agent: None,
    };

    let result = extract_audio_via_ytdlp(
        &resolver,
        "https://www.loom.com/share/abc123",
        &out_dir,
        Some("sid=secret-value; pref=1"),
        Some("https://utage-system.com/"),
        &options,
        &progress,
    );

    let mp3 = result.expect("extract should succeed with fake yt-dlp");
    assert!(mp3.ends_with("mocktitle.mp3"), "resolved mp3 path: {mp3}");
    assert!(Path::new(&mp3).exists());

    // --- argv 検証 ---
    let argv: Vec<String> = fs::read_to_string(&argv_out)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();

    assert!(has_pair(&argv, "--referer", "https://utage-system.com/"));
    assert!(argv.iter().any(|a| a == "--user-agent"));
    assert!(argv.iter().any(|a| a == "--extract-audio"));
    assert!(has_pair(&argv, "--audio-format", "mp3"));
    assert!(has_pair(&argv, "--audio-quality", "2"));
    assert!(argv.iter().any(|a| a == "--no-playlist"));
    assert!(argv.iter().any(|a| a == "--newline"));
    // Slice A: 音声優先フォーマット選択と断片並列 DL が argv に載ること。
    assert!(has_pair(&argv, "-f", "bestaudio/best"));
    assert!(has_pair(&argv, "--concurrent-fragments", "4"));
    // A13: --output テンプレートは空タイトルで id へフォールバックし、title 部を
    // 200 byte に制限する `%(title,id).200B.%(ext)s`。
    // A4: テンプレートは out_dir 直下ではなく out_dir 配下の job folder
    // (`download-mp3-<ts>/`) を指す (生 MP3 を出力 dir 直下に散らかさない)。
    let output_idx = argv
        .iter()
        .position(|a| a == "--output")
        .expect("--output present");
    let template = &argv[output_idx + 1];
    assert!(
        template.ends_with("%(title,id).200B.%(ext)s"),
        "template: {template}"
    );
    assert!(
        template.starts_with(out_dir.to_str().unwrap()),
        "template under out_dir: {template}"
    );
    let template_parent = Path::new(template).parent().unwrap();
    assert_eq!(
        template_parent.parent(),
        Some(out_dir.as_path()),
        "job folder must be directly under out_dir: {template}"
    );
    assert!(
        template_parent
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("download-mp3-"),
        "job folder name must follow <stem>-mp3-<ts>: {template}"
    );
    // A13: --restrict-filenames も argv に載る。
    assert!(
        argv.iter().any(|a| a == "--restrict-filenames"),
        "--restrict-filenames must be present"
    );
    // URL は最後。
    assert_eq!(
        argv.last().map(String::as_str),
        Some("https://www.loom.com/share/abc123")
    );

    // --- cookie 値が argv に生で載らないこと ---
    assert!(
        !argv.iter().any(|a| a.contains("secret-value")),
        "raw cookie value must NOT appear in argv"
    );

    // --- cookie file の中身 (Netscape) とパーミッション (0600) ---
    let cookie_path_in_argv = {
        let idx = argv
            .iter()
            .position(|a| a == "--cookies")
            .expect("--cookies present");
        PathBuf::from(&argv[idx + 1])
    };
    let netscape = fs::read_to_string(&cookie_copy).expect("fake captured cookie file");
    assert!(netscape.starts_with("# Netscape HTTP Cookie File"));
    assert!(netscape.contains("\tsid\tsecret-value\n"));
    assert!(netscape.contains("\tpref\t1\n"));

    let mode = fs::read_to_string(&cookie_mode).unwrap().trim().to_string();
    // 末尾 3 桁が 600 (先頭に余分な桁が付く環境差を吸収)。
    assert!(
        mode.ends_with("600"),
        "cookie file mode should be 0600, got {mode}"
    );

    // --- 処理完了後に cookie 一時ファイルが削除されていること ---
    assert!(
        !cookie_path_in_argv.exists(),
        "temp cookie file must be deleted after run: {}",
        cookie_path_in_argv.display()
    );
}

#[test]
fn extract_audio_optimize_uses_quality_3() {
    let tmp = TmpDir::new("q3");
    let bin_dir = tmp.path.join("bin");
    let out_dir = tmp.path.join("out");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();
    let argv_out = tmp.path.join("argv.txt");
    let cookie_copy = tmp.path.join("c.txt");
    let cookie_mode = tmp.path.join("m.txt");
    let fake = write_fake_ytdlp(&bin_dir, &argv_out, &cookie_copy, &cookie_mode);
    let resolver = FixedResolver { ytdlp: fake };
    let progress = noop_progress();
    let options = YtDlpOptions {
        optimize_for_size: true,
        user_agent: None,
    };

    extract_audio_via_ytdlp(
        &resolver,
        "https://www.loom.com/share/zzz",
        &out_dir,
        None, // cookie 無し -> --cookies フラグ自体が出ないこと
        None,
        &options,
        &progress,
    )
    .expect("should succeed");

    let argv: Vec<String> = fs::read_to_string(&argv_out)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert!(has_pair(&argv, "--audio-quality", "3"));
    assert!(
        !argv.iter().any(|a| a == "--cookies"),
        "no --cookies when cookie absent"
    );
    // cookie 無しなので fake は cookie ファイルを captureしない。
    assert!(!cookie_copy.exists());
}

#[test]
fn routing_direct_vs_extractor() {
    // .mp4 / .m3u8 は yt-dlp を呼ばず直経路。
    assert_eq!(
        route_for_url(&Url::parse("https://cdn.example.com/video/clip.mp4").unwrap()),
        MediaRoute::Direct
    );
    assert_eq!(
        route_for_url(&Url::parse("https://cdn.example.com/hls/master.m3u8?token=1").unwrap()),
        MediaRoute::Direct
    );
    // Loom 風 / ページ URL は yt-dlp 経路。
    assert_eq!(
        route_for_url(&Url::parse("https://www.loom.com/share/abcdef0123456789").unwrap()),
        MediaRoute::Extractor
    );
    assert_eq!(
        route_for_url(&Url::parse("https://utage-system.com/r/lesson006").unwrap()),
        MediaRoute::Extractor
    );
}

fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2).any(|w| w[0] == flag && w[1] == value)
}

/// F1: cookie file パスを stderr に出して **失敗** する fake yt-dlp。
///
/// yt-dlp が `--cookies <temp-path>` のパスを stderr へ漏らす実態を模す。
fn write_fake_ytdlp_leaks_cookie_path(dir: &Path) -> PathBuf {
    let script_path = dir.join("yt-dlp");
    let script = r#"#!/usr/bin/env bash
set -uo pipefail
cookie_file=""
prev=""
for arg in "$@"; do
  if [[ "$prev" == "--cookies" ]]; then
    cookie_file="$arg"
  fi
  prev="$arg"
done
# yt-dlp が cookie file の絶対パスを stderr へ漏らすケースを再現。
echo "WARNING: Loading cookies from $cookie_file" 1>&2
echo "ERROR: unable to parse cookie file $cookie_file: bad row" 1>&2
exit 1
"#;
    fs::write(&script_path, script).unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();
    script_path
}

#[test]
fn error_message_never_leaks_cookie_file_path() {
    let tmp = TmpDir::new("leak");
    let bin_dir = tmp.path.join("bin");
    let out_dir = tmp.path.join("out");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let fake = write_fake_ytdlp_leaks_cookie_path(&bin_dir);
    let resolver = FixedResolver { ytdlp: fake };
    let progress = noop_progress();
    let options = YtDlpOptions {
        optimize_for_size: false,
        user_agent: None,
    };

    let result = extract_audio_via_ytdlp(
        &resolver,
        "https://www.loom.com/share/leak123",
        &out_dir,
        Some("sid=secret-value; pref=1"),
        Some("https://utage-system.com/"),
        &options,
        &progress,
    );

    let err = result.expect_err("fake yt-dlp exits non-zero");
    let message = err.to_string();

    // cookie 一時ファイルは revmagic-cookies/cookies-<ts>.txt の形。
    // ディレクトリ名・ファイル名・stem いずれも error message に出てはならない。
    assert!(
        !message.contains("revmagic-cookies"),
        "cookie temp dir name leaked into error message: {message}"
    );
    assert!(
        !message.contains("cookies-"),
        "cookie temp file name/stem leaked into error message: {message}"
    );
    assert!(
        !message.contains(".txt"),
        "cookie temp file name leaked into error message: {message}"
    );
    // raw cookie 値も当然出ない。
    assert!(
        !message.contains("secret-value"),
        "raw cookie value leaked: {message}"
    );
    // redaction marker が入っていること (パスが添えられていた箇所)。
    assert!(
        message.contains("<redacted>"),
        "expected redaction marker: {message}"
    );
}

#[test]
fn cookie_temp_dir_is_0700() {
    // F3: cookie 一時ディレクトリ (revmagic-cookies) が 0700 であること。
    let tmp = TmpDir::new("perm");
    let bin_dir = tmp.path.join("bin");
    let out_dir = tmp.path.join("out");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let argv_out = tmp.path.join("argv.txt");
    let cookie_copy = tmp.path.join("c.txt");
    let cookie_mode = tmp.path.join("m.txt");
    // cookie file が置かれた親ディレクトリの mode を書き出す fake。
    let dir_mode = tmp.path.join("dirmode.txt");
    let script_path = bin_dir.join("yt-dlp");
    let script = format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
: > "{argv_out}"
cookie_file=""
output_template=""
prev=""
for arg in "$@"; do
  printf '%s\n' "$arg" >> "{argv_out}"
  if [[ "$prev" == "--cookies" ]]; then cookie_file="$arg"; fi
  if [[ "$prev" == "--output" ]]; then output_template="$arg"; fi
  prev="$arg"
done
if [[ -n "$cookie_file" ]]; then
  cp "$cookie_file" "{cookie_copy}"
  if stat -f '%Lp' "$cookie_file" > "{cookie_mode}" 2>/dev/null; then :; else stat -c '%a' "$cookie_file" > "{cookie_mode}"; fi
  dir="$(dirname "$cookie_file")"
  if stat -f '%Lp' "$dir" > "{dir_mode}" 2>/dev/null; then :; else stat -c '%a' "$dir" > "{dir_mode}"; fi
fi
if [[ -n "$output_template" ]]; then
  out_dir="$(dirname "$output_template")"
  : > "$out_dir/mocktitle.mp3"
fi
exit 0
"#,
        argv_out = argv_out.display(),
        cookie_copy = cookie_copy.display(),
        cookie_mode = cookie_mode.display(),
        dir_mode = dir_mode.display(),
    );
    fs::write(&script_path, script).unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();

    let resolver = FixedResolver { ytdlp: script_path };
    let progress = noop_progress();
    let options = YtDlpOptions {
        optimize_for_size: false,
        user_agent: None,
    };

    extract_audio_via_ytdlp(
        &resolver,
        "https://www.loom.com/share/perm",
        &out_dir,
        Some("sid=v"),
        None,
        &options,
        &progress,
    )
    .expect("should succeed");

    let mode = fs::read_to_string(&dir_mode).unwrap().trim().to_string();
    assert!(
        mode.ends_with("700"),
        "cookie temp dir should be 0700, got {mode}"
    );
}
