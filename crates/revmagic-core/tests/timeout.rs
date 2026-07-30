//! core 層 内部タイムアウト (deadline) のテスト (Slice B, 実ネットワーク不要)。
//!
//! 検証点:
//! - yt-dlp プロセスが終了しない (無限ブロック) 場合でも、極小 deadline を
//!   渡すと deadline 超過でエラー化し、呼び出しが応答不能にならないこと。
//! - deadline 超過エラーの文言に secret / cookie 値 / URL クエリトークン /
//!   cookie 一時ファイルパスが一切含まれないこと (既存 redaction 方針)。
//! - deadline 超過時にハングした子プロセスが kill され、呼び出しが概ね
//!   deadline + 回収猶予の範囲で戻ること (無限ブロックしない決定的検証)。
//! - 正常終了するプロセスは deadline 内なら従来どおり成功すること (回帰防止)。

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use revmagic_core::{
    extract_audio_via_ytdlp_with_deadline, BinaryResolver, Progress, YtDlpErrorKind, YtDlpOptions,
};

/// 解決先を fake yt-dlp の実体パスに固定する resolver。
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
            "revmagic-timeout-test-{tag}-{}-{}",
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

/// 永久にブロックする fake yt-dlp。
///
/// stdout / stderr に何も出さず `sleep` し続けるため、deadline ポーリングが
/// 効かない実装だと `child.wait()` で無限ブロックする。deadline が効いていれば
/// kill されて短時間で戻る。`sleep 600` は「実質無限」を表す (テスト本体は
/// 極小 deadline で先に抜ける)。
fn write_hanging_ytdlp(dir: &Path) -> PathBuf {
    let script_path = dir.join("yt-dlp");
    let script = r#"#!/usr/bin/env bash
# 何も出力せず眠り続ける (無限ブロック相当)。
sleep 600
exit 0
"#;
    fs::write(&script_path, script).unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();
    script_path
}

/// 子孫プロセスが stdout/stderr の FD を握ったまま残る fake yt-dlp (指摘1(c))。
///
/// この親スクリプト (= yt-dlp 役) は、stdout/stderr を **リダイレクトせずに**
/// 長時間 sleep する子プロセスを background 起動する。子は親 (= グループリーダー)
/// の pipe FD を継承し、それを握ったまま生き続ける。
///
/// この状況では、直下プロセス (親) **だけ** を kill しても、子が FD を握り続けて
/// pipe が EOF にならず、reader thread の素の `join()` が再ハングする。実装が
/// (a) プロセスグループごと kill していれば子も殺され FD が解放され、(b) reader の
/// join が bounded なら、いずれにせよ有限時間で確実に戻る。これを決定的に検証する。
fn write_descendant_holding_ytdlp(dir: &Path) -> PathBuf {
    let script_path = dir.join("yt-dlp");
    // 子 (sleep) は stdout/stderr を継承したまま background 化する。親はその子の
    // pid を wait して block する (出力は一切しない)。group kill されなければ
    // 親 kill 後も子が FD を握り続ける。
    let script = r#"#!/usr/bin/env bash
# stdout/stderr を握ったまま残る子孫を起動する (リダイレクトしない)。
sleep 600 &
child=$!
# 親もブロックして待つ (無出力)。group kill でないと子は生き残る。
wait "$child"
exit 0
"#;
    fs::write(&script_path, script).unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();
    script_path
}

/// 出力テンプレートのディレクトリに mp3 を作って即終了する fake yt-dlp
/// (正常系の回帰確認用)。
fn write_quick_ok_ytdlp(dir: &Path) -> PathBuf {
    let script_path = dir.join("yt-dlp");
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
output_template=""
prev=""
for arg in "$@"; do
  if [[ "$prev" == "--output" ]]; then
    output_template="$arg"
  fi
  prev="$arg"
done
if [[ -n "$output_template" ]]; then
  out_dir="$(dirname "$output_template")"
  : > "$out_dir/mocktitle.mp3"
fi
exit 0
"#;
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
fn hanging_ytdlp_times_out_with_redacted_error() {
    let tmp = TmpDir::new("hang");
    let bin_dir = tmp.path.join("bin");
    let out_dir = tmp.path.join("out");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let fake = write_hanging_ytdlp(&bin_dir);
    let resolver = FixedResolver { ytdlp: fake };
    let progress = noop_progress();
    let options = YtDlpOptions {
        optimize_for_size: false,
        user_agent: None,
    };

    // 極小 deadline (200ms)。秘密含みの cookie / クエリ付き URL を渡す。
    let started = Instant::now();
    let result = extract_audio_via_ytdlp_with_deadline(
        &resolver,
        "https://www.loom.com/share/abc123?token=SECRETSIG&sig=DEADBEEF",
        &out_dir,
        Some("sid=SECRET-COOKIE-VALUE; pref=1"),
        Some("https://utage-system.com/"),
        &options,
        &progress,
        Duration::from_millis(200),
    );
    let elapsed = started.elapsed();

    // deadline 超過でエラーになること。
    let err = result.expect_err("hanging yt-dlp must time out, not block forever");
    assert_eq!(
        err.kind(),
        &YtDlpErrorKind::Other,
        "timeout should be classified as Other"
    );

    // --- 無限ブロックしない決定的検証: deadline + 回収猶予 (5s) + 余裕の範囲で戻る ---
    assert!(
        elapsed < Duration::from_secs(30),
        "call must return promptly after deadline, took {elapsed:?}"
    );

    // --- redaction: エラー文言に secret / cookie / token / URL 生値が載らない ---
    let message = err.to_string();
    assert!(
        !message.contains("SECRET-COOKIE-VALUE"),
        "cookie value leaked: {message}"
    );
    assert!(
        !message.contains("SECRETSIG"),
        "url query token leaked: {message}"
    );
    assert!(!message.contains("DEADBEEF"), "url sig leaked: {message}");
    assert!(
        !message.contains("token="),
        "url query key leaked: {message}"
    );
    // cookie 一時ファイルパス断片 (revmagic-cookies) も載らない。
    assert!(
        !message.contains("revmagic-cookies"),
        "cookie temp path leaked: {message}"
    );
    // 固定文言であること (実装の deadline メッセージ)。
    assert!(
        message.contains("制限時間"),
        "should be the fixed timeout message, got: {message}"
    );
}

#[test]
fn quick_ytdlp_succeeds_within_generous_deadline() {
    // 正常系の回帰確認: 即終了するプロセスは deadline 内なら従来どおり成功。
    let tmp = TmpDir::new("ok");
    let bin_dir = tmp.path.join("bin");
    let out_dir = tmp.path.join("out");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let fake = write_quick_ok_ytdlp(&bin_dir);
    let resolver = FixedResolver { ytdlp: fake };
    let progress = noop_progress();
    let options = YtDlpOptions {
        optimize_for_size: false,
        user_agent: None,
    };

    let result = extract_audio_via_ytdlp_with_deadline(
        &resolver,
        "https://www.loom.com/share/abc123",
        &out_dir,
        None,
        None,
        &options,
        &progress,
        Duration::from_secs(30),
    );

    let mp3 = result.expect("quick yt-dlp should succeed within deadline");
    assert!(mp3.ends_with("mocktitle.mp3"), "resolved mp3 path: {mp3}");
    assert!(Path::new(&mp3).exists());
}

#[test]
fn descendant_holding_stdio_times_out_in_bounded_time() {
    // 指摘1(c): yt-dlp の子孫 (ffmpeg 等相当) が stdout/stderr の FD を握ったまま
    // 残るケース。直下プロセスだけを kill + 無期限 join() する実装だと、子が FD を
    // 握り続けて pipe が EOF にならず再ハングする。group kill + bounded join なら
    // 有限時間で確実に戻る。
    let tmp = TmpDir::new("descendant");
    let bin_dir = tmp.path.join("bin");
    let out_dir = tmp.path.join("out");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let fake = write_descendant_holding_ytdlp(&bin_dir);
    let resolver = FixedResolver { ytdlp: fake };
    let progress = noop_progress();
    let options = YtDlpOptions {
        optimize_for_size: false,
        user_agent: None,
    };

    let started = Instant::now();
    let result = extract_audio_via_ytdlp_with_deadline(
        &resolver,
        "https://www.loom.com/share/abc123",
        &out_dir,
        None,
        None,
        &options,
        &progress,
        Duration::from_millis(200),
    );
    let elapsed = started.elapsed();

    let err = result.expect_err("descendant-holding yt-dlp must time out, not block forever");
    assert_eq!(
        err.kind(),
        &YtDlpErrorKind::Other,
        "timeout should be classified as Other"
    );

    // 決定的検証: deadline(0.2s) + kill 回収猶予(5s) + reader join 猶予(5s) + 余裕。
    // group kill が効いていなければここで 600 秒近くハングして失敗する。
    assert!(
        elapsed < Duration::from_secs(20),
        "call must return promptly even when a descendant holds stdio FDs, took {elapsed:?}"
    );
    assert!(
        err.to_string().contains("制限時間"),
        "fixed timeout message expected: {err}"
    );
}

#[test]
fn direct_download_error_redacts_url_query() {
    // 指摘2: direct download (reqwest) の接続エラーが対象 URL (query 含む) を
    // エラー文言に漏らさないこと。connection refused を即時に起こすため、誰も
    // listen していないループバックポートへ繋ぐ (実ネット非依存・即失敗)。
    // reqwest::Error の Display は対象 URL を埋め込むため、redact されていなければ
    // query の署名トークンが流出する。
    let tmp = TmpDir::new("redact");
    let bin_dir = tmp.path.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    // download_media は直リンク経路では yt-dlp を使わないが、resolver は trait 境界
    // を満たすため渡す。
    let fake = write_quick_ok_ytdlp(&bin_dir);
    let resolver = FixedResolver { ytdlp: fake };
    let progress = noop_progress();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // port 1 は通常 listen されておらず connection refused が即時に返る。
    // .mp4 拡張子で direct media 経路 (reqwest) に入れる。query に署名トークンを載せる。
    let url = "http://127.0.0.1:1/clip.mp4?token=SECRETSIG&sig=DEADBEEF";
    let result = rt.block_on(revmagic_core::download_media(
        &resolver,
        url.to_string(),
        Some("sid=SECRET-COOKIE-VALUE".to_string()),
        None,
        &progress,
    ));

    let err = result.expect_err("connection to closed port must fail");
    // query トークンが文言に残らないこと。
    assert!(!err.contains("SECRETSIG"), "url query token leaked: {err}");
    assert!(!err.contains("DEADBEEF"), "url sig leaked: {err}");
    assert!(!err.contains("token="), "url query key leaked: {err}");
    // cookie は reqwest エラーには元々載らないが念のため確認。
    assert!(!err.contains("SECRET-COOKIE-VALUE"), "cookie leaked: {err}");
}
