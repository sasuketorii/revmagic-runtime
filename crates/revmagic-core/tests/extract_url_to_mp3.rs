//! `extract_url_to_mp3` (Slice I-1) の core 層テスト (実ネットワーク不要)。
//!
//! 検証点:
//! - 非分割: fake yt-dlp が生成した MP3 から `ConversionSummary` を構築すること
//!   (file_count=1, files[0].path が生成 MP3, output_folder が出力 dir)。
//! - 分割: fake yt-dlp の MP3 を `segment_mp3_copy` 経路で扱うこと (split_minutes
//!   が Some の時に単一 MP3 経路へ落ちないこと) — ffmpeg 実体非依存にするため、
//!   分割は ffmpeg 解決失敗で `Other` エラーになることを確認する (経路選択の検証)。
//! - キャンセル (progress-Err 経路): 進捗コールバックが `Err` を返すと中断エラーに
//!   なり、ハングする yt-dlp child が kill されて呼び出しが有限時間で戻ること。
//!   さらに **child と孫 (`sleep 600 &`) が実際に消滅したこと** を PID ファイル +
//!   `kill -0` で決定的に検証する (指摘3: killpg がグループごと殺す保証)。
//! - キャンセル (cancel-token 経路): 無音 (出力ゼロ) の fake yt-dlp + 途中で立てる
//!   cancel トークンで、~数百 ms で kill されて戻ること + child/孫消滅を検証する
//!   (指摘2 の即応性 + 指摘3 のトークン経路 kill 検証)。
//! - ルーティング: `route_for_url` が直リンク / 要 extractor を正しく振り分けること。

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use revmagic_core::{
    extract_url_to_mp3, extract_url_to_mp3_with_cancel, route_for_url,
    segment_mp3_copy_with_cancel, BinaryResolver, MediaRoute, Progress, YtDlpErrorKind,
};
use url::Url;

/// yt-dlp は固定 fake、それ以外 (ffmpeg/ffprobe) は解決失敗にする resolver。
struct YtDlpOnlyResolver {
    ytdlp: PathBuf,
}

impl BinaryResolver for YtDlpOnlyResolver {
    fn resolve(&self, binary: &str) -> Result<PathBuf, String> {
        if binary == "yt-dlp" {
            Ok(self.ytdlp.clone())
        } else {
            // ffmpeg / ffprobe は意図的に解決しない (分割経路に入ったことを Other
            // エラーで観測するため)。
            Err(format!("not provided in test: {binary}"))
        }
    }
}

struct TmpDir {
    path: PathBuf,
}

impl TmpDir {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "revmagic-extract-url-test-{tag}-{}-{}",
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

/// 出力テンプレートのディレクトリに mp3 を作って即終了する fake yt-dlp。
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

/// 1 行だけ進捗を出してからハングする fake yt-dlp (progress-Err 経路の検証用)。
///
/// 指摘3: 自分 (スクリプト) の PID と、stdout FD を握る孫 `sleep 600 &` の PID を
/// `<pid_dir>/self.pid` / `<pid_dir>/child.pid` に書き出す。孫は stdout を継承した
/// まま走るので、直下プロセスだけ kill しても孫が FD を握り続ければ reader が EOF に
/// 到達しない。`killpg` でグループごと殺せていれば両 PID とも消滅する。
fn write_progress_then_hang_ytdlp(dir: &Path, pid_dir: &Path) -> PathBuf {
    let script_path = dir.join("yt-dlp");
    let pid_dir = pid_dir.to_string_lossy();
    // 孫 `sleep 600 &` は stdout を継承する (FD を握る)。両 PID を記録してから、
    // スクリプト本体も待機し続ける (実装が kill しなければ長時間ブロックする)。
    let script = format!(
        r#"#!/usr/bin/env bash
echo $$ > "{pid_dir}/self.pid"
sleep 600 &
child=$!
echo "$child" > "{pid_dir}/child.pid"
echo "[download]   1.0% of 10.00MiB at 1.00MiB/s ETA 00:09"
wait "$child"
exit 0
"#
    );
    fs::write(&script_path, script).unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();
    script_path
}

/// 完全に無音 (進捗を一切出さない) でハングする fake yt-dlp (cancel-token 経路の
/// 即応性検証用)。
///
/// 指摘2: 進捗を 1 行も出さないため、progress-Err 経路ではキャンセルできない
/// (progress コールバックが一度も呼ばれない)。cancel トークンを recv_timeout の
/// 各周回 (100ms) で確認する実装でのみ ~数百 ms で停止できる。指摘3 同様に self/
/// child の PID を記録し、killpg でグループごと消えることを検証する。
fn write_silent_hang_ytdlp(dir: &Path, pid_dir: &Path) -> PathBuf {
    let script_path = dir.join("yt-dlp");
    let pid_dir = pid_dir.to_string_lossy();
    let script = format!(
        r#"#!/usr/bin/env bash
echo $$ > "{pid_dir}/self.pid"
sleep 600 &
child=$!
echo "$child" > "{pid_dir}/child.pid"
wait "$child"
exit 0
"#
    );
    fs::write(&script_path, script).unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();
    script_path
}

/// PID ファイルから PID を読む。最大 `timeout` 待って出現を待つ
/// (fake が PID を書く前に呼ばれる競合を避ける)。
fn read_pid(pid_file: &Path, timeout: Duration) -> i32 {
    let started = Instant::now();
    loop {
        if let Ok(text) = fs::read_to_string(pid_file) {
            if let Ok(pid) = text.trim().parse::<i32>() {
                return pid;
            }
        }
        if started.elapsed() >= timeout {
            panic!(
                "PID file {} not populated within {timeout:?}",
                pid_file.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// PID が生存しているか (`kill(pid, 0)` 相当)。
///
/// `kill -0` は signal を送らず存在確認だけ行う。`Ok` = 生存、`Err(ESRCH)` = 不在。
/// zombie (回収待ち) も「生存」と判定され得るが、本テストは PID を別プロセスとして
/// fork しており、core 側が reap するため zombie は残らない。
fn pid_alive(pid: i32) -> bool {
    // SAFETY: kill は副作用 (signal 0 = 送信なし) のみ。
    unsafe { libc::kill(pid, 0) == 0 }
}

/// PID が `timeout` 以内に消滅することを待つ。消滅したら true。
fn wait_pid_gone(pid: i32, timeout: Duration) -> bool {
    let started = Instant::now();
    while pid_alive(pid) {
        if started.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

fn noop_progress() -> impl Fn(&Progress) -> Result<(), String> + Send + Sync {
    |_p: &Progress| Ok(())
}

#[test]
fn route_for_url_classifies_direct_and_extractor() {
    // 直リンク (拡張子あり) は Direct。
    assert_eq!(
        route_for_url(&Url::parse("https://cdn.example.com/clip.mp4").unwrap()),
        MediaRoute::Direct
    );
    assert_eq!(
        route_for_url(&Url::parse("https://cdn.example.com/audio.m4a").unwrap()),
        MediaRoute::Direct
    );
    // ページ / 埋め込み URL は Extractor。
    assert_eq!(
        route_for_url(&Url::parse("https://www.youtube.com/watch?v=abc").unwrap()),
        MediaRoute::Extractor
    );
    assert_eq!(
        route_for_url(&Url::parse("https://www.loom.com/share/xyz").unwrap()),
        MediaRoute::Extractor
    );
}

#[test]
fn extract_url_to_mp3_builds_summary_without_split() {
    let tmp = TmpDir::new("nosplit");
    let bin_dir = tmp.path.join("bin");
    let out_dir = tmp.path.join("out");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let fake = write_quick_ok_ytdlp(&bin_dir);
    let resolver = YtDlpOnlyResolver { ytdlp: fake };
    let progress = noop_progress();

    let summary = extract_url_to_mp3(
        &resolver,
        "https://www.loom.com/share/abc",
        &out_dir,
        None,
        false,
        None,
        None,
        &progress,
    )
    .expect("extract should succeed");

    assert_eq!(summary.file_count, 1);
    assert_eq!(summary.files.len(), 1);
    assert!(
        summary.files[0].path.ends_with("mocktitle.mp3"),
        "path={}",
        summary.files[0].path
    );
    // A4: 出力は出力 dir 直下ではなく、出力 dir 配下の job folder
    // (`download-mp3-<ts>/`) に隔離される。output_folder はその job folder を指す。
    let job_folder = PathBuf::from(&summary.output_folder);
    assert_eq!(
        job_folder.parent().map(Path::to_path_buf),
        Some(out_dir.clone()),
        "job folder must live directly under the output dir"
    );
    // job folder は生成 MP3 のファイル名 (ここでは fake yt-dlp が出す "mocktitle")
    // ベースにリネームされる (タイトルベース命名)。
    let job_name = job_folder.file_name().unwrap().to_string_lossy();
    assert!(
        job_name.starts_with("mocktitle-mp3-"),
        "job folder name must follow <stem>-mp3-<ts>: {job_name}"
    );
    // 生成 MP3 は job folder 内にあり、出力 dir 直下には無い (A4: 生 MP3 散らかり解消)。
    assert!(
        summary.files[0].path.starts_with(&summary.output_folder),
        "mp3 must be inside the job folder: {}",
        summary.files[0].path
    );
    let stray_mp3 = fs::read_dir(&out_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("mp3"));
    assert!(
        !stray_mp3,
        "no raw mp3 must be left directly in the output dir"
    );
}

#[test]
fn extract_url_to_mp3_split_takes_segment_path() {
    // split_minutes=Some の場合は segment_mp3_copy 経路へ入る。テストでは ffmpeg/
    // ffprobe を解決しないため、その経路は Other エラーになる (= 単一 MP3 経路では
    // なく分割経路へ分岐したことの観測)。
    let tmp = TmpDir::new("split");
    let bin_dir = tmp.path.join("bin");
    let out_dir = tmp.path.join("out");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();

    let fake = write_quick_ok_ytdlp(&bin_dir);
    let resolver = YtDlpOnlyResolver { ytdlp: fake };
    let progress = noop_progress();

    let result = extract_url_to_mp3(
        &resolver,
        "https://www.loom.com/share/abc",
        &out_dir,
        Some(10),
        false,
        None,
        None,
        &progress,
    );

    let err = result.expect_err("split path should fail (ffmpeg/ffprobe not provided)");
    assert_eq!(err.kind(), &YtDlpErrorKind::Other);
}

#[test]
fn cancel_via_progress_err_aborts_and_kills_child_and_grandchild() {
    // 指摘3: 進捗コールバックが (キャンセルにより) Err を返すと、ハングする yt-dlp
    // child **と孫 (`sleep 600 &`)** が killpg でグループごと kill され、呼び出しが
    // 有限時間 (deadline よりはるか手前) で戻ること。child を drop しただけで孫が
    // 残る実装ではこのテストは失敗する (孫 PID が消えない)。
    let tmp = TmpDir::new("cancel-progress");
    let bin_dir = tmp.path.join("bin");
    let out_dir = tmp.path.join("out");
    let pid_dir = tmp.path.join("pids");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();
    fs::create_dir_all(&pid_dir).unwrap();

    let fake = write_progress_then_hang_ytdlp(&bin_dir, &pid_dir);
    let resolver = YtDlpOnlyResolver { ytdlp: fake };

    // 「yt-dlp の DL 進捗が届いた = yt-dlp が起動し PID を書き終えた」時点で初めて
    // Err を返す。準備フェーズ ("解決中" percent:0) は yt-dlp 起動前に発火するので
    // ここで Err を返すと yt-dlp が起動せず PID が書かれない (レース)。fake script は
    // PID 書き込み → DL 進捗出力の順なので、"ダウンロード中" 通知が来た時点で PID は
    // 必ず書かれている。これにより progress-Err 経路で child/孫を確実に観測できる。
    let progress = |p: &Progress| -> Result<(), String> {
        if p.step == "ダウンロード中" {
            Err("キャンセルされました".to_string())
        } else {
            Ok(())
        }
    };

    let started = Instant::now();
    let result = extract_url_to_mp3(
        &resolver,
        "https://www.loom.com/share/abc",
        &out_dir,
        None,
        false,
        None,
        None,
        &progress,
    );
    let elapsed = started.elapsed();

    let err = result.expect_err("cancel should abort");
    assert_eq!(
        err.kind(),
        &YtDlpErrorKind::Other,
        "cancel maps to Other kind"
    );
    // sleep 600 を待たず、kill + 回収猶予の範囲で戻ること (無限ブロックしない検証)。
    assert!(
        elapsed < Duration::from_secs(30),
        "cancel must return promptly (killed child), took {elapsed:?}"
    );

    // 指摘3 の核: child と孫が **実際に消滅** したことを PID 単位で決定的に検証。
    // 関数が戻った時点で core は killpg + bounded reap 済み。孫 (`sleep 600`) は
    // グループ kill されていなければ最大 600 秒生き残る。短い猶予で消滅を確認する。
    let self_pid = read_pid(&pid_dir.join("self.pid"), Duration::from_secs(5));
    let child_pid = read_pid(&pid_dir.join("child.pid"), Duration::from_secs(5));
    assert!(
        wait_pid_gone(self_pid, Duration::from_secs(10)),
        "yt-dlp self pid {self_pid} must be killed (killpg)"
    );
    assert!(
        wait_pid_gone(child_pid, Duration::from_secs(10)),
        "grandchild sleep pid {child_pid} must be killed by killpg (orphan check)"
    );
}

#[test]
fn cancel_via_token_aborts_silent_hang_and_kills_child_and_grandchild() {
    // 指摘2 + 指摘3: **無音 (進捗ゼロ)** の yt-dlp に対し、cancel トークンを最初から
    // 立てておくと、progress コールバックが一度も呼ばれなくても recv_timeout の各
    // 周回 (100ms) でトークンが確認され、~数百 ms で killpg されて戻ること。さらに
    // child/孫が消滅することを検証する (トークン経路の kill 規律)。
    let tmp = TmpDir::new("cancel-token");
    let bin_dir = tmp.path.join("bin");
    let out_dir = tmp.path.join("out");
    let pid_dir = tmp.path.join("pids");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();
    fs::create_dir_all(&pid_dir).unwrap();

    let fake = write_silent_hang_ytdlp(&bin_dir, &pid_dir);
    let resolver = YtDlpOnlyResolver { ytdlp: fake };

    // 無音なので progress は一切呼ばれない (常に Ok)。中断はトークン経路のみが担う。
    let progress = noop_progress();
    // トークンは最初 false。別スレッドが「fake が PID ファイルを書き終えた (= 起動
    // 済み)」のを観測してから true を立てる。これにより (a) PID 書き込み完了を保証
    // しつつ (レース回避)、(b) トークンを立てた瞬間から core が poll 粒度 (100ms) で
    // 中断に反応することを検証できる。
    let cancel = AtomicBool::new(false);
    let self_pid_file = pid_dir.join("self.pid");
    let child_pid_file = pid_dir.join("child.pid");

    std::thread::scope(|scope| {
        let setter = scope.spawn(|| {
            // PID ファイルが両方そろう (= fake 起動 + 孫 spawn 完了) まで待ってから立てる。
            let _ = read_pid(&self_pid_file, Duration::from_secs(10));
            let _ = read_pid(&child_pid_file, Duration::from_secs(10));
            cancel.store(true, Ordering::SeqCst);
        });

        let token_set_at = Instant::now();
        let result = extract_url_to_mp3_with_cancel(
            &resolver,
            "https://www.loom.com/share/abc",
            &out_dir,
            None,
            false,
            None,
            None,
            &progress,
            Some(&cancel),
        );
        let elapsed = token_set_at.elapsed();
        setter.join().unwrap();

        let err = result.expect_err("cancel token should abort silent hang");
        assert_eq!(
            err.kind(),
            &YtDlpErrorKind::Other,
            "cancel maps to Other kind"
        );
        // 即応性: deadline (30 分) / sleep 600 を待たず、poll 粒度 (100ms) + kill/reap
        // 猶予の範囲で戻ること。無音 (進捗ゼロ) でもトークン経路で停止する決定的検証。
        assert!(
            elapsed < Duration::from_secs(10),
            "token cancel must stop a silent hang promptly, took {elapsed:?}"
        );
    });

    // child/孫が消滅したこと (トークン経路でも killpg が効くこと)。
    let self_pid = read_pid(&pid_dir.join("self.pid"), Duration::from_secs(5));
    let child_pid = read_pid(&pid_dir.join("child.pid"), Duration::from_secs(5));
    assert!(
        wait_pid_gone(self_pid, Duration::from_secs(10)),
        "yt-dlp self pid {self_pid} must be killed via cancel token"
    );
    assert!(
        wait_pid_gone(child_pid, Duration::from_secs(10)),
        "grandchild sleep pid {child_pid} must be killed by killpg via cancel token"
    );
}

// ---------------------------------------------------------------------------
// 指摘1: ffmpeg child のキャンセル kill 規律 (segment_mp3_copy_with_cancel 経由)
// ---------------------------------------------------------------------------

/// ffmpeg / ffprobe を fake にマップする resolver (ffmpeg cancel 検証用)。
struct FakeFfmpegResolver {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

impl BinaryResolver for FakeFfmpegResolver {
    fn resolve(&self, binary: &str) -> Result<PathBuf, String> {
        match binary {
            "ffmpeg" => Ok(self.ffmpeg.clone()),
            "ffprobe" => Ok(self.ffprobe.clone()),
            other => Err(format!("not provided in test: {other}")),
        }
    }
}

/// 固定 duration を返す fake ffprobe (read_duration_seconds を満たす)。
fn write_fake_ffprobe(dir: &Path) -> PathBuf {
    let path = dir.join("ffprobe");
    // read_duration_seconds は stdout の trim を f64 パースするだけ。
    let script = "#!/usr/bin/env bash\necho 120.0\nexit 0\n";
    fs::write(&path, script).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

/// 1 行進捗を出してからハングする fake ffmpeg (指摘1: child kill 検証)。
///
/// `-progress pipe:1` 経路を模し、`progress=continue` を 1 回出して core の
/// progress コールバックを発火させ、その後は孫 `sleep 600 &` (stdout FD を握る)
/// と共にハングする。core が ffmpeg child を **プロセスグループごと** kill しない
/// 限り、孫が生き残り reader も EOF に到達しない。self/child の PID を記録する。
fn write_progress_then_hang_ffmpeg(dir: &Path, pid_dir: &Path) -> PathBuf {
    let path = dir.join("ffmpeg");
    let pid_dir = pid_dir.to_string_lossy();
    let script = format!(
        r#"#!/usr/bin/env bash
echo $$ > "{pid_dir}/self.pid"
sleep 600 &
child=$!
echo "$child" > "{pid_dir}/child.pid"
# duration=120s (fake ffprobe)。out_time=60s で percent=50 (> 5) になり、
# テストの progress コールバックが cancel トークンを立てる契機になる。
echo "out_time_us=60000000"
echo "progress=continue"
wait "$child"
exit 0
"#
    );
    fs::write(&path, script).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

#[test]
fn ffmpeg_cancel_via_token_kills_child_and_grandchild() {
    // 指摘1 + 指摘2: ffmpeg 実行中に cancel トークンが立っていると、行読み (poll)
    // ループの各反復でトークンが確認され、ffmpeg child **と孫** が killpg で
    // グループごと kill され、呼び出しが有限時間で戻ること。従来 (kill/wait 無し) では
    // child を drop するだけで孫 `sleep 600` が orphan 化し、このテストは孫 PID が
    // 残って失敗する。トークン経路を使うのは、ffmpeg 起動前の準備 progress (percent:0)
    // で誤って中断せず、確実に ffmpeg child を起動させてから kill 規律を検証するため。
    let tmp = TmpDir::new("ffmpeg-cancel");
    let bin_dir = tmp.path.join("bin");
    let out_dir = tmp.path.join("out");
    let pid_dir = tmp.path.join("pids");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&out_dir).unwrap();
    fs::create_dir_all(&pid_dir).unwrap();

    // segment_mp3_copy は input.exists() を要求するので空ファイルを置く。
    let input = tmp.path.join("input.mp3");
    fs::write(&input, b"x").unwrap();

    let resolver = FakeFfmpegResolver {
        ffmpeg: write_progress_then_hang_ffmpeg(&bin_dir, &pid_dir),
        ffprobe: write_fake_ffprobe(&bin_dir),
    };

    // トークン経路を検証する。ただし「ffmpeg が PID ファイルを書き終えてから」
    // 中断するよう、ffmpeg の最初の進捗 (percent > 5 = out_time_us 由来) を観測した
    // 時点でトークンを立てる。fake script は PID 書き込み → 進捗出力の順なので、
    // 進捗が届いた時点で PID は必ず書かれている (レース回避)。progress 自体は常に
    // Ok を返し、中断はトークン経路 (poll 各反復のチェック) のみが担う。
    let cancel = AtomicBool::new(false);
    let progress = |p: &Progress| -> Result<(), String> {
        if p.percent > 5 {
            cancel.store(true, Ordering::SeqCst);
        }
        Ok(())
    };

    let started = Instant::now();
    let result =
        segment_mp3_copy_with_cancel(&resolver, &input, &out_dir, 1, &progress, Some(&cancel));
    let elapsed = started.elapsed();

    assert!(result.is_err(), "cancel should abort ffmpeg conversion");
    assert!(
        elapsed < Duration::from_secs(15),
        "ffmpeg cancel must return promptly (killed child), took {elapsed:?}"
    );

    // child/孫が消滅したこと (ffmpeg も killpg でグループごと殺されること)。
    let self_pid = read_pid(&pid_dir.join("self.pid"), Duration::from_secs(5));
    let child_pid = read_pid(&pid_dir.join("child.pid"), Duration::from_secs(5));
    assert!(
        wait_pid_gone(self_pid, Duration::from_secs(10)),
        "ffmpeg self pid {self_pid} must be killed (killpg)"
    );
    assert!(
        wait_pid_gone(child_pid, Duration::from_secs(10)),
        "ffmpeg grandchild sleep pid {child_pid} must be killed by killpg (orphan check)"
    );
}
